//! Limine boot protocol glue for the HAL.
//!
//! Limine requests must live in the kernel binary (the bootloader scans the
//! ELF for the `.requests` section), so the request statics themselves are
//! declared in each example via the [`limine_boot_requests!`] macro.  The
//! macro emits a private module that:
//!
//! 1. Defines the standard request statics (BaseRevision, HHDM, kernel
//!    address, cmdline, memory map, framebuffer, stack-size) with the
//!    correct `link_section` attributes.
//! 2. Re-exports a `collect()` function that reads the responses and
//!    returns a [`LimineBootInfo`] suitable for [`crate::init`].
//!
//! Typical usage in `kmain`:
//!
//! ```ignore
//! embclox_hal_x86::limine_boot_requests!(limine_boot);
//!
//! #[unsafe(no_mangle)]
//! unsafe extern "C" fn kmain() -> ! {
//!     let boot_info = limine_boot::collect();
//!     let p = embclox_hal_x86::init(boot_info, embclox_hal_x86::Config::default());
//!     // ... drivers, runtime ...
//! }
//! ```
//!
//! The `limine` crate is re-exported as [`crate::limine_boot::limine`] so
//! examples don't need to add it as a separate dependency.

pub use limine;

/// Information collected from Limine boot responses, in the form
/// [`crate::init`] expects.
///
/// Only the fields the HAL needs are exposed.  Examples that need additional
/// requests (framebuffer, stack size, etc.) can declare extra statics
/// alongside the macro-generated ones and read them directly.
#[derive(Debug, Clone, Copy)]
pub struct LimineBootInfo<'a> {
    /// Higher-half direct map offset: `vaddr = paddr + hhdm_offset`.
    pub hhdm_offset: u64,
    /// Kernel virtual-to-physical offset: `paddr = vaddr - kernel_offset`.
    /// Computed from the Limine `ExecutableAddressRequest` response
    /// (`virtual_base - physical_base`).
    pub kernel_offset: u64,
    /// Kernel command-line string, or `""` if Limine didn't provide one.
    pub cmdline: &'a str,
}

/// Declare the standard Limine request statics in the calling crate and
/// generate a `collect()` function that reads them into a [`LimineBootInfo`].
///
/// The macro takes one argument — the module name to emit. All request
/// statics are placed inside that module so the call site stays clean.
///
/// ```ignore
/// embclox_hal_x86::limine_boot_requests!(limine_boot);
/// // generates:
/// //   mod limine_boot {
/// //       static BASE_REVISION: ... = ...;
/// //       static HHDM_REQUEST: ...  = ...;
/// //       ... etc ...
/// //       pub fn collect() -> embclox_hal_x86::limine_boot::LimineBootInfo<'static> { ... }
/// //   }
/// ```
///
/// Requests declared:
/// - `BaseRevision` — protocol version negotiation
/// - `RequestsStartMarker` / `RequestsEndMarker` — `.requests_*_marker`
///   section anchors
/// - `HhdmRequest` — physical memory mapping offset
/// - `ExecutableAddressRequest` — kernel virt/phys base for offset compute
/// - `ExecutableCmdlineRequest` — kernel command line
/// - `MemoryMapRequest` — physical memory map (re-exported for callers)
/// - `FramebufferRequest` — graphics framebuffer (re-exported for callers)
/// - `StackSizeRequest` — request 64 KiB stack (matches existing examples)
#[macro_export]
macro_rules! limine_boot_requests {
    ($mod_name:ident) => {
        mod $mod_name {
            use $crate::limine_boot::limine::request::{
                ExecutableAddressRequest, ExecutableCmdlineRequest, FramebufferRequest,
                HhdmRequest, MemoryMapRequest, RequestsEndMarker, RequestsStartMarker,
                StackSizeRequest,
            };
            use $crate::limine_boot::limine::BaseRevision;

            #[used]
            #[unsafe(link_section = ".requests_start_marker")]
            static _START_MARKER: RequestsStartMarker = RequestsStartMarker::new();

            #[used]
            #[unsafe(link_section = ".requests_end_marker")]
            static _END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

            #[used]
            #[unsafe(link_section = ".requests")]
            static BASE_REVISION: BaseRevision = BaseRevision::new();

            #[used]
            #[unsafe(link_section = ".requests")]
            static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

            #[used]
            #[unsafe(link_section = ".requests")]
            static KERNEL_ADDR_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();

            #[used]
            #[unsafe(link_section = ".requests")]
            static CMDLINE_REQUEST: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();

            #[used]
            #[unsafe(link_section = ".requests")]
            pub static MEMMAP_REQUEST: MemoryMapRequest = MemoryMapRequest::new();

            #[used]
            #[unsafe(link_section = ".requests")]
            pub static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

            #[used]
            #[unsafe(link_section = ".requests")]
            static STACK_SIZE_REQUEST: StackSizeRequest =
                StackSizeRequest::new().with_size(64 * 1024);

            /// Read the Limine responses and build a [`LimineBootInfo`].
            ///
            /// Asserts that the bootloader honoured the requested base
            /// revision; panics otherwise so a Limine version mismatch
            /// fails loudly instead of producing wrong offsets.
            pub fn collect() -> $crate::limine_boot::LimineBootInfo<'static> {
                assert!(
                    BASE_REVISION.is_supported(),
                    "Limine base revision not supported by bootloader"
                );

                let hhdm_offset = HHDM_REQUEST
                    .get_response()
                    .map(|r| r.offset())
                    .expect("Limine HhdmRequest response missing");

                let kernel_offset = KERNEL_ADDR_REQUEST
                    .get_response()
                    .map(|r| r.virtual_base() - r.physical_base())
                    .expect("Limine ExecutableAddressRequest response missing");

                let cmdline = CMDLINE_REQUEST
                    .get_response()
                    .and_then(|r| r.cmdline().to_str().ok())
                    .unwrap_or("");

                $crate::limine_boot::LimineBootInfo {
                    hhdm_offset,
                    kernel_offset,
                    cmdline,
                }
            }
        }
    };
}
