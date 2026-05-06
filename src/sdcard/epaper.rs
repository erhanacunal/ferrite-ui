//! SD card SPI bit-bang backend for LilyGo T5-ePaper-S3 (ESP32-S3).
//!
//! Pin assignment:
//!   GPIO11 = SCLK
//!   GPIO15 = MOSI
//!   GPIO16 = MISO
//!   GPIO42 = CS  (active low)
//!
//! Protocol: SPI Mode 0 (CPOL=0, CPHA=0).
//! SD init sequence: CMD0 → CMD8 → ACMD41 loop → CMD58 for SDHC detection.
//! Block read: CMD17, wait for 0xFE token, read 512 bytes + 2-byte CRC.

#![cfg(feature = "epaper")]

use super::{CardType, SdCardBackend, SdError};

// --- ESP32-S3 GPIO register addresses ---

// Bank 0: GPIOs 0-31
const GPIO_OUT_W1TS: *mut u32  = 0x6000_4008u32 as *mut u32;
const GPIO_OUT_W1TC: *mut u32  = 0x6000_400Cu32 as *mut u32;
const GPIO_IN:       *const u32 = 0x6000_403Cu32 as *const u32;

// Bank 1: GPIOs 32-48
const GPIO_OUT1_W1TS: *mut u32  = 0x6000_4014u32 as *mut u32;
const GPIO_OUT1_W1TC: *mut u32  = 0x6000_4018u32 as *mut u32;

// --- Pin bit masks ---

/// SCLK = GPIO11 → bank0 bit 11
const SCLK_BIT:  u32 = 1 << 11;
/// MOSI = GPIO15 → bank0 bit 15
const MOSI_BIT:  u32 = 1 << 15;
/// MISO = GPIO16 → bank0 bit 16
const MISO_BIT:  u32 = 1 << 16;
/// CS   = GPIO42 → bank1 bit (42 - 32) = 10
const CS_BIT1:   u32 = 1 << 10;

// --- SD command bytes ---

const CMD0:  u8 = 0;
const CMD8:  u8 = 8;
const CMD16: u8 = 16;
const CMD17: u8 = 17;
const CMD55: u8 = 55;
const CMD58: u8 = 58;
const ACMD41: u8 = 41;

const DATA_START_TOKEN: u8 = 0xFE;

// --- Low-level GPIO ---

#[inline(always)]
unsafe fn sclk_high() { core::ptr::write_volatile(GPIO_OUT_W1TS, SCLK_BIT); }
#[inline(always)]
unsafe fn sclk_low()  { core::ptr::write_volatile(GPIO_OUT_W1TC, SCLK_BIT); }

#[inline(always)]
unsafe fn mosi_high() { core::ptr::write_volatile(GPIO_OUT_W1TS, MOSI_BIT); }
#[inline(always)]
unsafe fn mosi_low()  { core::ptr::write_volatile(GPIO_OUT_W1TC, MOSI_BIT); }

#[inline(always)]
unsafe fn cs_high()   { core::ptr::write_volatile(GPIO_OUT1_W1TS, CS_BIT1); }
#[inline(always)]
unsafe fn cs_low()    { core::ptr::write_volatile(GPIO_OUT1_W1TC, CS_BIT1); }

#[inline(always)]
unsafe fn miso_read() -> bool {
    core::ptr::read_volatile(GPIO_IN) & MISO_BIT != 0
}

// --- SPI Mode 0 bit-bang byte transfer ---
//
// CPOL=0 (clock idles low), CPHA=0 (data captured on rising edge).
// MSB first.

#[inline]
unsafe fn spi_transfer(byte: u8) -> u8 {
    let mut tx = byte;
    let mut rx: u8 = 0;
    for _ in 0..8 {
        // Set MOSI before rising edge.
        if tx & 0x80 != 0 { mosi_high(); } else { mosi_low(); }
        tx <<= 1;

        // Small hold time — at 240 MHz a few nops are enough for ~1 MHz.
        sclk_high();
        // Sample MISO on rising edge.
        rx = (rx << 1) | (miso_read() as u8);

        sclk_low();
    }
    rx
}

// --- SD command / response ---

/// Send a 6-byte SD command frame and return R1 response.
unsafe fn sd_command(cmd: u8, arg: u32, crc: u8) -> u8 {
    cs_high();
    spi_transfer(0xFF);
    cs_low();

    spi_transfer(0x40 | cmd);
    spi_transfer((arg >> 24) as u8);
    spi_transfer((arg >> 16) as u8);
    spi_transfer((arg >> 8)  as u8);
    spi_transfer(arg         as u8);
    spi_transfer(crc);

    // Wait for a valid (non-0xFF) response byte — up to 10 attempts.
    let mut r1: u8 = 0xFF;
    for _ in 0..10 {
        r1 = spi_transfer(0xFF);
        if r1 & 0x80 == 0 {
            break;
        }
    }
    r1
}

unsafe fn release_bus_impl() {
    cs_high();
    // Clock out 8 dummy bits so the card releases the bus.
    spi_transfer(0xFF);
}

// --- Public struct ---

pub struct EspSdCard {
    card_type: CardType,
}

/// Quick presence check — sends CMD0 and returns whether a card responds.
pub fn probe() -> bool {
    unsafe {
        cs_high();
        // Send 80 clocks (10 × 0xFF) to put the card in SPI mode.
        for _ in 0..10 {
            spi_transfer(0xFF);
        }
        let r1 = sd_command(CMD0, 0, 0x95);
        release_bus_impl();
        r1 == 0x01
    }
}

impl EspSdCard {
    /// Full SD init sequence. Returns `Err` if the card does not respond
    /// within the allotted retries.
    pub fn init() -> Result<Self, SdError> {
        unsafe {
            cs_high();

            // 80+ init clocks.
            for _ in 0..10 {
                spi_transfer(0xFF);
            }

            // CMD0: software reset → expect R1 = 0x01 (idle).
            if sd_command(CMD0, 0, 0x95) != 0x01 {
                release_bus_impl();
                return Err(SdError::InitFailed);
            }

            // CMD8: voltage/interface check (SD v2 only).
            let r1 = sd_command(CMD8, 0x0000_01AA, 0x87);
            let is_v2 = r1 == 0x01;
            if is_v2 {
                let mut r7 = [0u8; 4];
                for b in r7.iter_mut() { *b = spi_transfer(0xFF); }
                if r7[2] != 0x01 || r7[3] != 0xAA {
                    release_bus_impl();
                    return Err(SdError::InitFailed);
                }
            }

            // ACMD41 loop: wait for card to leave idle state.
            let hcs: u32 = if is_v2 { 0x4000_0000 } else { 0 };
            let mut attempts: u16 = 0;
            loop {
                sd_command(CMD55, 0, 0xFF);
                if sd_command(ACMD41, hcs, 0xFF) == 0x00 {
                    break;
                }
                attempts += 1;
                if attempts > 2000 {
                    release_bus_impl();
                    return Err(SdError::Timeout);
                }
                // Brief spin between retries.
                for _ in 0..240u32 {}
            }

            // CMD58: read OCR to detect SDHC.
            let card_type = if is_v2 {
                sd_command(CMD58, 0, 0xFF);
                let mut ocr = [0u8; 4];
                for b in ocr.iter_mut() { *b = spi_transfer(0xFF); }
                if ocr[0] & 0x40 != 0 { CardType::SdHc } else { CardType::SdV2 }
            } else {
                CardType::SdV1
            };

            // CMD16: set block length to 512 for non-SDHC cards.
            if card_type != CardType::SdHc {
                sd_command(CMD16, 512, 0xFF);
            }

            Ok(EspSdCard { card_type })
        }
    }
}

impl SdCardBackend for EspSdCard {
    fn card_type(&self) -> CardType {
        self.card_type
    }

    fn read_block(&self, block: u32, buf: &mut [u8; 512]) -> Result<(), SdError> {
        // SDHC uses block addressing; SD v1/v2 uses byte addressing.
        let addr = if self.card_type == CardType::SdHc {
            block
        } else {
            block * 512
        };

        unsafe {
            if sd_command(CMD17, addr, 0xFF) != 0x00 {
                return Err(SdError::ReadError);
            }

            // Wait for data-start token (0xFE).
            let mut attempts: u16 = 0;
            loop {
                if spi_transfer(0xFF) == DATA_START_TOKEN {
                    break;
                }
                attempts += 1;
                if attempts > 10_000 {
                    return Err(SdError::Timeout);
                }
            }

            // Read 512 data bytes.
            for b in buf.iter_mut() {
                *b = spi_transfer(0xFF);
            }
            // Discard 2-byte CRC.
            spi_transfer(0xFF);
            spi_transfer(0xFF);
        }

        Ok(())
    }

    fn release_bus(&self) {
        unsafe { release_bus_impl(); }
    }

    fn acquire_bus(&self) {
        // Bit-bang has no baud-rate register to restore; just assert CS low
        // indirectly on the next command.  A dummy clock cycle ensures the
        // bus is in a known state.
        unsafe { cs_high(); }
    }
}
