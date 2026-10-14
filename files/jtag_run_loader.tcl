# Run the NAND loader from SDRAM over JTAG, without touching NAND.
#
# Start it from RAM, then flash it properly from its own menu
# (flash_tool.py ... writeldr nandloader.bin writes the IBR spare marks).
#
# No reset here: on this board a reset disables JTAG.  Power up with the
# JTAG switch pressed, connect, then:
#
#   openocd -f <your interface/target cfg> -f jtag_run_loader.tcl \
#           -c "init; run_loader target/arm926ej-s/release/n32903_nand_loader.bin"
#
# or, from a telnet session on an already running OpenOCD:
#   source jtag_run_loader.tcl
#   run_loader target/arm926ej-s/release/n32903_nand_loader.bin
#
# Load the raw binary (no IBR header): it is linked to run at 0x700000.

proc run_loader {bin} {
    if {[load_loader $bin]} {
        resume
        echo "Loader started - the UART should show the boot countdown."
    }
}

# Everything up to the jump: the CPU is left halted at 0x700000, so
# breakpoints can be set before "resume".
proc load_loader {bin} {
    halt
    wait_halt

    set cpsr [lindex [reg cpsr] 2]
    set pc   [lindex [reg pc] 2]
    echo "Halted at pc=$pc cpsr=$cpsr"

    # SVC mode, IRQ/FIQ masked, ARM state.  The boot ROM is Thumb code;
    # resuming with the T bit still set runs our ARM code as Thumb.
    reg cpsr 0xD3

    # MMU and caches off, caches and TLBs invalidated
    set sctlr [arm mrc 15 0 1 0 0]
    echo [format "SCTLR=0x%08x" $sctlr]
    arm mcr 15 0 1 0 0 [expr {$sctlr & ~0x1005}]
    arm mcr 15 0 7 7 0 0
    arm mcr 15 0 8 7 0 0

    # We run from 0x700000, so SDRAM must work.  With the JTAG switch the
    # boot stops in on-chip SRAM (pc=0xff00xxxx) before DDR is set up.
    if {![sdram_ok]} {
        echo "SDRAM not working - initialising DDR over JTAG..."
        init_ddr
        if {![sdram_ok]} {
            echo "ERROR: SDRAM still not working after init_ddr."
            return 0
        }
    }
    echo "SDRAM OK"

    # verify_image runs a checksum stub on the target; give it 4 KB of
    # internal SRAM (the loader doesn't need SRAM contents at start).
    [target current] configure -work-area-phys 0xFF001000 -work-area-size 0x1000 -work-area-backup 0
    load_image $bin 0x700000 bin
    verify_image $bin 0x700000 bin

    reg pc 0x700000
    return 1
}

# Write two patterns to SDRAM and read them back.
proc sdram_ok {} {
    foreach {addr val} {0x700000 0x12345678 0x7FFFFC 0xA5A55A5A} {
        mww $addr $val
        set got [lindex [read_memory $addr 32 1] 0]
        if {$got != $val} {
            echo [format "  0x%08x: wrote 0x%08x, read 0x%08x" $addr $val $got]
            return 0
        }
    }
    return 1
}

# Same register sequence as sys_init_ddr() in src/clock.rs, done from the
# debugger.  The busy-wait polls become short sleeps.
proc init_ddr {} {
    mww 0xB0000224 0x00000E6E    ;# UPLLCON
    mww 0xB0000220 0x1008CE6E    ;# APLLCON
    mww 0xB000020C 0x00000019    ;# CLKDIV0
    sleep 10                     ;# let the PLLs lock
    mww 0xB0003030 0x00001010    ;# DQSODS
    mww 0xB0003010 0x00000005
    mww 0xB0003004 0x00000021    ;# SDCMD: precharge all
    mww 0xB0003004 0x00000023
    mww 0xB0003004 0x00000027
    sleep 1
    mww 0xB000301C 0x00001002    ;# SDEMR
    mww 0xB0003018 0x00000122    ;# SDMR (DLL reset)
    mww 0xB0003004 0x00000027
    sleep 1
    mww 0xB0003004 0x0000002B    ;# auto refresh x2
    sleep 1
    mww 0xB0003004 0x0000002B
    sleep 1
    mww 0xB0003018 0x00000022    ;# SDMR
    sleep 1
    mww 0xB0003004 0x00000020    ;# normal operation
    mww 0xB0003034 0x00AAAA00
    mww 0xB0003008 0x000080C0    ;# SDREF
    mww 0xB00000A0 0x00000000
}

# If nothing appears on the UART: halt and run this to see where the
# loader ended up (pc in 0x700000-0x706000 = running/stuck in our code;
# pc in 0x0-0x1c or garbage = it took an exception).
proc where {} {
    halt
    wait_halt
    echo "pc=[lindex [reg pc] 2] cpsr=[lindex [reg cpsr] 2] lr=[lindex [reg lr] 2]"
}

# Halt on the exception itself instead of after the CPU has wandered off.
# Run before run_loader; when the target halts on its own, run abort_info.
proc catch_aborts {} {
    arm9 vector_catch undef pabt dabt
}

# Where the abort came from.  Data abort: faulting instruction = lr_abt - 8,
# accessed address = FAR.  Prefetch abort: faulting instruction = lr_abt - 4.
proc abort_info {} {
    echo "pc=[lindex [reg pc] 2] cpsr=[lindex [reg cpsr] 2]"
    echo "lr_abt=[lindex [reg lr_abt] 2] spsr_abt=[lindex [reg spsr_abt] 2]"
    echo "lr_und=[lindex [reg lr_und] 2] spsr_und=[lindex [reg spsr_und] 2]"
    echo "sp_svc=[lindex [reg sp_svc] 2] lr_svc=[lindex [reg lr_svc] 2]"
    echo [format "FSR=0x%08x FAR=0x%08x" [arm mrc 15 0 5 0 0] [arm mrc 15 0 6 0 0]]
}
