use linked_list_allocator::LockedHeap;

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

const HEAP_SIZE: usize = 4 * 1024 * 1024; // 4 MiB max

#[unsafe(link_section = ".bss")]
static mut HEAP_AREA: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

pub fn init(size: usize) {
    assert!(size > 0, "heap size must be > 0");
    assert!(size <= HEAP_SIZE, "heap size exceeds HEAP_AREA");
    let start = core::ptr::addr_of_mut!(HEAP_AREA);
    unsafe {
        ALLOCATOR.lock().init(start as *mut u8, size);
    }
}

pub fn heap_start() -> usize {
    core::ptr::addr_of!(HEAP_AREA) as usize
}
