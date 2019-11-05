#![no_main]
#![no_std]

use core::panic::PanicInfo;
use riscv_rt::entry;

extern crate betrusted_hal;

const CONFIG_CLOCK_FREQUENCY: u32 = 12_000_000;

// allocate a global, unsafe static string for debug output
#[used]  // test to see if the "used" attribute means we no longer need to call debugcommit() <<< 
static mut DBGSTR: [u32; 4] = [0, 0, 0, 0];

#[panic_handler]
fn panic(_panic: &PanicInfo<'_>) -> ! {
    loop {}
}

// debug simply forces DBGSTR to be committed to an unsafe bbbbb
fn debugcommit(p: &betrusted_pac::Peripherals) {
    for i in 0..4 {
        unsafe{&p.RGB.raw.write( |w| {w.bits(DBGSTR[i] as u32)}); }
    }
}

#[entry]
fn main() -> ! {
    use betrusted_hal::hal_i2c::hal_i2c::*;
    use betrusted_hal::hal_time::hal_time::*;
    use betrusted_hal::api_gasgauge::api_gasgauge::*;
    use betrusted_hal::api_charger::api_charger::*;

    let p = betrusted_pac::Peripherals::take().unwrap();

    i2c_init(&p, CONFIG_CLOCK_FREQUENCY / 1_000_000);
    time_init(&p);

    gg_start(&p);
    chg_set_safety(&p);
    chg_set_autoparams(&p);
    chg_start(&p);

    unsafe{ DBGSTR[0] = gg_device_type(&p) as u32; }

    // flash an LED!
    loop { 
        unsafe{p.RGB.raw.write( |w| {w.bits(5)}); }
        delay_ms(&p, 500);

        unsafe{ DBGSTR[1] = gg_voltage(&p) as u32; }
        if chg_is_charging(&p) {
            unsafe{ DBGSTR[2] = 1;}
        } else {
            unsafe{ DBGSTR[2] = 0;}
        }
        debugcommit(&p); // check if the "used" attrbute obviates this call
        
        unsafe{p.RGB.raw.write( |w| {w.bits(0)}); }
        delay_ms(&p, 500);

        chg_keepalive_ping(&p);
    }
}