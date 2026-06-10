/// W25Q256JVFQ 32MB SPI Flash Driver — Hardware SPI0 with DMA
///
/// Pin assignment (Ghidra RE):
///   PA4 = CS (chip select, active low, software GPIO)
///   PA5 = SCK (SPI0_CLK, AF push-pull)
///   PA6 = MISO (SPI0_MISO, input)
///   PA7 = MOSI (SPI0_MOSI, AF push-pull)
///
/// SPI0: APB2 bus, Mode 3 (CPOL=1, CPHA=1), MSB first, 8-bit frame
/// DMA0: CH1 = SPI0_RX, CH2 = SPI0_TX — used for bulk reads
/// 4-byte address mode: activated at init for full 32MB access.
///
/// GPIO and clock configuration is done in init_ports().
use super::FlashBackend;

// --- Hardware addresses ---

const GPIOA_BASE: u32 = 0x4001_0800;
const GPIO_BOP: u32 = GPIOA_BASE + 0x10;
const GPIO_BC: u32 = GPIOA_BASE + 0x14;

// SPI0 registers (APB2, base 0x4001_3000)
const SPI0_BASE: u32 = 0x4001_3000;
const SPI_CTL0: u32 = SPI0_BASE + 0x00;
const SPI_CTL1: u32 = SPI0_BASE + 0x04;
const SPI_STAT: u32 = SPI0_BASE + 0x08;
const SPI_DATA: u32 = SPI0_BASE + 0x0C;

// SPI_STAT bits
const SPI_FLAG_TBE: u32 = 1 << 1; // Transmit buffer empty
const SPI_FLAG_RBNE: u32 = 1 << 0; // Receive buffer not empty

// SPI_CTL1 bits
const SPI_CTL1_DMAREN: u32 = 1 << 0; // RX DMA enable
const SPI_CTL1_DMATEN: u32 = 1 << 1; // TX DMA enable

// --- DMA0 registers ---
// SPI0_RX = DMA0_CH1, SPI0_TX = DMA0_CH2

const RCU_AHBEN: u32 = 0x4002_1014;

const DMA0_BASE: u32 = 0x4002_0000;
const DMA_INTC: u32 = DMA0_BASE + 0x04; // Interrupt flag clear register
const DMA_INTF: u32 = DMA0_BASE + 0x00; // Interrupt flag register

// CH1 (SPI0_RX)
const DMA_CH1CTL: u32 = DMA0_BASE + 0x1C;
const DMA_CH1CNT: u32 = DMA0_BASE + 0x20;
const DMA_CH1PADDR: u32 = DMA0_BASE + 0x24;
const DMA_CH1MADDR: u32 = DMA0_BASE + 0x28;

// CH2 (SPI0_TX)
const DMA_CH2CTL: u32 = DMA0_BASE + 0x30;
const DMA_CH2CNT: u32 = DMA0_BASE + 0x34;
const DMA_CH2PADDR: u32 = DMA0_BASE + 0x38;
const DMA_CH2MADDR: u32 = DMA0_BASE + 0x3C;

// DMA_CHxCTL bits
const DMA_CHEN: u32 = 1 << 0;
const DMA_DIR: u32 = 1 << 4;
const DMA_MNAGA: u32 = 1 << 7;
const DMA_PRIO_HIGH: u32 = 0b10 << 12;

const DMA_INTF_FTFIF1: u32 = 1 << 5;
#[allow(dead_code)]
const DMA_INTF_FTFIF2: u32 = 1 << 9;
const DMA_INTC_CH1: u32 = 0xF << 4;
const DMA_INTC_CH2: u32 = 0xF << 8;

const DMA_THRESHOLD: usize = 32;

const CS_PIN: u32 = 4;

// --- W25Q256 commands ---

const CMD_READ_DATA: u8 = 0x03;
const CMD_PAGE_PROGRAM: u8 = 0x02;
const CMD_WRITE_ENABLE: u8 = 0x06;
#[allow(dead_code)]
const CMD_WRITE_DISABLE: u8 = 0x04;
const CMD_READ_STATUS1: u8 = 0x05;
const CMD_SECTOR_ERASE: u8 = 0x20;
const CMD_JEDEC_ID: u8 = 0x9F;
const CMD_ENTER_4BYTE: u8 = 0xB7;

const STATUS_BUSY: u8 = 0x01;
const PAGE_SIZE: usize = 256;

static DUMMY_BYTE: u8 = 0xFF;

#[derive(Clone)]
pub struct FpgaFlash {
    _private: (),
}

impl FpgaFlash {
    pub fn new() -> Self {
        Self { _private: () }
    }

    pub fn init() -> Self {
        unsafe {
            let val = core::ptr::read_volatile(RCU_AHBEN as *const u32);
            core::ptr::write_volatile(RCU_AHBEN as *mut u32, val | 1);

            let ctl0 = SPI_CTL0 as *mut u32;
            core::ptr::write_volatile(ctl0, 0);

            let cfg: u32 = (1 << 0) | (1 << 1) | (1 << 2) | (0b010 << 3) | (1 << 8) | (1 << 9);

            core::ptr::write_volatile(ctl0, cfg);
            core::ptr::write_volatile(ctl0, cfg | (1 << 6));
        }

        let flash = Self { _private: () };
        flash.command_only(CMD_ENTER_4BYTE);
        flash
    }

    fn dma_read(&self, buf: &mut [u8]) {
        unsafe {
            let stat = SPI_STAT as *const u32;
            let data_reg = SPI_DATA as *mut u32;
            if core::ptr::read_volatile(stat) & SPI_FLAG_RBNE != 0 {
                core::ptr::read_volatile(data_reg);
            }

            core::ptr::write_volatile(DMA_CH1CTL as *mut u32, 0);
            core::ptr::write_volatile(DMA_CH2CTL as *mut u32, 0);

            core::ptr::write_volatile(DMA_INTC as *mut u32, DMA_INTC_CH1 | DMA_INTC_CH2);

            let len = buf.len() as u32;

            core::ptr::write_volatile(DMA_CH1PADDR as *mut u32, SPI_DATA);
            core::ptr::write_volatile(DMA_CH1MADDR as *mut u32, buf.as_mut_ptr() as u32);
            core::ptr::write_volatile(DMA_CH1CNT as *mut u32, len);
            core::ptr::write_volatile(DMA_CH1CTL as *mut u32, DMA_MNAGA | DMA_PRIO_HIGH);

            core::ptr::write_volatile(DMA_CH2PADDR as *mut u32, SPI_DATA);
            core::ptr::write_volatile(DMA_CH2MADDR as *mut u32, &DUMMY_BYTE as *const u8 as u32);
            core::ptr::write_volatile(DMA_CH2CNT as *mut u32, len);
            core::ptr::write_volatile(DMA_CH2CTL as *mut u32, DMA_DIR | DMA_PRIO_HIGH);

            let ctl1 = SPI_CTL1 as *mut u32;
            core::ptr::write_volatile(ctl1, SPI_CTL1_DMAREN | SPI_CTL1_DMATEN);

            let ch1ctl = DMA_CH1CTL as *mut u32;
            let ch2ctl = DMA_CH2CTL as *mut u32;
            core::ptr::write_volatile(ch1ctl, core::ptr::read_volatile(ch1ctl) | DMA_CHEN);
            core::ptr::write_volatile(ch2ctl, core::ptr::read_volatile(ch2ctl) | DMA_CHEN);

            let intf = DMA_INTF as *const u32;
            while core::ptr::read_volatile(intf) & DMA_INTF_FTFIF1 == 0 {}

            core::ptr::write_volatile(ch1ctl, 0);
            core::ptr::write_volatile(ch2ctl, 0);
            core::ptr::write_volatile(ctl1, 0);
            core::ptr::write_volatile(DMA_INTC as *mut u32, DMA_INTC_CH1 | DMA_INTC_CH2);
        }
    }

    fn write_enable(&self) {
        self.command_only(CMD_WRITE_ENABLE);
    }

    pub fn command_only(&self, cmd: u8) {
        cs_low();
        spi_transfer(cmd);
        cs_high();
    }

    fn read_status(&self) -> u8 {
        cs_low();
        spi_transfer(CMD_READ_STATUS1);
        let status = spi_transfer(0xFF);
        cs_high();
        status
    }

    fn wait_busy(&self) {
        while self.read_status() & STATUS_BUSY != 0 {}
    }

    fn page_program(&self, addr: u32, data: &[u8]) {
        debug_assert!(data.len() <= PAGE_SIZE);

        self.write_enable();

        cs_low();
        spi_transfer(CMD_PAGE_PROGRAM);
        spi_write_addr(addr);

        for &byte in data {
            spi_transfer(byte);
        }
        cs_high();

        self.wait_busy();
    }
}

impl FlashBackend for FpgaFlash {
    fn read_id(&self) -> (u8, u8, u8) {
        cs_low();
        spi_transfer(CMD_JEDEC_ID);
        let mfr = spi_transfer(0xFF);
        let mem = spi_transfer(0xFF);
        let cap = spi_transfer(0xFF);
        cs_high();
        (mfr, mem, cap)
    }

    fn read(&self, addr: u32, buf: &mut [u8]) {
        if buf.is_empty() {
            return;
        }

        cs_low();
        spi_transfer(CMD_READ_DATA);
        spi_write_addr(addr);

        if buf.len() >= DMA_THRESHOLD {
            self.dma_read(buf);
        } else {
            for byte in buf.iter_mut() {
                *byte = spi_transfer(0xFF);
            }
        }

        cs_high();
    }

    fn erase_sector(&self, addr: u32) {
        self.write_enable();

        cs_low();
        spi_transfer(CMD_SECTOR_ERASE);
        spi_write_addr(addr);
        cs_high();

        self.wait_busy();
    }

    fn write(&self, mut addr: u32, mut data: &[u8]) {
        while !data.is_empty() {
            let page_offset = (addr & 0xFF) as usize;
            let page_remaining = PAGE_SIZE - page_offset;
            let chunk = if data.len() < page_remaining {
                data.len()
            } else {
                page_remaining
            };

            self.page_program(addr, &data[..chunk]);

            addr += chunk as u32;
            data = &data[chunk..];
        }
    }
}

// === Hardware SPI0 ===

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

#[inline]
fn spi_write_addr(addr: u32) {
    spi_transfer((addr >> 24) as u8);
    spi_transfer((addr >> 16) as u8);
    spi_transfer((addr >> 8) as u8);
    spi_transfer(addr as u8);
}

#[inline(always)]
fn cs_low() {
    unsafe {
        let bc = GPIO_BC as *mut u32;
        core::ptr::write_volatile(bc, 1 << CS_PIN);
    }
}

#[inline(always)]
fn cs_high() {
    unsafe {
        let bop = GPIO_BOP as *mut u32;
        core::ptr::write_volatile(bop, 1 << CS_PIN);
    }
}
