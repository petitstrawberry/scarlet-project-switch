OUTPUT_ARCH(aarch64)
ENTRY(_start)
SECTIONS {
    . = 0x80200000;
    .head : { KEEP(*(.head)) }
    .text : { *(.text .text.*) }
    .rodata : { *(.rodata .rodata.*) }
    .data : { *(.data .data.*) }
    . = ALIGN(16);
    .bss (NOLOAD) : { __bss_start = .; *(.bss .bss.*) . = ALIGN(16); . += 65536; __stack_top = .; __bss_end = .; }
    __image_size = . - 0x80200000;
    ASSERT(. < 0x8d000000, "probe overlaps handed-off DTB")
    /DISCARD/ : { *(.eh_frame*) *(.comment) }
}
