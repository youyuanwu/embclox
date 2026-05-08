/* Linker script for Limine boot protocol (x86_64) */
ENTRY(kmain)

PHDRS {
    text    PT_LOAD FLAGS(5);  /* r-x */
    rodata  PT_LOAD FLAGS(4);  /* r-- */
    data    PT_LOAD FLAGS(6);  /* rw- */
}

SECTIONS {
    . = 0xffffffff80000000;

    .text : {
        *(.text .text.*)
    } :text

    . = ALIGN(4096);

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(4096);

    .data : {
        *(.data .data.*)

        /* Limine requests sections */
        KEEP(*(.requests_start_marker))
        KEEP(*(.requests))
        KEEP(*(.requests_end_marker))
    } :data

    . = ALIGN(4096);

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note*)
    }
}
