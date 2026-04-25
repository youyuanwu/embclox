use bootloader_api::BootInfo;
use linked_list_allocator::LockedHeap;

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

const HEAP_SIZE: usize = 4 * 1024 * 1024; // 4 MiB

#[unsafe(link_section = ".bss")]
static mut HEAP_AREA: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

pub fn init(_boot_info: &'static mut BootInfo) {
    let heap_start = core::ptr::addr_of_mut!(HEAP_AREA);
    unsafe {
        ALLOCATOR.lock().init(heap_start as *mut u8, HEAP_SIZE);
    }
}
