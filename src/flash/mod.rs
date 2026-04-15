#[cfg(not(feature = "host"))]
pub mod hw;
#[cfg(feature = "host")]
pub mod sim;

/// Backend abstraction for the 32MB external flash.
/// Hardware backend talks to W25Q256 over SPI0+DMA; host backend is file-backed.
pub trait FlashBackend {
    fn read(&self, addr: u32, buf: &mut [u8]);
    fn write(&self, addr: u32, data: &[u8]);
    fn erase_sector(&self, addr: u32);
    fn read_id(&self) -> (u8, u8, u8);
}

pub struct FlashImpl<B: FlashBackend> {
    be: B,
}

impl<B: FlashBackend> FlashImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be }
    }

    #[inline]
    pub fn read(&self, addr: u32, buf: &mut [u8]) {
        self.be.read(addr, buf)
    }

    #[inline]
    pub fn write(&self, addr: u32, data: &[u8]) {
        self.be.write(addr, data)
    }

    #[inline]
    pub fn erase_sector(&self, addr: u32) {
        self.be.erase_sector(addr)
    }

    #[inline]
    pub fn read_id(&self) -> (u8, u8, u8) {
        self.be.read_id()
    }
}

// --- Type alias: pick backend by build feature ---

#[cfg(not(feature = "host"))]
pub type Flash = FlashImpl<hw::FpgaFlash>;

#[cfg(feature = "host")]
pub type Flash = FlashImpl<sim::FileFlash>;

#[cfg(not(feature = "host"))]
impl Flash {
    /// Create a handle assuming hardware was already initialized.
    pub fn new() -> Self {
        Self::with_backend(hw::FpgaFlash::new())
    }

    /// Full hardware initialization: SPI0 + DMA0 + 4-byte address mode.
    pub fn init() -> Self {
        Self::with_backend(hw::FpgaFlash::init())
    }
}

#[cfg(feature = "host")]
impl Flash {
    /// Build a sim flash backed by the given 32MB image (padded/truncated as needed).
    pub fn from_image(bytes: std::vec::Vec<u8>) -> Self {
        Self::with_backend(sim::FileFlash::new(bytes))
    }

    /// Construct an empty sim flash. Matches the firmware `Flash::new()` surface
    /// used by internal VM default-init paths.
    pub fn new() -> Self {
        Self::with_backend(sim::FileFlash::new(std::vec::Vec::new()))
    }
}
