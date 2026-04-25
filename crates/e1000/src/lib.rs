#![no_std]

pub mod desc;
pub mod device;
pub mod dma;
pub mod error;
pub mod regs;

pub use device::{E1000Device, RxHalf, TxHalf};
pub use dma::{DmaAllocator, DmaRegion};
pub use error::InterruptStatus;
pub use regs::RegisterAccess;
