//! Async utilities
mod critical_section;
mod drop_bomb;
mod forever;
mod mutex;
mod on_drop;
mod portal;
mod signal;

#[cfg_attr(feature = "executor-agnostic", path = "waker_agnostic.rs")]
mod waker;

pub use critical_section::*;
pub use drop_bomb::*;
pub use forever::*;
pub use mutex::*;
pub use on_drop::*;
pub use portal::*;
pub use signal::*;
pub use waker::*;

pub trait Unborrow {
    type Target;
    unsafe fn unborrow(self) -> Self::Target;
}

pub trait Steal {
    unsafe fn steal() -> Self;
}
