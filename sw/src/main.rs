#![no_main]
#![no_std]

// note: to get vscode to reload file, do shift-ctrl-p, 'reload window'. developer:Reload window

extern crate betrusted_hal;
extern crate com_rs;
extern crate utralib;
extern crate volatile;
extern crate wfx_bindings;
extern crate wfx_rs;
extern crate wfx_sys;
extern crate xous_nommu;

use betrusted_hal::api_bq25618::BtCharger;
use betrusted_hal::api_gasgauge::{
    gg_full_capacity, gg_remaining_capacity, gg_set_design_capacity, gg_set_hibernate, gg_start,
    gg_state_of_charge,
};
use betrusted_hal::api_lm3509::BtBacklight;
use betrusted_hal::api_tusb320::BtUsbCc;
use betrusted_hal::hal_hardi2c::Hardi2c;
use betrusted_hal::hal_time::{delay_ms, get_time_ms, set_msleep_target_ticks, time_init};
use com_rs::ComState;
use core::panic::PanicInfo;
use gyro_rs::hal_gyro::BtGyro;
use riscv_rt::entry;
use utralib::generated::{
    utra, CSR, HW_COM_BASE, HW_CRG_BASE, HW_GIT_BASE, HW_POWER_BASE, HW_SPIFLASH_MEM,
    HW_TICKTIMER_BASE, HW_WIFI_BASE,
};
use volatile::Volatile;
use wfx_bindings::SL_STATUS_OK;
use wfx_rs::hal_wf200::{
    wf200_fw_build, wf200_fw_major, wf200_fw_minor, wf200_get_rx_stats_raw, wf200_mutex_get,
    wf200_send_pds, wf200_ssid_get_list, wf200_ssid_updated, wfx_handle_event, wfx_init,
    wfx_scan_ongoing, wfx_start_scan,
};

// Modules from this crate
#[macro_use]
mod debug;
mod power_mgmt;
mod spi;

use power_mgmt::charger_handler;
use spi::{spi_erase_region, spi_program_page, spi_standby};

// Constants
const CONFIG_CLOCK_FREQUENCY: u32 = 18_000_000;
// allocate a global, unsafe static string for debug output
#[used] // This is necessary to keep DBGSTR from being optimized out
static mut DBGSTR: [u32; 4] = [0, 0, 0, 0];

/// Infinite loop panic handler (TODO: fix this to use less power)
#[panic_handler]
fn panic(_panic: &PanicInfo<'_>) -> ! {
    loop {}
}

fn ticktimer_int_handler(_irq_no: usize) {
    let mut ticktimer_csr = CSR::new(HW_TICKTIMER_BASE as *mut u32);
    let mut crg_csr = CSR::new(HW_CRG_BASE as *mut u32);
    let mut power_csr = CSR::new(HW_POWER_BASE as *mut u32);

    // disarm the watchdog
    crg_csr.wfo(utra::crg::WATCHDOG_RESET_CODE, 0x600d);
    crg_csr.wfo(utra::crg::WATCHDOG_RESET_CODE, 0xc0de);

    // fast-monitor the keyboard wakeup inputs if the soc is in the off state
    if (power_csr.rf(utra::power::POWER_SOC_ON) == 0)
        && (power_csr.rf(utra::power::STATS_STATE) == 0)
    {
        // drive sense for keyboard
        let power = power_csr.ms(utra::power::POWER_SELF, 1)
            | power_csr.ms(utra::power::POWER_DISCHARGE, 1)
            | power_csr.ms(utra::power::POWER_KBDDRIVE, 1);
        power_csr.wo(utra::power::POWER, power);

        if power_csr.rf(utra::power::STATS_MONKEY) == 3 {
            // both keys have to be hit
            // power on the SOC
            let power = power_csr.ms(utra::power::POWER_SELF, 1)
                | power_csr.ms(utra::power::POWER_SOC_ON, 1);
            power_csr.wo(utra::power::POWER, power);
        } else {
            // re-engage discharge fets, disable keyboard drive
            let power = power_csr.ms(utra::power::POWER_SELF, 1)
                | power_csr.ms(utra::power::POWER_KBDDRIVE, 0)
                | power_csr.ms(utra::power::POWER_DISCHARGE, 1);
            power_csr.wo(utra::power::POWER, power);
        }
    }

    set_msleep_target_ticks(50); // resetting this will also clear the alarm

    ticktimer_csr.wfo(utra::ticktimer::EV_PENDING_ALARM, 1);
}

fn com_int_handler(_irq_no: usize) {
    let mut com_csr = CSR::new(HW_COM_BASE as *mut u32);
    // nop handler, here just to wake up the CPU in case of an incoming SPI packet and run the normal loop
    com_csr.wfo(utra::com::EV_PENDING_SPI_AVAIL, 1);
}

#[allow(dead_code)] // used for debugging
fn dump_rom_addr(addr: u32) {
    let rom_ptr: *mut u32 = (addr + HW_SPIFLASH_MEM as u32) as *mut u32;
    let rom = rom_ptr as *mut Volatile<u32>;
    for i in 0..64 {
        if i % 8 == 0 {
            sprint!("\n\r0x{:06x}: ", addr + i * 4);
        }
        let data: u32 = unsafe { (*rom.add(i as usize)).read() };
        sprint!(
            "{:02x} {:02x} {:02x} {:02x} ",
            data & 0xFF,
            (data >> 8) & 0xff,
            (data >> 16) & 0xff,
            (data >> 24) & 0xff
        );
    }
    sprintln!("");
}

fn com_tx(tx: u16) {
    let com_ptr: *mut u32 = utralib::HW_COM_MEM as *mut u32;
    let com_fifo = com_ptr as *mut Volatile<u32>;

    unsafe {
        (*com_fifo).write(tx as u32);
    }
}

fn com_rx(timeout: u32) -> Result<u16, &'static str> {
    let com_csr = CSR::new(HW_COM_BASE as *mut u32);
    let com_rd_ptr: *mut u32 = utralib::HW_COM_MEM as *mut u32;
    let com_rd = com_rd_ptr as *mut Volatile<u32>;

    if timeout != 0 && (com_csr.rf(utra::com::STATUS_RX_AVAIL) == 0) {
        let start = get_time_ms();
        loop {
            if com_csr.rf(utra::com::STATUS_RX_AVAIL) == 1 {
                break;
            } else if start + timeout < get_time_ms() {
                return Err("timeout");
            }
        }
    }
    Ok(unsafe { (*com_rd).read() as u16 })
}

fn ll_debug(msg: &str) {
    if cfg!(feature = "debug_uart") && true {
        // extra boolean for finer-grained control of debug spew
        sprintln!("{}", msg);
        // delay_ms(50);
    }
}

#[entry]
fn main() -> ! {
    /* loop {  // a tiny sanity check stub
        (debug::Uart {}).putc('a' as u8);
    } */
    ll_debug("\r\n====UP5K==02");
    let mut power_csr: CSR<u32> = CSR::new(HW_POWER_BASE as *mut u32);
    let mut com_csr = CSR::new(HW_COM_BASE as *mut u32);
    let mut crg_csr = CSR::new(HW_CRG_BASE as *mut u32);
    let mut ticktimer_csr = CSR::new(HW_TICKTIMER_BASE as *mut u32);
    let mut wifi_csr = CSR::new(HW_WIFI_BASE as *mut u32);
    let git_csr = CSR::new(HW_GIT_BASE as *mut u32);

    let com_rd_ptr: *mut u32 = utralib::HW_COM_MEM as *mut u32;
    let com_rd = com_rd_ptr as *mut Volatile<u32>;

    ll_debug("before nommu");
    // Initialize the no-MMU version of Xous, which will give us
    // basic access to tasks and interrupts.
    xous_nommu::init();

    let mut i2c = Hardi2c::new();
    ll_debug("i2c");

    time_init();
    ll_debug("time_init");

    i2c.i2c_init(CONFIG_CLOCK_FREQUENCY);
    ll_debug("i2c_init");

    let mut charger: BtCharger = BtCharger::new();
    ll_debug("charger");

    let mut last_run_time: u32 = get_time_ms();
    let mut loopcounter: u32 = 0; // in seconds, so this will last ~125 years
    let mut pd_loop_timer: u32 = 0;
    let mut pow = power_mgmt::PowerState {
        voltage: 0,
        last_voltage: 0,
        current: 0,
        stby_current: 0,
        soc_was_on: power_csr.rf(utra::power::STATS_STATE) == 1,
        battery_panic: false,
        voltage_glitch: false,
        usb_cc_event: false,
    };

    // this needs to be one of the first things called after I2C comes up
    charger.chg_set_safety(&mut i2c);
    ll_debug("chg_set_safety");

    gg_start(&mut i2c);
    ll_debug("gg_start");

    charger.chg_set_autoparams(&mut i2c);
    ll_debug("chg_set_autoparams");
    charger.chg_start(&mut i2c);
    ll_debug("chg_start");

    let mut usb_cc = BtUsbCc::new();
    ll_debug("BtUsbCc:new");
    let tusb320_rev = usb_cc.init(&mut i2c);
    ll_debug("usb_cc");

    let mut gyro: BtGyro = BtGyro::new();
    gyro.init();
    ll_debug("gyro_init");

    let mut backlight: BtBacklight = BtBacklight::new();
    backlight.set_brightness(&mut i2c, 0, 0); // make sure the backlight is off on boot
    ll_debug("backlight");

    let mut start_time: u32 = get_time_ms();
    let mut wifi_ready: bool = false;

    charger.update_regs(&mut i2c);
    ll_debug("charger.update_regs");

    let mut use_wifi: bool = true;

    /*
    // check that the gas gauge capacity is correct; if not, reset it
    if gg_set_design_capacity(&mut i2c, None) != 1100 {
        gg_set_design_capacity(&mut i2c, Some(1100));
    } */
    // seems to work better with the default 1340mAh capacity even though that's not our actual capacity

    /*  // kept around as a quick test routine for SPI flashing
        let mut idcode: [u8; 3] = [0; 3];
        spi_cmd(CMD_RDID, None, Some(&mut idcode));
        sprintln!("SPI ID code: {:02x} {:02x} {:02x}", idcode[0], idcode[1], idcode[2]);
        let test_addr = 0x8_0000;
        dump_rom_addr(test_addr);
        spi_erase_region(test_addr, 4096);

        dump_rom_addr(test_addr);

        let mut test_data: [u8; 256] = [0; 256];
        for i in 0..256 {
            test_data[i] = (255 - i) as u8;
        }
        spi_program_page(test_addr, &mut test_data);

        dump_rom_addr(test_addr);
    */
    spi_standby(); // make sure the OE's are off, no spurious power consumption
    ll_debug("spi_standby");

    xous_nommu::syscalls::sys_interrupt_claim(
        utra::ticktimer::TICKTIMER_IRQ,
        ticktimer_int_handler,
    )
    .unwrap();
    set_msleep_target_ticks(50);
    ticktimer_csr.wfo(utra::ticktimer::EV_PENDING_ALARM, 1); // clear the pending signal just in case
    ticktimer_csr.wfo(utra::ticktimer::EV_ENABLE_ALARM, 1); // enable the interrupt

    /////// NOTE TO SELF: if using GDB, must disable the watchdog!!!
    if cfg!(feature = "debug_uart") {
        ll_debug("debug_uart selected: watchdog is not enabled");
    } else {
        ll_debug("**WATCHDOG ENABLED**, but debugging: GDB will not work.");
        crg_csr.wfo(utra::crg::WATCHDOG_ENABLE, 1); // 1 = enable the watchdog reset
    }

    xous_nommu::syscalls::sys_interrupt_claim(utra::com::COM_IRQ, com_int_handler).unwrap();
    com_csr.wfo(utra::com::EV_ENABLE_SPI_AVAIL, 1);

    let mut com_sentinel: u16 = 0; // for link debugging mostly
    let mut flash_update_lock = false;
    let mut do_scan = false;
    ll_debug("main loop");
    delay_ms(250);
    loop {
        if !flash_update_lock {
            //////////////////////// WIFI HANDLER BLOCK ---------
            // slight delay to allow for wishbone-tool to connect for debuggening
            if (get_time_ms() - start_time > 1000) && !wifi_ready && use_wifi {
                sprintln!("initializing wifi!");
                // init the wifi interface
                if wfx_init() == SL_STATUS_OK {
                    sprintln!("Wifi ready");
                    wifi_ready = true;
                } else {
                    sprintln!("Wifi init failed");
                }
                start_time = get_time_ms();
            }
            if do_scan {
                if wifi_ready && use_wifi {
                    if get_time_ms() - start_time > 20_000 {
                        sprintln!("starting ssid scan");
                        wfx_start_scan(); // turn this off for FCC testing
                        start_time = get_time_ms();
                    }
                }
                if wfx_rs::hal_wf200::wf200_event_get() && use_wifi {
                    // first thing -- clear the event. So that if we get another event
                    // while handling this packet, we have a chance of detecting that.
                    // we lack mutexes, so we need to think about this behavior very carefully.

                    if wf200_mutex_get() {
                        // don't process events while the driver has locked us out
                        wfx_rs::hal_wf200::wf200_event_clear();

                        // handle the Rx packet
                        if wfx_scan_ongoing() {
                            wfx_handle_event();
                        }
                    }
                }
            }
            //////////////////////// ---------------------------

            //////////////////////// CHARGER HANDLER BLOCK -----
            charger_handler(
                &mut power_csr,
                &mut i2c,
                &mut charger,
                &mut usb_cc,
                &mut backlight,
                &mut last_run_time,
                &mut loopcounter,
                &mut pow,
            );
            //////////////////////// ---------------------------
        }

        //////////////////////// COM HANDLER BLOCK ---------
        while com_csr.rf(utra::com::STATUS_RX_AVAIL) == 1 {
            // if we've received data from the SOC, we don't have to assert its power-on line any more to boot it up.
            // De-activate it, so that the SOC is entirely in control of its own power state.
            power_csr.rmwf(utra::power::POWER_SOC_ON, 0);
            // note: this line is occasionally re-asserted whenever the charger is detected as present

            let rx: u16;
            unsafe {
                rx = (*com_rd).read() as u16;
            }

            if rx == ComState::SSID_CHECK.verb {
                if wf200_ssid_updated() {
                    com_tx(1);
                } else {
                    com_tx(0);
                }
            } else if rx == ComState::SSID_FETCH.verb {
                let ssid_list = wf200_ssid_get_list();

                for index in 0..16 * 6 {
                    com_tx(
                        ssid_list[index / 16].ssid[(index % 16) * 2] as u16
                            | ((ssid_list[index / 16].ssid[(index % 16) * 2 + 1] as u16) << 8),
                    );
                }
            } else if rx == ComState::LOOP_TEST.verb {
                com_tx((rx & 0xFF) | ((com_sentinel as u16 & 0xFF) << 8));
                com_sentinel += 1;
            } else if rx == ComState::GAS_GAUGE.verb {
                com_tx(pow.current as u16);
                com_tx(pow.stby_current as u16);
                com_tx(pow.voltage as u16);
                com_tx(power_csr.r(utra::power::POWER) as u16);
            } else if rx == ComState::GG_FACTORY_CAPACITY.verb {
                let mut error = false;
                let mut capacity: u16 = 1100;
                match com_rx(250) {
                    Ok(result) => capacity = result,
                    _ => error = true,
                }
                if !error {
                    // some manual "sanity checks" so we really don't bork the gas guage in case of a protocol error
                    if capacity >= 1900 {
                        capacity = 1100;
                    }
                    if capacity <= 600 {
                        capacity = 1100;
                    }
                    let old_capacity = gg_set_design_capacity(&mut i2c, Some(capacity));
                    com_tx(old_capacity);
                } else {
                    com_tx(ComState::ERROR.verb); // return an erroneous former capacity
                }
            } else if rx == ComState::GG_GET_CAPACITY.verb {
                let old_capacity = gg_set_design_capacity(&mut i2c, None);
                com_tx(old_capacity);
            } else if rx == ComState::GG_DEBUG.verb {
                if pow.voltage_glitch {
                    com_tx(1);
                } else {
                    com_tx(0);
                }
                pow.voltage_glitch = false;
            } else if rx == ComState::STAT.verb {
                com_tx(0x8888); // first is just a response to the initial command
                charger.update_regs(&mut i2c);
                for i in 0..0xC {
                    com_tx(charger.registers[i] as u16);
                }
                com_tx(pow.voltage as u16);
                com_tx(pow.stby_current as u16);
                com_tx(pow.current as u16);
            } else if rx == ComState::POWER_OFF.verb {
                com_tx(power_csr.r(utra::power::POWER) as u16);
                // ignore rapid, successive power down requests
                backlight.set_brightness(&mut i2c, 0, 0); // make sure the backlight is off
                if get_time_ms() - pd_loop_timer > 1500 {
                    let power = power_csr.ms(utra::power::POWER_SELF, 1)
                        | power_csr.ms(utra::power::POWER_DISCHARGE, 1);
                    power_csr.wo(utra::power::POWER, power);

                    set_msleep_target_ticks(500); // extend next service so we can discharge

                    pd_loop_timer = get_time_ms();
                }
            } else if rx == ComState::POWER_SHIPMODE.verb {
                backlight.set_brightness(&mut i2c, 0, 0); // make sure the backlight is off
                charger.set_shipmode(&mut i2c);
                gg_set_hibernate(&mut i2c);
                let power = power_csr.ms(utra::power::POWER_SELF, 1)
                    | power_csr.ms(utra::power::POWER_DISCHARGE, 1);
                power_csr.wo(utra::power::POWER, power);
                set_msleep_target_ticks(500); // extend next service so we can discharge

                pd_loop_timer = get_time_ms();
            } else if rx == ComState::POWER_CHARGER_STATE.verb {
                if charger.chg_is_charging(&mut i2c, false) {
                    com_tx(1);
                } else {
                    com_tx(0);
                }
            } else if rx == ComState::GG_SOC.verb {
                com_tx(gg_state_of_charge(&mut i2c) as u16);
            } else if rx == ComState::GG_REMAINING.verb {
                com_tx(gg_remaining_capacity(&mut i2c) as u16);
            } else if rx == ComState::GG_FULL_CAPACITY.verb {
                com_tx(gg_full_capacity(&mut i2c) as u16);
            } else if rx == ComState::GYRO_UPDATE.verb {
                gyro.update_xyz();
            } else if rx == ComState::GYRO_READ.verb {
                com_tx(gyro.x);
                com_tx(gyro.y);
                com_tx(gyro.z);
                com_tx(gyro.id as u16);
            } else if rx == ComState::POLL_USB_CC.verb {
                if pow.usb_cc_event {
                    com_tx(1)
                } else {
                    com_tx(0)
                }
                pow.usb_cc_event = false; // clear the usb_cc_event pending flag as its been checked
                for i in 0..3 {
                    com_tx(usb_cc.status[i] as u16);
                }
                com_tx(tusb320_rev as u16);
            } else if rx == ComState::CHG_START.verb {
                // charging mode
                charger.chg_start(&mut i2c);
            } else if rx == ComState::CHG_BOOST_ON.verb {
                // boost on
                charger.chg_boost(&mut i2c);
            } else if rx == ComState::CHG_BOOST_OFF.verb {
                // boost off
                charger.chg_boost_off(&mut i2c);
            } else if rx >= ComState::BL_START.verb && rx <= ComState::BL_END.verb {
                let main_bl_level: u8 = (rx & 0x1F) as u8;
                let sec_bl_level: u8 = ((rx >> 5) & 0x1F) as u8;
                backlight.set_brightness(&mut i2c, main_bl_level, sec_bl_level);
            } else if rx == ComState::LINK_READ.verb {
                // this a "read continuation" command, in other words, return read data
                // based on the current ComState
            } else if rx == ComState::LINK_SYNC.verb {
                // sync link command, when received, empty all the FIFOs, and prime Tx with dummy data
                com_csr.wfo(utra::com::CONTROL_RESET, 1); // reset fifos
                com_csr.wfo(utra::com::CONTROL_CLRERR, 1); // clear all error flags
            } else if rx == ComState::FLASH_ERASE.verb {
                let mut error = false;
                let mut address: u32 = 0;
                let mut len: u32 = 0;
                // receive address in "network order" (big endian)
                match com_rx(100) {
                    Ok(result) => address = (result as u32) << 16,
                    _ => error = true,
                }
                match com_rx(100) {
                    Ok(result) => address |= (result as u32) & 0xFFFF,
                    _ => error = true,
                }
                // receive len, in bytes
                match com_rx(100) {
                    Ok(result) => len = (result as u32) << 16,
                    _ => error = true,
                }
                match com_rx(100) {
                    Ok(result) => len |= (result as u32) & 0xFFFF,
                    _ => error = true,
                }
                if !error {
                    sprintln!("Erasing {} bytes from 0x{:08x}", len, address);
                    spi_erase_region(address, len);
                }
            } else if rx == ComState::FLASH_PP.verb {
                let mut error = false;
                let mut address: u32 = 0;
                let mut page: [u8; 256] = [0; 256];
                // receive address in "network order" (big endian)
                match com_rx(100) {
                    Ok(result) => address = (result as u32) << 16,
                    _ => error = true,
                }
                match com_rx(100) {
                    Ok(result) => address |= (result as u32) & 0xFFFF,
                    _ => error = true,
                }
                for i in 0..128 {
                    match com_rx(200) {
                        Ok(result) => {
                            let b = result.to_le_bytes();
                            page[i * 2] = b[0];
                            page[i * 2 + 1] = b[1];
                        }
                        _ => error = true,
                    }
                }
                if !error {
                    // sprintln!("Programming 256 bytes to 0x{:08x}", address);
                    spi_program_page(address, &mut page);
                }
            } else if rx == ComState::FLASH_LOCK.verb {
                flash_update_lock = true;
                wifi_csr.wfo(utra::wifi::EV_ENABLE_WIRQ, 0);
            } else if rx == ComState::FLASH_UNLOCK.verb {
                flash_update_lock = false;
                wifi_csr.wfo(utra::wifi::EV_ENABLE_WIRQ, 1);
            } else if rx == ComState::FLASH_WAITACK.verb {
                com_tx(ComState::FLASH_ACK.verb);
            } else if rx == ComState::WFX_RXSTAT_GET.verb {
                let rx_stat_raw: [u8; 376] = wf200_get_rx_stats_raw();
                for i in 0..rx_stat_raw.len() / 2 {
                    com_tx(rx_stat_raw[i * 2] as u16 | ((rx_stat_raw[i * 2 + 1] as u16) << 8));
                }
            } else if rx == ComState::WFX_PDS_LINE_SET.verb {
                // set one line of the PDS record (up to 256 bytes length)
                let mut error = false;
                let mut pds_data: [u8; 256] = [0; 256];
                let mut pds_length: u16 = 0;
                match com_rx(500) {
                    Ok(result) => pds_length = result,
                    _ => error = true,
                }
                if pds_length >= 256 {
                    // length is in BYTES not words
                    error = true;
                }
                // even if length error, do receive, because we have to clear the rx queue for proper operation
                for i in 0..128 as usize {
                    // ALWAYS expect 128 pds data elements, even if length < 256
                    match com_rx(500) {
                        Ok(result) => {
                            let b = result.to_le_bytes();
                            pds_data[i * 2] = b[0];
                            pds_data[i * 2 + 1] = b[1];
                        }
                        _ => error = true,
                    }
                }
                if false {
                    // nuggets for debugging PDS issues
                    ll_debug("got wfx_pds_line_set command");
                    //let s = unsafe{ core::str::from_utf8_unchecked(&pds_data[0..(pds_length as usize)]) };
                    //ll_debug(s);
                    sprintln!("length {}", pds_length);
                    sprintln!("{:?}", pds_data);
                    sprintln!("error {:?}", error);
                    delay_ms(50);
                }
                if !error {
                    wf200_send_pds(pds_data, pds_length);
                }
                com_csr.wfo(utra::com::CONTROL_RESET, 1); // reset fifos
                com_csr.wfo(utra::com::CONTROL_CLRERR, 1); // clear all error flags
            } else if rx == ComState::WFX_FW_REV_GET.verb {
                com_tx(wf200_fw_major() as u16);
                com_tx(wf200_fw_minor() as u16);
                com_tx(wf200_fw_build() as u16);
            } else if rx == ComState::EC_GIT_REV.verb {
                com_tx((git_csr.rf(utra::git::GITREV_GITREV) >> 16) as u16);
                com_tx((git_csr.rf(utra::git::GITREV_GITREV) & 0xFFFF) as u16);
                com_tx(git_csr.rf(utra::git::DIRTY_DIRTY) as u16);
            } else if rx == ComState::WF200_RESET.verb {
                match com_rx(250) {
                    Ok(result) => {
                        if result == 0 {
                            wifi_ready = false;
                            use_wifi = true;
                            wifi_csr.rmwf(utra::wifi::WIFI_RESET, 1);
                            delay_ms(10);
                            wifi_csr.rmwf(utra::wifi::WIFI_RESET, 0);
                            delay_ms(10);
                            start_time = get_time_ms();
                        } else {
                            wifi_ready = false;
                            use_wifi = false;
                            wifi_csr.rmwf(utra::wifi::WIFI_RESET, 1);
                        }
                    }
                    _ => {
                        // default to a normal reset
                        wifi_ready = false;
                        use_wifi = true;
                        wifi_csr.rmwf(utra::wifi::WIFI_RESET, 1);
                        delay_ms(10);
                        wifi_csr.rmwf(utra::wifi::WIFI_RESET, 0);
                        delay_ms(10);
                        start_time = get_time_ms();
                    }
                }
            } else if rx == ComState::SSID_SCAN_ON.verb {
                do_scan = true;
            } else if rx == ComState::SSID_SCAN_OFF.verb {
                do_scan = false;
            } else {
                com_tx(ComState::ERROR.verb);
            }
        }
        //////////////////////// ---------------------------
        // unsafe { riscv::asm::wfi() }; // potential for power savings? unfortunately WFI seems broken
    }
}
