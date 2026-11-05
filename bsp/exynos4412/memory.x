/*
 * ORIGIN is the load address every exynos_4412_rs application uses (bootcmd /
 * uart_boot: the image moves itself down here from where it was started).
 *
 * Layout: image + .bss (two 4 MiB framebuffers) + the stacks of the four cores,
 * then the TLSF heap. The heap may overlap uart_boot's staging area at
 * 0x50000000: the image has relocated itself before heap::init runs.
 * 1 GiB of DRAM ends at 0xBEA00000 (reserved top): a 512 MiB heap holds the
 * resource shadow (64 MB), the object pool (16 MB default, platform.rs),
 * cached font bitmaps, program code and later JPEG decode buffers.
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 544M
}

__heap_size = 512M;

/* embedded-sdmmc reads and writes 512-byte blocks on the stack (storage.rs). */
__svc_stack_size = 64K;
