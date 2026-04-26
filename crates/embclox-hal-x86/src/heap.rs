use linked_list_allocator::LockedHeap;

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

const HEAP_SIZE: usize = 8 * 1024 * 1024; // 8 MiB max

#[unsafe(link_section = ".bss")]
static mut HEAP_AREA: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

/// Initialize the global heap allocator from a BSS-resident array.
/// Panics if `size` is 0 or exceeds the available heap area.
pub fn init(size: usize) {
    assert!(size > 0, "heap size must be > 0");
    assert!(size <= HEAP_SIZE, "heap size exceeds HEAP_AREA");
    let start = core::ptr::addr_of_mut!(HEAP_AREA);
    unsafe {
        ALLOCATOR.lock().init(start as *mut u8, size);
    }
}

/// Get the start address of the heap area (for kernel offset computation).
pub fn heap_start() -> usize {
    core::ptr::addr_of!(HEAP_AREA) as usize
}
