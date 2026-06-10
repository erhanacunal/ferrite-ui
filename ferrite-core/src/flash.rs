/// Backend abstraction for external flash storage (W25Q256 or equivalent).
/// Hardware backends talk to SPI flash; the framework provides FlashImpl<B>.

/// Note: this trait stays object-safe (`Box<dyn FlashBackend>` lives in the VM),
/// so `Clone` is *not* a supertrait. Backends are stateless MMIO/SPI drivers
/// (pins are global), so concrete backends derive `Clone` and `Platform::FlashB`
/// requires it — letting the VM take its own handle for flash-execution cache
/// refills while the framework keeps `ctx.flash` for resource reads.
pub trait FlashBackend {
    fn read(&self, addr: u32, buf: &mut [u8]);
    fn write(&self, addr: u32, data: &[u8]);
    fn erase_sector(&self, addr: u32);
    fn read_id(&self) -> (u8, u8, u8);
}

#[derive(Clone)]
pub struct FlashImpl<B: FlashBackend> {
    be: B,
}

impl<B: FlashBackend> FlashImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be }
    }

    /// Consume the wrapper, returning the raw backend. Used to type-erase a flash
    /// handle (e.g. `Box<dyn FlashBackend>`) without exposing the private field.
    pub fn into_backend(self) -> B {
        self.be
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

#[cfg(feature = "mock")]
pub type Flash = FlashImpl<crate::mock::MockFlash>;
