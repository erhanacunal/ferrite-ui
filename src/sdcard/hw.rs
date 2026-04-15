/// SD Card SPI driver — shares SPI0 bus with W25Q256 flash.
///
/// Pin assignment (configured in main.rs init_ports):
///   PA5  = CLK  (AF PP, shared)
///   PA6  = MISO (input, shared)
///   PA7  = MOSI (AF PP, shared)
///   PC13 = CS   (PP output, dedicated)
///
/// IMPORTANT: uses SPI **Mode 3** (CPOL=1, CPHA=1) — same as flash.
/// Only the baud-rate prescaler changes between init (~400kHz) and data
/// (~13.5MHz). Switching to Mode 0 corrupts subsequent flash reads.

use super::{CardType, SdCardBackend, SdError};

const GPIOC_BASE: u32 = 0x4001_1000;
const GPIO_BOP: u32 = GPIOC_BASE + 0x10;
const GPIO_BC: u32 = GPIOC_BASE + 0x14;

const SPI0_BASE: u32 = 0x4001_3000;
const SPI_CTL0: u32 = SPI0_BASE + 0x00;
const SPI_STAT: u32 = SPI0_BASE + 0x08;
const SPI_DATA: u32 = SPI0_BASE + 0x0C;

const SPI_FLAG_TBE: u32 = 1 << 1;
const SPI_FLAG_RBNE: u32 = 1 << 0;
const SPI_FLAG_TRANS: u32 = 1 << 7;

const SPI_CTL0_SPE: u32 = 1 << 6;
const SPI_CTL0_BR_MASK: u32 = 0b111 << 3;

const BR_DIV8: u32 = 0b010 << 3;
const BR_DIV256: u32 = 0b111 << 3;

const CS_PIN: u32 = 13;

const CMD0: u8 = 0;
const CMD8: u8 = 8;
const CMD16: u8 = 16;
const CMD17: u8 = 17;
const CMD55: u8 = 55;
const CMD58: u8 = 58;
const ACMD41: u8 = 41;

const DATA_START_TOKEN: u8 = 0xFE;

pub struct SpiSd {
    card_type: CardType,
}

pub fn probe() -> bool {
    cs_high();
    set_br(BR_DIV256);
    for _ in 0..10 {
        spi_transfer(0xFF);
    }
    let r1 = sd_command(CMD0, 0x00000000, 0x95);
    release_bus();
    r1 == 0x01
}

impl SpiSd {
    pub fn init() -> Result<Self, SdError> {
        cs_high();

        set_br(BR_DIV256);
        for _ in 0..10 {
            spi_transfer(0xFF);
        }

        if sd_command(CMD0, 0, 0x95) != 0x01 {
            release_bus();
            return Err(SdError::InitFailed);
        }

        let r1 = sd_command(CMD8, 0x0000_01AA, 0x87);
        let is_v2 = r1 == 0x01;
        if is_v2 {
            let mut r7 = [0u8; 4];
            for b in r7.iter_mut() {
                *b = spi_transfer(0xFF);
            }
            if r7[2] != 0x01 || r7[3] != 0xAA {
                release_bus();
                return Err(SdError::InitFailed);
            }
        }

        let hcs = if is_v2 { 0x4000_0000 } else { 0 };
        let mut attempts = 0u16;
        loop {
            sd_command(CMD55, 0, 0xFF);
            if sd_command(ACMD41, hcs, 0xFF) == 0x00 {
                break;
            }
            attempts += 1;
            if attempts > 1000 {
                release_bus();
                return Err(SdError::Timeout);
            }
        }

        let card_type = if is_v2 {
            sd_command(CMD58, 0, 0xFF);
            let mut ocr = [0u8; 4];
            for b in ocr.iter_mut() {
                *b = spi_transfer(0xFF);
            }
            if ocr[0] & 0x40 != 0 {
                CardType::SdHc
            } else {
                CardType::SdV2
            }
        } else {
            CardType::SdV1
        };

        if card_type != CardType::SdHc {
            sd_command(CMD16, 512, 0xFF);
        }

        set_br(BR_DIV8);

        Ok(SpiSd { card_type })
    }
}

impl SdCardBackend for SpiSd {
    fn card_type(&self) -> CardType {
        self.card_type
    }

    fn read_block(&self, block: u32, buf: &mut [u8; 512]) -> Result<(), SdError> {
        let addr = if self.card_type == CardType::SdHc {
            block
        } else {
            block * 512
        };

        if sd_command(CMD17, addr, 0xFF) != 0x00 {
            return Err(SdError::ReadError);
        }

        let mut attempts = 0u16;
        loop {
            if spi_transfer(0xFF) == DATA_START_TOKEN {
                break;
            }
            attempts += 1;
            if attempts > 10000 {
                return Err(SdError::Timeout);
            }
        }

        for b in buf.iter_mut() {
            *b = spi_transfer(0xFF);
        }
        spi_transfer(0xFF);
        spi_transfer(0xFF);

        Ok(())
    }

    fn release_bus(&self) {
        release_bus();
    }

    fn acquire_bus(&self) {
        set_br(BR_DIV8);
    }
}

fn set_br(br_bits: u32) {
    unsafe {
        let ctl0 = SPI_CTL0 as *mut u32;

        let stat = SPI_STAT as *const u32;
        while core::ptr::read_volatile(stat) & SPI_FLAG_TRANS != 0 {}

        let mut v = core::ptr::read_volatile(ctl0);
        v &= !SPI_CTL0_SPE;
        core::ptr::write_volatile(ctl0, v);

        v &= !SPI_CTL0_BR_MASK;
        v |= br_bits & SPI_CTL0_BR_MASK;
        core::ptr::write_volatile(ctl0, v);

        v |= SPI_CTL0_SPE;
        core::ptr::write_volatile(ctl0, v);
    }
}

fn release_bus() {
    cs_high();
    set_br(BR_DIV8);
}

fn sd_command(cmd: u8, arg: u32, crc: u8) -> u8 {
    cs_high();
    spi_transfer(0xFF);
    cs_low();

    spi_transfer(0x40 | cmd);
    spi_transfer((arg >> 24) as u8);
    spi_transfer((arg >> 16) as u8);
    spi_transfer((arg >> 8) as u8);
    spi_transfer(arg as u8);
    spi_transfer(crc);

    let mut r1: u8 = 0xFF;
    for _ in 0..10 {
        r1 = spi_transfer(0xFF);
        if r1 & 0x80 == 0 {
            break;
        }
    }
    r1
}

#[inline]
fn spi_transfer(byte: u8) -> u8 {
    unsafe {
        let stat = SPI_STAT as *const u32;
        let data = SPI_DATA as *mut u32;
        while core::ptr::read_volatile(stat) & SPI_FLAG_TBE == 0 {}
        core::ptr::write_volatile(data, byte as u32);
        while core::ptr::read_volatile(stat) & SPI_FLAG_RBNE == 0 {}
        core::ptr::read_volatile(data) as u8
    }
}

#[inline(always)]
fn cs_low() {
    unsafe {
        core::ptr::write_volatile(GPIO_BC as *mut u32, 1 << CS_PIN);
    }
}

#[inline(always)]
fn cs_high() {
    unsafe {
        core::ptr::write_volatile(GPIO_BOP as *mut u32, 1 << CS_PIN);
    }
}
