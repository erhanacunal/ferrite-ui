//! SD card on the SIC's SD port 0 (GPE2–GPE7: DAT0–3, CMD, CLK), 4-bit bus,
//! single-block DMA reads, polled.
//!
//! The command sequence and register use follow Nuvoton's N3290x SD loader
//! (`Loader/SDLoader/sd.c`, N32905 Non-OS BSP). SD cards only (SDSC v1/v2 and
//! SDHC/SDXC); MMC is not supported. There is no card-detect line: a missing
//! card shows up as a response timeout, so `init` doubles as the probe.
//!
//! The SD host shares the FMI and its DMA channel with the NAND driver. Each
//! operation selects its own function in FMICR first (the NAND driver does the
//! same for SM), and both only run from the main loop, so they don't collide.
//! The FMI itself is never reset here: that would drop the NAND controller's
//! configuration.

use ferrite_core::sdcard::{CardType, SdCardBackend, SdCardImpl, SdError};
use n32903_rs::cache;
use n32903_rs::clock::{self, ClockNode};
use n32903_rs::mmu::UNCACHED_ALIAS;
use n32903_rs::registers::*;

use crate::systick::millis;

pub type SdCard = SdCardImpl<SicSd>;

// --- FMI / SD host registers (W55FA93_reg.h) ---

const REG_FMIIER: u32 = FMI_BA + 0x004;
const REG_SDCR: u32 = FMI_BA + 0x020;
const REG_SDARG: u32 = FMI_BA + 0x024;
const REG_SDIER: u32 = FMI_BA + 0x028;
const REG_SDISR: u32 = FMI_BA + 0x02C;
const REG_SDRSP0: u32 = FMI_BA + 0x030;
const REG_SDRSP1: u32 = FMI_BA + 0x034;
const REG_SDBLEN: u32 = FMI_BA + 0x038;
const REG_SDTMOUT: u32 = FMI_BA + 0x03C;

const FMI_SD_EN: u32 = 1 << 1;

// SDCR
const SDCR_CO_EN: u32 = 1 << 0; // command out
const SDCR_RI_EN: u32 = 1 << 1; // 48-bit response in
const SDCR_DI_EN: u32 = 1 << 2; // data in
const SDCR_R2_EN: u32 = 1 << 4; // 136-bit response in
const SDCR_74CLK_OE: u32 = 1 << 5;
const SDCR_8CLK_OE: u32 = 1 << 6;
const SDCR_SWRST: u32 = 1 << 14;
const SDCR_DBW: u32 = 1 << 15; // 4-bit data bus
const SDCR_SDPORT: u32 = 3 << 29; // 0 = port 0
/// Bits kept when issuing a command (vendor masks: CMD_CODE and the
/// enables are replaced; block count too for data commands).
const SDCR_KEEP_CMD: u32 = 0xFFFF_C080;
const SDCR_KEEP_DATA: u32 = 0xFF00_C080;

// SDISR
const SDISR_BLKD_IF: u32 = 1 << 0;
const SDISR_CRC_IF: u32 = 1 << 1;
const SDISR_CRC_7: u32 = 1 << 2; // 1 = response CRC ok
const SDISR_CRC_16: u32 = 1 << 3; // 1 = data CRC ok
const SDISR_DATA0: u32 = 1 << 7; // DAT0 level (low = card busy)

// CLKDIV2: SD clock = source / (N0 + 1) / (N1 + 1)
const SD_N1: u32 = 0xFF00_0000;
const SD_S: u32 = 0x0018_0000;
const SD_N0: u32 = 0x0007_0000;
const SD_S_UPLL: u32 = 3 << 19;

/// SD port 0 pins GPE2–GPE7 to their SD function.
const GPEFUN_SD0_MASK: u32 = 0x0000_FFF0;
const GPEFUN_SD0: u32 = 0x0000_AAA0;

const INIT_KHZ: u32 = 200;
/// Default-speed bus (≤ 25 MHz), the vendor loader's rate.
const TRANSFER_KHZ: u32 = 24_000;

/// Command → response (64 clocks, < 1 ms even at the init clock).
const CMD_TIMEOUT_MS: u32 = 50;
/// Read access time: ≤ 100 ms for SDHC, plus margin.
const DATA_TIMEOUT_MS: u32 = 250;
const BUSY_TIMEOUT_MS: u32 = 500;
/// ACMD41 power-up loop: the spec allows the card 1 s.
const READY_TIMEOUT_MS: u32 = 1000;

/// 512-byte DMA target. The CPU only reads it through the uncached alias;
/// a D-cache clean before every transfer keeps stale dirty lines (zeroed
/// .bss) from being written back over fresh DMA data.
#[repr(C, align(32))]
struct DmaBuf([u8; 512]);
static mut DMA_BUF: DmaBuf = DmaBuf([0; 512]);

pub struct SicSd {
    kind: CardType,
}

#[inline(always)]
fn rd(reg: u32) -> u32 {
    unsafe { reg_read32(reg) }
}

#[inline(always)]
fn wr(reg: u32, v: u32) {
    unsafe { reg_write32(reg, v) }
}

fn timed_out(start: u32, ms: u32) -> bool {
    millis().wrapping_sub(start) > ms
}

/// Reset the SD engine after a timeout, keeping the bus width and port.
fn reset_engine() {
    let keep = rd(REG_SDCR) & (SDCR_DBW | SDCR_SDPORT);
    wr(REG_SDCR, rd(REG_SDCR) | SDCR_SWRST);
    for _ in 0..100_000 {
        if rd(REG_SDCR) & SDCR_SWRST == 0 {
            break;
        }
    }
    wr(REG_SDCR, (rd(REG_SDCR) & !(SDCR_DBW | SDCR_SDPORT)) | keep);
    wr(REG_SDISR, 0xFFFF);
}

/// Wait until the controller clears `bits` in SDCR.
fn wait_sdcr(bits: u32, timeout_ms: u32) -> Result<(), SdError> {
    let start = millis();
    while rd(REG_SDCR) & bits != 0 {
        if timed_out(start, timeout_ms) {
            reset_engine();
            return Err(SdError::Timeout);
        }
    }
    Ok(())
}

/// Issue command `cmd` with the SDCR enables in `flags` and wait for them.
fn send(cmd: u32, arg: u32, flags: u32) -> Result<(), SdError> {
    wr(REG_SDARG, arg);
    wr(REG_SDCR, (rd(REG_SDCR) & SDCR_KEEP_CMD) | (cmd << 8) | flags);
    wait_sdcr(flags, CMD_TIMEOUT_MS)
}

/// Command with an R1/R6/R7 response (CRC-7 checked).
fn cmd_r1(cmd: u32, arg: u32) -> Result<(), SdError> {
    send(cmd, arg, SDCR_CO_EN | SDCR_RI_EN)?;
    if rd(REG_SDISR) & SDISR_CRC_7 == 0 {
        wr(REG_SDISR, SDISR_CRC_IF);
        return Err(SdError::ReadError);
    }
    Ok(())
}

/// Command with an R3 response (OCR; carries no valid CRC).
fn cmd_r3(cmd: u32, arg: u32) -> Result<(), SdError> {
    send(cmd, arg, SDCR_CO_EN | SDCR_RI_EN)?;
    wr(REG_SDISR, SDISR_CRC_IF);
    Ok(())
}

/// SDRSP0 holds response bits 47..16: OCR / card status bits 31..8 sit in
/// its low 24 bits (OCR busy = bit 23, CCS = bit 22).
fn rsp0() -> u32 {
    rd(REG_SDRSP0)
}

/// Clock the card until it releases DAT0 (end of busy).
fn wait_busy() -> Result<(), SdError> {
    let start = millis();
    loop {
        wr(REG_SDCR, rd(REG_SDCR) | SDCR_8CLK_OE);
        wait_sdcr(SDCR_8CLK_OE, CMD_TIMEOUT_MS)?;
        if rd(REG_SDISR) & SDISR_DATA0 != 0 {
            return Ok(());
        }
        if timed_out(start, BUSY_TIMEOUT_MS) {
            return Err(SdError::Timeout);
        }
    }
}

/// SD clock from UPLL, at or below `khz`.
fn set_clock(khz: u32) {
    let src = unsafe { clock::get_clock(ClockNode::UPll) };
    let total = src.div_ceil(khz).max(2);
    // Pre-divide by at least 2, like the vendor code, and as little as fits.
    let n0 = (2..=8).find(|n| total.div_ceil(*n) <= 256).unwrap_or(8);
    let n1 = total.div_ceil(n0).clamp(1, 256);
    let v = rd(REG_CLKDIV2) & !(SD_N1 | SD_S | SD_N0);
    wr(REG_CLKDIV2, v | ((n1 - 1) << 24) | SD_S_UPLL | ((n0 - 1) << 16));
    for _ in 0..1000 {
        core::hint::spin_loop();
    }
}

/// Bring up the SD host and the card. `Err` when no (usable) card is in the
/// slot; costs ~100 ms then.
pub fn init() -> Result<SdCard, SdError> {
    // Host on: engine clocks, DMA channel, SD function, engine reset, pins.
    wr(REG_AHBCLK, rd(REG_AHBCLK) | SIC_CKE | SD_CKE);
    wr(REG_DMACCSR, rd(REG_DMACCSR) | DMAC_EN);
    wr(REG_FMICR, FMI_SD_EN);
    wr(REG_SDCR, rd(REG_SDCR) & !0xFF);
    reset_engine();
    wr(REG_GPEFUN, (rd(REG_GPEFUN) & !GPEFUN_SD0_MASK) | GPEFUN_SD0);
    wr(REG_SDCR, rd(REG_SDCR) & !(SDCR_SDPORT | SDCR_DBW));
    wr(REG_FMIIER, 0);
    wr(REG_SDIER, 0x0000_0A00); // vendor default, interrupts unused
    wr(REG_SDISR, 0xFFFF);
    wr(REG_SDTMOUT, 0x00FF_FFFF);

    set_clock(INIT_KHZ);
    wr(REG_SDCR, rd(REG_SDCR) | SDCR_74CLK_OE);
    wait_sdcr(SDCR_74CLK_OE, CMD_TIMEOUT_MS)?;

    send(0, 0, SDCR_CO_EN)?; // GO_IDLE_STATE
    for _ in 0..1000 {
        core::hint::spin_loop();
    }

    // CMD8 (2.7–3.6 V, check pattern 0x55) answers only on v2 cards.
    let v2 = cmd_r1(8, 0x155).is_ok() && rd(REG_SDRSP1) & 0xFF == 0x55;
    let hcs = if v2 { 0x4000_0000 } else { 0 };

    // ACMD41 until the card leaves power-up. A v1 card that ignores CMD55
    // is MMC (unsupported) or nothing at all.
    let start = millis();
    loop {
        cmd_r1(55, 0).map_err(|_| SdError::InitFailed)?;
        cmd_r3(41, hcs | 0x00FF_8000)?;
        if rsp0() & 0x0080_0000 != 0 {
            break;
        }
        if timed_out(start, READY_TIMEOUT_MS) {
            return Err(SdError::InitFailed);
        }
    }
    let kind = match (v2, rsp0() & 0x0040_0000 != 0) {
        (true, true) => CardType::SdHc,
        (true, false) => CardType::SdV2,
        _ => CardType::SdV1,
    };

    send(2, 0, SDCR_CO_EN | SDCR_R2_EN)?; // ALL_SEND_CID (CID unused)
    cmd_r1(3, 0)?; // SEND_RELATIVE_ADDR
    let rca = (rsp0() << 8) & 0xFFFF_0000;

    set_clock(TRANSFER_KHZ);
    cmd_r1(7, rca)?; // select; it stays selected
    wait_busy()?;
    cmd_r1(55, rca)?;
    cmd_r1(6, 2)?; // 4-bit bus
    wr(REG_SDCR, rd(REG_SDCR) | SDCR_DBW);
    cmd_r1(16, 512)?; // block length (fixed on SDHC, harmless)
    wr(REG_SDISR, 0xFFFF);

    Ok(SdCard::with_backend(SicSd { kind }))
}

impl SdCardBackend for SicSd {
    fn card_type(&self) -> CardType {
        self.kind
    }

    fn read_block(&self, block: u32, buf: &mut [u8; 512]) -> Result<(), SdError> {
        let addr = if self.kind == CardType::SdHc { block } else { block * 512 };
        let dma = (&raw mut DMA_BUF) as u32;

        unsafe { cache::dcache_clean() };
        wr(REG_FMICR, FMI_SD_EN); // the NAND driver may have selected SM
        let start = millis();
        while rd(REG_DMACCSR) & FMI_BUSY != 0 {
            if timed_out(start, CMD_TIMEOUT_MS) {
                return Err(SdError::Timeout);
            }
        }
        wr(REG_DMACCSR, rd(REG_DMACCSR) | DMAC_EN);
        wr(REG_SDBLEN, 511);
        wr(REG_SDARG, addr);
        wr(REG_DMACSAR, dma & !UNCACHED_ALIAS);
        wr(REG_SDISR, SDISR_BLKD_IF | SDISR_CRC_IF);

        // READ_SINGLE_BLOCK: command, response and one data block.
        wr(
            REG_SDCR,
            (rd(REG_SDCR) & SDCR_KEEP_DATA)
                | (1 << 16)
                | (17 << 8)
                | SDCR_CO_EN
                | SDCR_RI_EN
                | SDCR_DI_EN,
        );
        let start = millis();
        while rd(REG_SDISR) & SDISR_BLKD_IF == 0 || rd(REG_SDCR) & SDCR_DI_EN != 0 {
            if timed_out(start, DATA_TIMEOUT_MS) {
                reset_engine();
                return Err(SdError::Timeout);
            }
        }
        wr(REG_SDISR, SDISR_BLKD_IF);

        let isr = rd(REG_SDISR);
        if isr & SDISR_CRC_7 == 0 || isr & SDISR_CRC_16 == 0 {
            wr(REG_SDISR, SDISR_CRC_IF);
            return Err(SdError::ReadError);
        }

        let src = (dma | UNCACHED_ALIAS) as *const u8;
        unsafe { core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), 512) };
        Ok(())
    }

    fn release_bus(&self) {}
    fn acquire_bus(&self) {}
}
