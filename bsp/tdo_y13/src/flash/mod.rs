//! Flash backend for TDO Y13 — W25Q128 / GD25Q128 SPI NOR flash (16 MB).
//!
//! Uses the f1c100s HAL `SpiFlash` driver over SPI0. The flash is external
//! (not memory-mapped) so all reads/writes go through SPI commands.

use alloc::boxed::Box;
use ferrite_core::flash::FlashBackend;

/// Hardware backend wrapping the HAL SPI flash driver.
#[derive(Clone)]
pub struct HwFlash;

/// Global SPI flash instance — initialized by `init()`, never freed.
/// SAFETY: only the UI thread accesses this. SPI is interrupt-protected.
static mut FLASH: Option<f1c100s::spi_flash::SpiFlash<'static>> = None;

impl FlashBackend for HwFlash {
    fn read(&self, addr: u32, buf: &mut [u8]) {
        let addr = addr & 0x00FF_FFFF;
        unsafe { (&raw const FLASH).as_ref().unwrap().as_ref().unwrap().read(addr, buf).ok(); }
    }

    fn write(&self, addr: u32, data: &[u8]) {
        let addr = addr & 0x00FF_FFFF;
        let flash = unsafe { (&raw const FLASH).as_ref().unwrap().as_ref().unwrap() };
        let mut offset = 0usize;
        while offset < data.len() {
            let page_off = (addr as usize + offset) & 0xFF;
            let chunk = (256 - page_off).min(data.len() - offset);
            flash.page_program(addr + offset as u32, &data[offset..offset + chunk]).ok();
            offset += chunk;
        }
    }

    fn erase_sector(&self, addr: u32) {
        let addr = addr & 0x00FF_FFFF;
        unsafe { (&raw const FLASH).as_ref().unwrap().as_ref().unwrap().sector_erase_4k(addr).ok(); }
    }

    fn read_id(&self) -> (u8, u8, u8) {
        match unsafe { (&raw const FLASH).as_ref().unwrap().as_ref().unwrap().read_jedec_id() } {
            Ok(id) => (id[0], id[1], id[2]),
            Err(_) => (0, 0, 0),
        }
    }
}

/// Initialize the SPI0 bus and SpiFlash driver. Must be called once at boot.
///
/// # Safety
/// Must be called before any flash operations. Requires SPI0 GPIO mux to be
/// configured (PC0=CS, PC1=CLK, PC2=MISO, PC3=MOSI by default).
pub unsafe fn init() {
    use f1c100s::spi::{self, BitOrder, ChipSelect, SpiMode};
    use f1c100s::spi_flash::SpiFlash;

    let spi = spi::spi0();
    spi.configure(SpiMode::Mode0, BitOrder::MsbFirst, ChipSelect::Ss0, 10_000_000);

    let spi_ref: &'static _ = Box::leak(Box::new(spi));
    unsafe { FLASH = Some(SpiFlash::new(spi_ref)) };
}

/// Return a new `HwFlash` instance. Must call `init()` first.
pub fn new() -> HwFlash {
    HwFlash
}
