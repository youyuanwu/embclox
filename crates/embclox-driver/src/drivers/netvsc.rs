//! Hyper-V synthetic NIC (NetVSC) VMBus driver wrapper.

use crate::driver::{ProbeCtx, VmBusDriver};
use crate::error::ProbeError;
use crate::nic::EmbcloxNic;
use alloc::boxed::Box;
use core::task::{Context, Waker};
use embassy_net_driver::{Capabilities, HardwareAddress, LinkState};
use embclox_hyperv::netvsc::{NetvscDevice, NETVSC_WAKER};
use embclox_hyperv::{guid, ChannelOffer};

pub struct NetvscDriver;

impl VmBusDriver for NetvscDriver {
    fn name(&self) -> &'static str {
        "netvsc"
    }
    fn priority(&self) -> u8 {
        10
    }
    fn matches(&self, offer: &ChannelOffer) -> bool {
        offer.device_type == guid::NETVSC
    }
    fn probe(
        &self,
        offer: ChannelOffer,
        ctx: &mut ProbeCtx<'_>,
    ) -> Result<Box<dyn EmbcloxNic>, ProbeError> {
        let vmbus = ctx
            .vmbus
            .as_deref_mut()
            .ok_or(ProbeError::Driver("netvsc: vmbus unavailable"))?;
        log::info!("netvsc: probing offer relid={}", offer.child_relid);

        let device = NetvscDevice::init(vmbus, ctx.dma, ctx.memory)
            .map_err(|_| ProbeError::Driver("netvsc: NetvscDevice::init failed"))?;
        let mac = device.mac();
        let mtu = device.mtu();
        log::info!(
            "netvsc: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, MTU {}",
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5],
            mtu,
        );

        Ok(Box::new(NicNetvsc { device, mac, mtu }))
    }
}

// ---- EmbcloxNic wrapper ------------------------------------------------

struct NicNetvsc {
    device: NetvscDevice,
    mac: [u8; 6],
    mtu: u32,
}

unsafe impl Send for NicNetvsc {}

impl EmbcloxNic for NicNetvsc {
    fn rx_ready(&mut self) -> bool {
        self.device.has_rx_packet()
    }
    fn tx_ready(&mut self) -> bool {
        self.device.has_tx_space()
    }
    fn register_waker(&mut self, waker: &Waker) {
        NETVSC_WAKER.register(waker);
    }
    fn recv_with(&mut self, f: &mut dyn FnMut(&mut [u8])) {
        self.device
            .recv_with(|buf| f(buf))
            .expect("netvsc: recv_with called without ready packet");
    }
    fn transmit_with(&mut self, len: usize, f: &mut dyn FnMut(&mut [u8])) {
        self.device
            .transmit_with(len, |buf| f(buf))
            .expect("netvsc: transmit_with called without TX space");
    }
    fn link_state(&mut self, _cx: &mut Context<'_>) -> LinkState {
        LinkState::Up
    }
    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::default();
        caps.max_transmission_unit = self.mtu as usize;
        caps
    }
    fn hardware_address(&self) -> HardwareAddress {
        HardwareAddress::Ethernet(self.mac)
    }
}
