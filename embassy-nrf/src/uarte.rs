//! Async UART

use core::future::Future;
use core::marker::PhantomData;
use core::sync::atomic::{compiler_fence, Ordering};
use core::task::Poll;
use embassy::interrupt::InterruptExt;
use embassy::traits::uart::{Error, Read, Write};
use embassy::util::{AtomicWaker, OnDrop, Unborrow};
use embassy_extras::unborrow;
use futures::future::poll_fn;

use crate::fmt::{assert, panic, *};
use crate::gpio::sealed::Pin as _;
use crate::gpio::{OptionalPin as GpioOptionalPin, Pin as GpioPin};
use crate::interrupt;
use crate::interrupt::Interrupt;
use crate::pac;
use crate::peripherals;
use crate::target_constants::EASY_DMA_SIZE;

// Re-export SVD variants to allow user to directly set values.
pub use pac::uarte0::{baudrate::BAUDRATE_A as Baudrate, config::PARITY_A as Parity};

#[non_exhaustive]
pub struct Config {
    pub parity: Parity,
    pub baudrate: Baudrate,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            parity: Parity::EXCLUDED,
            baudrate: Baudrate::BAUD115200,
        }
    }
}

/// Interface to the UARTE peripheral
pub struct Uarte<'d, T: Instance> {
    peri: T,
    phantom: PhantomData<&'d mut T>,
}

impl<'d, T: Instance> Uarte<'d, T> {
    /// Creates the interface to a UARTE instance.
    /// Sets the baud rate, parity and assigns the pins to the UARTE peripheral.
    ///
    /// # Safety
    ///
    /// The returned API is safe unless you use `mem::forget` (or similar safe mechanisms)
    /// on stack allocated buffers which which have been passed to [`send()`](Uarte::send)
    /// or [`receive`](Uarte::receive).
    #[allow(unused_unsafe)]
    pub unsafe fn new(
        uarte: impl Unborrow<Target = T> + 'd,
        irq: impl Unborrow<Target = T::Interrupt> + 'd,
        rxd: impl Unborrow<Target = impl GpioPin> + 'd,
        txd: impl Unborrow<Target = impl GpioPin> + 'd,
        cts: impl Unborrow<Target = impl GpioOptionalPin> + 'd,
        rts: impl Unborrow<Target = impl GpioOptionalPin> + 'd,
        config: Config,
    ) -> Self {
        unborrow!(uarte, irq, rxd, txd, cts, rts);

        let r = T::regs();

        assert!(r.enable.read().enable().is_disabled());

        rxd.conf().write(|w| w.input().connect().drive().h0h1());
        r.psel.rxd.write(|w| unsafe { w.bits(rxd.psel_bits()) });

        txd.set_high();
        txd.conf().write(|w| w.dir().output().drive().h0h1());
        r.psel.txd.write(|w| unsafe { w.bits(txd.psel_bits()) });

        if let Some(pin) = rts.pin_mut() {
            pin.set_high();
            pin.conf().write(|w| w.dir().output().drive().h0h1());
        }
        r.psel.cts.write(|w| unsafe { w.bits(cts.psel_bits()) });

        if let Some(pin) = cts.pin_mut() {
            pin.conf().write(|w| w.input().connect().drive().h0h1());
        }
        r.psel.rts.write(|w| unsafe { w.bits(rts.psel_bits()) });

        // Configure
        let hardware_flow_control = match (rts.pin().is_some(), cts.pin().is_some()) {
            (false, false) => false,
            (true, true) => true,
            _ => panic!("RTS and CTS pins must be either both set or none set."),
        };
        r.config.write(|w| {
            w.hwfc().bit(hardware_flow_control);
            w.parity().variant(config.parity);
            w
        });
        r.baudrate.write(|w| w.baudrate().variant(config.baudrate));

        // Disable all interrupts
        r.intenclr.write(|w| unsafe { w.bits(0xFFFF_FFFF) });

        // Reset rxstarted, txstarted. These are used by drop to know whether a transfer was
        // stopped midway or not.
        r.events_rxstarted.reset();
        r.events_txstarted.reset();

        irq.set_handler(Self::on_interrupt);
        irq.unpend();
        irq.enable();

        // Enable
        r.enable.write(|w| w.enable().enabled());

        Self {
            peri: uarte,
            phantom: PhantomData,
        }
    }

    fn on_interrupt(_: *mut ()) {
        let r = T::regs();
        let s = T::state();

        if r.events_endrx.read().bits() != 0 {
            s.endrx_waker.wake();
            r.intenclr.write(|w| w.endrx().clear());
        }
        if r.events_endtx.read().bits() != 0 {
            s.endtx_waker.wake();
            r.intenclr.write(|w| w.endtx().clear());
        }

        if r.events_rxto.read().bits() != 0 {
            r.intenclr.write(|w| w.rxto().clear());
        }
        if r.events_txstopped.read().bits() != 0 {
            r.intenclr.write(|w| w.txstopped().clear());
        }
    }
}

impl<'a, T: Instance> Drop for Uarte<'a, T> {
    fn drop(&mut self) {
        info!("uarte drop");

        let r = T::regs();

        let did_stoprx = r.events_rxstarted.read().bits() != 0;
        let did_stoptx = r.events_txstarted.read().bits() != 0;
        info!("did_stoprx {} did_stoptx {}", did_stoprx, did_stoptx);

        // Wait for rxto or txstopped, if needed.
        r.intenset.write(|w| w.rxto().set().txstopped().set());
        while (did_stoprx && r.events_rxto.read().bits() == 0)
            || (did_stoptx && r.events_txstopped.read().bits() == 0)
        {
            info!("uarte drop: wfe");
            cortex_m::asm::wfe();
        }

        cortex_m::asm::sev();

        // Finally we can disable!
        r.enable.write(|w| w.enable().disabled());

        info!("uarte drop: done");

        // TODO: disable pins
    }
}

impl<'d, T: Instance> Read for Uarte<'d, T> {
    #[rustfmt::skip]
    type ReadFuture<'a> where Self: 'a = impl Future<Output = Result<(), Error>> + 'a;

    fn read<'a>(&'a mut self, rx_buffer: &'a mut [u8]) -> Self::ReadFuture<'a> {
        async move {
            let ptr = rx_buffer.as_ptr();
            let len = rx_buffer.len();
            assert!(len <= EASY_DMA_SIZE);

            let r = T::regs();
            let s = T::state();

            let drop = OnDrop::new(move || {
                info!("read drop: stopping");

                r.intenclr.write(|w| w.endrx().clear());
                r.events_rxto.reset();
                r.tasks_stoprx.write(|w| unsafe { w.bits(1) });

                while r.events_endrx.read().bits() == 0 {}

                info!("read drop: stopped");
            });

            r.rxd.ptr.write(|w| unsafe { w.ptr().bits(ptr as u32) });
            r.rxd.maxcnt.write(|w| unsafe { w.maxcnt().bits(len as _) });

            r.events_endrx.reset();
            r.intenset.write(|w| w.endrx().set());

            compiler_fence(Ordering::SeqCst);

            trace!("startrx");
            r.tasks_startrx.write(|w| unsafe { w.bits(1) });

            poll_fn(|cx| {
                s.endrx_waker.register(cx.waker());
                if r.events_endrx.read().bits() != 0 {
                    return Poll::Ready(());
                }
                Poll::Pending
            })
            .await;

            compiler_fence(Ordering::SeqCst);
            r.events_rxstarted.reset();
            drop.defuse();

            Ok(())
        }
    }
}

impl<'d, T: Instance> Write for Uarte<'d, T> {
    #[rustfmt::skip]
    type WriteFuture<'a> where Self: 'a = impl Future<Output = Result<(), Error>> + 'a;

    fn write<'a>(&'a mut self, tx_buffer: &'a [u8]) -> Self::WriteFuture<'a> {
        async move {
            let ptr = tx_buffer.as_ptr();
            let len = tx_buffer.len();
            assert!(len <= EASY_DMA_SIZE);
            // TODO: panic if buffer is not in SRAM

            let r = T::regs();
            let s = T::state();

            let drop = OnDrop::new(move || {
                info!("write drop: stopping");

                r.intenclr.write(|w| w.endtx().clear());
                r.events_txstopped.reset();
                r.tasks_stoptx.write(|w| unsafe { w.bits(1) });

                // TX is stopped almost instantly, spinning is fine.
                while r.events_endtx.read().bits() == 0 {}
                info!("write drop: stopped");
            });

            r.txd.ptr.write(|w| unsafe { w.ptr().bits(ptr as u32) });
            r.txd.maxcnt.write(|w| unsafe { w.maxcnt().bits(len as _) });

            r.events_endtx.reset();
            r.intenset.write(|w| w.endtx().set());

            compiler_fence(Ordering::SeqCst);

            trace!("starttx");
            r.tasks_starttx.write(|w| unsafe { w.bits(1) });

            poll_fn(|cx| {
                s.endtx_waker.register(cx.waker());
                if r.events_endtx.read().bits() != 0 {
                    return Poll::Ready(());
                }
                Poll::Pending
            })
            .await;

            compiler_fence(Ordering::SeqCst);
            r.events_txstarted.reset();
            drop.defuse();

            Ok(())
        }
    }
}

mod sealed {
    use super::*;

    pub struct State {
        pub endrx_waker: AtomicWaker,
        pub endtx_waker: AtomicWaker,
    }
    impl State {
        pub const fn new() -> Self {
            Self {
                endrx_waker: AtomicWaker::new(),
                endtx_waker: AtomicWaker::new(),
            }
        }
    }

    pub trait Instance {
        fn regs() -> &'static pac::uarte0::RegisterBlock;
        fn state() -> &'static State;
    }
}

pub trait Instance: sealed::Instance + 'static {
    type Interrupt: Interrupt;
}

macro_rules! impl_instance {
    ($type:ident, $irq:ident) => {
        impl sealed::Instance for peripherals::$type {
            fn regs() -> &'static pac::uarte0::RegisterBlock {
                unsafe { &*pac::$type::ptr() }
            }
            fn state() -> &'static sealed::State {
                static STATE: sealed::State = sealed::State::new();
                &STATE
            }
        }
        impl Instance for peripherals::$type {
            type Interrupt = interrupt::$irq;
        }
    };
}

impl_instance!(UARTE0, UARTE0_UART0);
#[cfg(any(feature = "52833", feature = "52840", feature = "9160"))]
impl_instance!(UARTE1, UARTE1);
