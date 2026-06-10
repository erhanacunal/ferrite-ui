OUTPUT_FORMAT("elf32-littlearm", "elf32-littlearm", "elf32-littlearm")
OUTPUT_ARCH(arm)
ENTRY(_start)

MEMORY
{
    ram : org = 0x80000000, len = 32M
}

SECTIONS
{
    . = ALIGN(4);

    .text :
    {
        /* ── SPL: Exception vectors & BROM header ───────────────────── */
        PROVIDE(__image_start = .);        

        /* ── SPL: Critical early-init functions (clock, dram, uart) ─── */
        *(.text.spl)
        /* ORIGIN(ram) is a constant (0x80000000), not a symbol — lld resolves correctly */
        __spl_size = ABSOLUTE(. - ORIGIN(ram));

        /* ── Remaining code ─────────────────────────────────────────── */
        *(.text*)
        *(.init.text)
        *(.exit.text)
        *(.glue*)
        *(.note.gnu.build-id)
    } > ram

    . = ALIGN(8);

    .rodata ALIGN(8) :
    {
        PROVIDE(__rodata_start = .);
        *(.rodata*)
        *(.eh_frame)
        PROVIDE(__rodata_end = .);
    } > ram

    .data ALIGN(8) :
    {
        PROVIDE(__data_start = .);
        *(.data*)
        . = ALIGN(8);
        PROVIDE(__data_end = .);
        PROVIDE(__image_end = .);
    } > ram

    .bss ALIGN(8) (NOLOAD) :
    {
        PROVIDE(__bss_start = .);
        *(.bss*)
        *(COMMON)
        . = ALIGN(8);
        PROVIDE(__bss_end = .);
    } > ram

    _end = .;

    /* ARM unwind tables — bare-metal, no C++ exceptions, discard entirely */
    /DISCARD/ :
    {
        *(.ARM.exidx*)
        *(.ARM.extab*)
    }

    /* Debug / misc sections (discard) */
    .stab           0 : { *(.stab) }
    .stabstr        0 : { *(.stabstr) }
    .stab.excl      0 : { *(.stab.excl) }
    .stab.exclstr   0 : { *(.stab.exclstr) }
    .stab.index     0 : { *(.stab.index) }
    .stab.indexstr  0 : { *(.stab.indexstr) }
    .comment        0 : { *(.comment) }
    .debug_abbrev   0 : { *(.debug_abbrev) }
    .debug_info     0 : { *(.debug_info) }
    .debug_line     0 : { *(.debug_line) }
    .debug_pubnames 0 : { *(.debug_pubnames) }
    .debug_aranges  0 : { *(.debug_aranges) }
}
