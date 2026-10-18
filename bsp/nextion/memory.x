/* GD32F103RBT6: 128KB Flash, 20KB RAM */
MEMORY
{
    FLASH : ORIGIN = 0x08000000, LENGTH = 128K
    RAM   : ORIGIN = 0x20000000, LENGTH = 20K
}

/* Stack reserve at the top of RAM. The heap takes all RAM between the end of
 * .bss/.uninit (__sheap) and the bottom of this reserve (_heap_end) — there is
 * no fixed heap size. Raise this if the stack overflows into the heap. */
_stack_size = 3K;
_heap_end = _stack_start - _stack_size;
ASSERT(_heap_end >= __sheap + 8K, "
ERROR(ferrite): RAM overcommitted — .bss + _stack_size leave less than 8 KB of heap");
