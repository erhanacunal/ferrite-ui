ENTRY(Reset_Go)

/* The NAND loader copies this image to SDRAM 0x0 and jumps there.  While it
 * loads, the loader itself runs at 0x700000, so the image must stay below. */
MEMORY
{
    RAM (rwx) : ORIGIN = 0x00000000, LENGTH = 0x00700000
}

SECTIONS
{
    . = ORIGIN(RAM);

    .text :
    {
        PROVIDE(__image_start = .);
        KEEP(*(.vectors));   /* exception vectors at 0x0 */
        . = ALIGN(4);
        *(.init);
        . = ALIGN(4);
        *(.text);
        *(.text.*);
        . = ALIGN(4);
        *(.rodata);
        *(.rodata.*);
        . = ALIGN(4);
    } > RAM

    .data :
    {
        *(.data)
        *(.data.*)
        . = ALIGN(4);
        PROVIDE(__image_end = .);
    } > RAM

    /* Not part of the image (objcopy -O binary stops at .data); zeroed by
     * rust_main.  Holds the frame buffers, so it is large. */
    .bss (NOLOAD) :
    {
        . = ALIGN(32);
        PROVIDE(__bss_start__ = .);
        *(.bss)
        *(.bss.*)
        *(COMMON)
        . = ALIGN(32);
        PROVIDE(__bss_end__ = .);
    } > RAM

    /* Heap: from the end of .bss up to just below the exception stacks that
     * startup.s puts at the top of the 8 MB SDRAM (lowest is the UND stack,
     * 0x7BEC00-0x7BF000).  It runs past the 0x700000 limit above: the loader
     * sitting there is no longer needed once we run. */
    . = ALIGN(32);
    PROVIDE(__heap_start__ = .);
    PROVIDE(__heap_end__ = 0x007BE000);

    /* ARM unwind tables: panic=abort, nothing unwinds */
    /DISCARD/ :
    {
        *(.ARM.exidx*)
        *(.ARM.extab*)
    }
}
