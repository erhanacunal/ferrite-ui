/// SD Card SPI driver — shares SPI0 bus with W25Q256 flash.
///
/// Pin assignment (configured in main.rs init_ports):
///   PA5  = CLK  (AF PP, shared)
///   PA6  = MISO (input, shared)
///   PA7  = MOSI (AF PP, shared)
///   PC13 = CS   (PP output, dedicated)
///
/// IMPORTANT: uses SPI **Mode 3** (CPOL=1, CPHA=1) — same as flash.
/// Reverse-engineered original firmware never switches CPOL/CPHA; it only
/// changes the baud-rate prescaler between init (~400kHz) and data
/// (~13.5MHz). Switching to Mode 0 corrupts subsequent flash reads.
///
/// Init sequence: set BR to /256, run CMD0/CMD8/ACMD41/CMD58, then switch
/// BR to /8 for data. On `release_bus()` the prescaler is restored to /8
/// (flash's default) — no CPOL/CPHA writes, no CTL0 rewrite.

// --- Hardware addresses ---

const GPIOC_BASE: u32 = 0x4001_1000;
const GPIO_BOP: u32 = GPIOC_BASE + 0x10;
const GPIO_BC: u32 = GPIOC_BASE + 0x14;

const SPI0_BASE: u32 = 0x4001_3000;
const SPI_CTL0: u32 = SPI0_BASE + 0x00;
const SPI_STAT: u32 = SPI0_BASE + 0x08;
const SPI_DATA: u32 = SPI0_BASE + 0x0C;

const SPI_FLAG_TBE: u32 = 1 << 1;   // Transmit buffer empty
const SPI_FLAG_RBNE: u32 = 1 << 0;  // Receive buffer not empty
const SPI_FLAG_TRANS: u32 = 1 << 7; // Bus busy

const SPI_CTL0_SPE: u32 = 1 << 6;
const SPI_CTL0_BR_MASK: u32 = 0b111 << 3;

// BR[2:0] values, already shifted into CTL0 bit position [5:3]
const BR_DIV8: u32 = 0b010 << 3;    // ~13.5MHz @ 108MHz PCLK — flash speed
const BR_DIV256: u32 = 0b111 << 3;  // ~421kHz — SD init clock

const CS_PIN: u32 = 13; // PC13

// --- SD SPI commands ---

const CMD0: u8 = 0;
const CMD8: u8 = 8;
const CMD16: u8 = 16;
const CMD17: u8 = 17;
const CMD55: u8 = 55;
const CMD58: u8 = 58;
const ACMD41: u8 = 41;

const DATA_START_TOKEN: u8 = 0xFE;

#[derive(Clone, Copy, PartialEq)]
pub enum CardType {
    SdV1,
    SdV2,
    SdHc,
}

pub struct SdCard {
    pub card_type: CardType,
}

#[derive(Clone, Copy)]
pub enum SdError {
    InitFailed,
    ReadError,
    Timeout,
}

impl SdCard {
    /// Quick presence check (CMD0 only). Restores flash baud on exit.
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

    /// Full init. Caller must call `release_bus()` before touching flash.
    pub fn init() -> Result<Self, SdError> {
        cs_high();

        // Slow clock for native-mode entry (≥74 cycles with CS high)
        set_br(BR_DIV256);
        for _ in 0..10 {
            spi_transfer(0xFF);
        }

        // CMD0 → R1 = 0x01 (idle)
        if sd_command(CMD0, 0, 0x95) != 0x01 {
            release_bus();
            return Err(SdError::InitFailed);
        }

        // CMD8 → voltage check (SD v2+)
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

        // ACMD41 loop
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

        // Determine addressing
        let card_type = if is_v2 {
            sd_command(CMD58, 0, 0xFF);
            let mut ocr = [0u8; 4];
            for b in ocr.iter_mut() {
                *b = spi_transfer(0xFF);
            }
            if ocr[0] & 0x40 != 0 { CardType::SdHc } else { CardType::SdV2 }
        } else {
            CardType::SdV1
        };

        if card_type != CardType::SdHc {
            sd_command(CMD16, 512, 0xFF);
        }

        // Switch to fast clock for data
        set_br(BR_DIV8);

        Ok(SdCard { card_type })
    }

    /// Read one 512B block.
    pub fn read_block(&self, block: u32, buf: &mut [u8; 512]) -> Result<(), SdError> {
        let addr = if self.card_type == CardType::SdHc { block } else { block * 512 };

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

    /// Restore prescaler to flash speed and raise CS. Flash ops are safe afterwards.
    pub fn release_bus(&self) {
        release_bus();
    }

    /// Switch prescaler back to /8 for SD data operations.
    pub fn acquire_bus(&self) {
        set_br(BR_DIV8);
    }
}

// --- SPI baud-rate switch (no CPOL/CPHA changes) ---

/// Mirror of the original firmware's SpiConfig: clear SPE, clear BR, set BR, set SPE.
fn set_br(br_bits: u32) {
    unsafe {
        let ctl0 = SPI_CTL0 as *mut u32;

        // Wait for any in-flight byte to finish before touching SPE
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

// --- SD command ---

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

// --- SPI byte transfer ---

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

// --- CS GPIO (PC13) ---

#[inline(always)]
fn cs_low() {
    unsafe { core::ptr::write_volatile(GPIO_BC as *mut u32, 1 << CS_PIN); }
}

#[inline(always)]
fn cs_high() {
    unsafe { core::ptr::write_volatile(GPIO_BOP as *mut u32, 1 << CS_PIN); }
}
