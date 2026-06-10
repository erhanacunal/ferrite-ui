/// Backend abstraction for SD card access over SPI.
/// BSPs provide the SPI driver; the framework provides SdCardImpl<B>.

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

#[cfg(feature = "mock")]
pub type SdCard = SdCardImpl<crate::mock::MockSdCard>;
