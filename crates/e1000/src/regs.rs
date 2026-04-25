/// Abstraction over MMIO register access.
///
/// `offset` is a **word index** (register byte offset / 4).
/// Both read and write use `&self` — MMIO side-effects are in
/// hardware, not Rust memory. Implementations must use volatile
/// access internally.
pub trait RegisterAccess {
    fn read_reg(&self, offset: usize) -> u32;
    fn write_reg(&self, offset: usize, value: u32);
}

// E1000 register word indices (byte offset / 4).
// From the Intel 82540EP/EM manual.

pub const CTL: usize = 0x00000;
pub const STAT: usize = 0x00008 / 4;
pub const FCAL: usize = 0x00028 / 4;
pub const FCAH: usize = 0x0002C / 4;
pub const FCT: usize = 0x00030 / 4;
pub const FCTTV: usize = 0x00170 / 4;
pub const ICR: usize = 0x000C0 / 4;
pub const ITR: usize = 0x000C4 / 4;
pub const ICS: usize = 0x000C8 / 4;
pub const IMS: usize = 0x000D0 / 4;
pub const IMC: usize = 0x000D8 / 4;
pub const RCTL: usize = 0x00100 / 4;
pub const TCTL: usize = 0x00400 / 4;
pub const TIPG: usize = 0x00410 / 4;
pub const RDBAL: usize = 0x02800 / 4;
pub const RDBAH: usize = 0x02804 / 4;
pub const RDTR: usize = 0x02820 / 4;
pub const RADV: usize = 0x0282C / 4;
pub const RDH: usize = 0x02810 / 4;
pub const RDT: usize = 0x02818 / 4;
pub const RDLEN: usize = 0x02808 / 4;
pub const TDBAL: usize = 0x03800 / 4;
pub const TDBAH: usize = 0x03804 / 4;
pub const TDLEN: usize = 0x03808 / 4;
pub const TDH: usize = 0x03810 / 4;
pub const TDT: usize = 0x03818 / 4;
pub const TIDV: usize = 0x03820 / 4;
pub const TADV: usize = 0x0382C / 4;
pub const MTA: usize = 0x05200 / 4;
pub const RAL: usize = 0x05400 / 4;
pub const RAH: usize = 0x05404 / 4;
pub const RFCTL: usize = 0x05008 / 4;

// Device Control
pub const CTL_ASDE: u32 = 0x00000020;
pub const CTL_SLU: u32 = 0x00000040;
pub const CTL_RST: u32 = 1 << 26;

// Transmit Control
pub const TCTL_EN: u32 = 0x00000002;
pub const TCTL_PSP: u32 = 0x00000008;
pub const TCTL_CT_SHIFT: u32 = 4;
pub const TCTL_COLD_SHIFT: u32 = 12;

// Receive Control
pub const RCTL_EN: u32 = 0x00000002;
pub const RCTL_UPE: u32 = 0x00000008;
pub const RCTL_MPE: u32 = 0x00000010;
pub const RCTL_BAM: u32 = 0x00008000;
pub const RCTL_SZ_2048: u32 = 0x00000000;
pub const RCTL_SECRC: u32 = 0x04000000;

// Transmit Descriptor
pub const TXD_CMD_EOP: u8 = 0x01;
pub const TXD_CMD_RS: u8 = 0x08;
pub const TXD_STAT_DD: u8 = 0x01;

// Receive Descriptor
pub const RXD_STAT_DD: u8 = 0x01;
pub const RXD_STAT_EOP: u8 = 0x02;

// Interrupt
pub const IMS_RXT0: u32 = 0x00000080;
pub const IMS_LSC: u32 = 0x00000004;
pub const IMS_ENABLE_MASK: u32 = IMS_RXT0 | IMS_LSC;
pub const ICR_LSC: u32 = 0x00000004;
