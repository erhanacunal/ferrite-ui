/*
 * ORIGIN is the load address every exynos_4412_rs application uses (bootcmd /
 * uart_boot: the image moves itself down here from where it was started).
 *
 * Layout: image + .bss (two 4 MiB framebuffers) + the stacks of the four cores,
 * then the TLSF heap. The heap may overlap uart_boot's staging area at
 * 0x50000000: the image has relocated itself before heap::init runs.
 * 1 GiB of DRAM ends at 0xBEA00000 (reserved top): 256 MiB heap is plenty for
 * the object pool (2 MB), JPEG decode buffers and the resource shadow.
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 288M
}

__heap_size = 256M;

/* embedded-sdmmc reads and writes 512-byte blocks on the stack (storage.rs). */
__svc_stack_size = 64K;
