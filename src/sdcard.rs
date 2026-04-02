/// SD Card SPI driver — shares SPI0 bus with W25Q256 flash
///
/// Pin assignment:
///   PA5 = CLK  (shared with flash, SPI0_SCK)
///   PA6 = MISO (shared with flash, SPI0_MISO)
///   PA7 = MOSI (shared with flash, SPI0_MOSI)
///   PC13 = CS  (dedicated SD card chip select)
///
/// SD card uses SPI Mode 0 (CPOL=0, CPHA=0).
/// Flash uses Mode 3 (CPOL=1, CPHA=1).
/// We reconfigure SPI0 mode on acquire/release.
///
/// Init sequence: ≤400kHz clock, then switch to fast (~13.5MHz).

// --- Hardware addresses ---

const GPIOC_BASE: u32 = 0x4001_1000;
const GPIO_BOP: u32 = GPIOC_BASE + 0x10;
const GPIO_BC: u32 = GPIOC_BASE + 0x14;

const SPI0_BASE: u32 = 0x4001_3000;
const SPI_CTL0: u32 = SPI0_BASE + 0x00;
const SPI_STAT: u32 = SPI0_BASE + 0x08;
const SPI_DATA: u32 = SPI0_BASE + 0x0C;

const SPI_FLAG_TBE: u32 = 1 << 1;
const SPI_FLAG_RBNE: u32 = 1 << 0;

const CS_PIN: u32 = 13; // PC13

// --- SD SPI commands ---

const CMD0: u8 = 0;   // GO_IDLE_STATE
const CMD8: u8 = 8;   // SEND_IF_COND
const CMD16: u8 = 16; // SET_BLOCKLEN
const CMD17: u8 = 17; // READ_SINGLE_BLOCK
const CMD55: u8 = 55; // APP_CMD
const CMD58: u8 = 58; // READ_OCR
const ACMD41: u8 = 41; // SD_SEND_OP_COND

const DATA_START_TOKEN: u8 = 0xFE;

/// SD card type detected during init.
#[derive(Clone, Copy, PartialEq)]
pub enum CardType {
    /// SD v1 — byte addressing
    SdV1,
    /// SD v2 standard capacity — byte addressing
    SdV2,
    /// SDHC/SDXC — block (512B) addressing
    SdHc,
}

pub struct SdCard {
    pub card_type: CardType,
}

/// Error type for SD operations.
#[derive(Clone, Copy)]
pub enum SdError {
    InitFailed,
    ReadError,
    Timeout,
}

impl SdCard {
    /// Quick check if an SD card is present (CMD0 only).
    /// Returns true if card responds with idle state.
    /// Always restores SPI0 to flash mode before returning.
    pub fn probe() -> bool {
        cs_high();
        set_spi_mode0(0b111); // slow clock
        for _ in 0..10 {
            spi_transfer(0xFF);
        }
        let r1 = sd_command(CMD0, 0x00000000, 0x95);
        release_bus();
        r1 == 0x01
    }

    /// Initialize the SD card. Reconfigures SPI0 for SD mode.
    /// Call flash operations only after calling `release_bus()`.
    pub fn init() -> Result<Self, SdError> {
        // Deselect SD card first
        cs_high();

        // Switch SPI0 to Mode 0, slow clock (≤400kHz)
        // PSC = /256 → 108MHz/256 ≈ 422kHz
        set_spi_mode0(0b111); // prescaler /256

        // Send ≥74 clock pulses with CS high (card enters native mode)
        for _ in 0..10 {
            spi_transfer(0xFF);
        }

        // CMD0: GO_IDLE_STATE → expect R1 = 0x01 (idle)
        let r1 = sd_command(CMD0, 0x00000000, 0x95);
        if r1 != 0x01 {
            release_bus();
            return Err(SdError::InitFailed);
        }

        // CMD8: SEND_IF_COND (voltage check, SD v2+)
        let r1 = sd_command(CMD8, 0x000001AA, 0x87);
        let is_v2 = r1 == 0x01;

        if is_v2 {
            // Read 4-byte R7 response
            let mut r7 = [0u8; 4];
            for b in r7.iter_mut() {
                *b = spi_transfer(0xFF);
            }
            // Check echo pattern
            if r7[2] != 0x01 || r7[3] != 0xAA {
                release_bus();
                return Err(SdError::InitFailed);
            }
        }

        // ACMD41: SD_SEND_OP_COND (with HCS bit for v2)
        let hcs = if is_v2 { 0x40000000 } else { 0 };
        let mut attempts = 0u16;
        loop {
            sd_command(CMD55, 0, 0xFF);
            let r1 = sd_command(ACMD41, hcs, 0xFF);
            if r1 == 0x00 {
                break; // card ready
            }
            attempts += 1;
            if attempts > 1000 {
                release_bus();
                return Err(SdError::Timeout);
            }
        }

        // Determine card type
        let card_type = if is_v2 {
            // CMD58: READ_OCR to check CCS bit
            sd_command(CMD58, 0, 0xFF);
            let mut ocr = [0u8; 4];
            for b in ocr.iter_mut() {
                *b = spi_transfer(0xFF);
            }
            if ocr[0] & 0x40 != 0 {
                CardType::SdHc // block addressing
            } else {
                CardType::SdV2 // byte addressing
            }
        } else {
            CardType::SdV1
        };

        // Set block length to 512 for non-HC cards
        if card_type != CardType::SdHc {
            sd_command(CMD16, 512, 0xFF);
        }

        // Switch to fast clock: PSC /8 → ~13.5MHz
        set_spi_mode0(0b010);

        Ok(SdCard { card_type })
    }

    /// Read a 512-byte block from the SD card.
    /// `block` is the block number (for SDHC) or byte address / 512 (for SD v1/v2).
    pub fn read_block(&self, block: u32, buf: &mut [u8; 512]) -> Result<(), SdError> {
        let addr = if self.card_type == CardType::SdHc {
            block
        } else {
            block * 512
        };

        let r1 = sd_command(CMD17, addr, 0xFF);
        if r1 != 0x00 {
            return Err(SdError::ReadError);
        }

        // Wait for data start token
        let mut attempts = 0u16;
        loop {
            let b = spi_transfer(0xFF);
            if b == DATA_START_TOKEN {
                break;
            }
            attempts += 1;
            if attempts > 10000 {
                return Err(SdError::Timeout);
            }
        }

        // Read 512 bytes
        for b in buf.iter_mut() {
            *b = spi_transfer(0xFF);
        }

        // Discard 2-byte CRC
        spi_transfer(0xFF);
        spi_transfer(0xFF);

        Ok(())
    }

    /// Release the SPI bus — restore SPI0 to flash Mode 3 configuration.
    /// Must be called before any flash operations.
    pub fn release_bus(&self) {
        release_bus();
    }

    /// Acquire the SPI bus — switch SPI0 to SD Mode 0, fast clock.
    /// Must be called before SD operations if the bus was released.
    pub fn acquire_bus(&self) {
        set_spi_mode0(0b010); // PSC /8
    }
}

// --- SPI mode switching ---

/// Configure SPI0 for SD card: Mode 0 (CPOL=0, CPHA=0), given prescaler.
fn set_spi_mode0(psc: u32) {
    unsafe {
        let ctl0 = SPI_CTL0 as *mut u32;
        // Disable SPI
        core::ptr::write_volatile(ctl0, 0);
        // Mode 0: CKPH=0, CKPL=0
        let cfg: u32 = (0 << 0)      // CKPH: 1st edge
            | (0 << 1)               // CKPL: low idle
            | (1 << 2)               // MSTMOD: master
            | (psc << 3)             // PSC
            | (1 << 8)               // SWNSS high
            | (1 << 9);              // SWNSSEN
        core::ptr::write_volatile(ctl0, cfg);
        core::ptr::write_volatile(ctl0, cfg | (1 << 6)); // SPIEN
    }
}

/// Restore SPI0 to flash configuration: Mode 3 (CPOL=1, CPHA=1), PSC /8.
fn release_bus() {
    cs_high();
    unsafe {
        let ctl0 = SPI_CTL0 as *mut u32;
        core::ptr::write_volatile(ctl0, 0);
        let cfg: u32 = (1 << 0)      // CKPH: 2nd edge
            | (1 << 1)               // CKPL: high idle
            | (1 << 2)               // MSTMOD
            | (0b010 << 3)           // PSC /8
            | (1 << 8)               // SWNSS
            | (1 << 9);              // SWNSSEN
        core::ptr::write_volatile(ctl0, cfg);
        core::ptr::write_volatile(ctl0, cfg | (1 << 6));
    }
}

// --- SD command ---

/// Send an SD SPI command. Returns R1 response byte.
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

    // Wait for response (MSB = 0)
    let mut r1: u8 = 0xFF;
    for _ in 0..10 {
        r1 = spi_transfer(0xFF);
        if r1 & 0x80 == 0 {
            break;
        }
    }
    r1
}

// --- SPI transfer ---

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
