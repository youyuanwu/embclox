#![no_std]
#![allow(unused)]

extern crate alloc;

#[macro_use]
extern crate log;

pub mod e1000;
pub mod pci;
mod utils;

pub use volatile::Volatile;

pub trait Ext {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {}
}
