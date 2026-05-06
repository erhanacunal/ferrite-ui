#[cfg(feature = "firmware")]
pub mod hw;
#[cfg(feature = "host")]
pub mod sim;
#[cfg(feature = "epaper")]
pub mod epaper;

#[derive(Clone, Copy, PartialEq)]
pub enum CardType {
    SdV1,
    SdV2,
    SdHc,
}

#[derive(Clone, Copy)]
pub enum SdError {
    InitFailed,
    ReadError,
    Timeout,
}

pub trait SdCardBackend {
    fn card_type(&self) -> CardType;
    fn read_block(&self, block: u32, buf: &mut [u8; 512]) -> Result<(), SdError>;
    fn release_bus(&self);
    fn acquire_bus(&self);
}

pub struct SdCardImpl<B: SdCardBackend> {
    be: B,
}

impl<B: SdCardBackend> SdCardImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be }
    }

    #[inline]
    pub fn card_type(&self) -> CardType {
        self.be.card_type()
    }

    #[inline]
    pub fn read_block(&self, block: u32, buf: &mut [u8; 512]) -> Result<(), SdError> {
        self.be.read_block(block, buf)
    }

    #[inline]
    pub fn release_bus(&self) {
        self.be.release_bus()
    }

    #[inline]
    pub fn acquire_bus(&self) {
        self.be.acquire_bus()
    }
}

#[cfg(feature = "firmware")]
pub type SdCard = SdCardImpl<hw::SpiSd>;

#[cfg(feature = "host")]
pub type SdCard = SdCardImpl<sim::FileSd>;

#[cfg(feature = "epaper")]
pub type SdCard = SdCardImpl<epaper::EspSdCard>;

#[cfg(feature = "firmware")]
impl SdCard {
    /// Quick presence check (CMD0 only). Restores flash baud on exit.
    pub fn probe() -> bool {
        hw::probe()
    }

    /// Full init over SPI0. Caller must call `release_bus()` before touching flash.
    pub fn init() -> Result<Self, SdError> {
        hw::SpiSd::init().map(Self::with_backend)
    }
}

#[cfg(feature = "host")]
impl SdCard {
    /// Always true in the sim — a "card" is a file.
    pub fn probe() -> bool {
        true
    }

    /// Build a sim card from a raw image file.
    pub fn init_from_image(bytes: std::vec::Vec<u8>) -> Result<Self, SdError> {
        Ok(Self::with_backend(sim::FileSd::new(bytes)))
    }
}

#[cfg(feature = "epaper")]
impl SdCard {
    pub fn probe() -> bool {
        epaper::probe()
    }

    pub fn init() -> Result<Self, SdError> {
        epaper::EspSdCard::init().map(Self::with_backend)
    }
}
