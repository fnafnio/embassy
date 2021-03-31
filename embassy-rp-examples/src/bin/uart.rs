#![no_std]
#![no_main]
#![feature(asm)]
#![feature(min_type_alias_impl_trait)]
#![feature(impl_trait_in_bindings)]
#![feature(type_alias_impl_trait)]
#![allow(incomplete_features)]

#[path = "../example_common.rs"]
mod example_common;

use defmt::*;
use embassy::executor::Spawner;
use embassy_rp::{uart, Peripherals};

#[embassy::main]
async fn main(_spanwer: Spawner) {
    let p = unwrap!(Peripherals::take());

    let config = uart::Config::default();
    let mut uart = uart::Uart::new(p.UART0, p.PIN_0, p.PIN_1, p.PIN_2, p.PIN_3, config);
    uart.send("Hello World!\r\n".as_bytes());

    loop {
        uart.send("hello there!\r\n".as_bytes());
        cortex_m::asm::delay(1_000_000);
    }
}
