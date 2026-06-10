/// Backend abstraction for display backlight control.
/// Hardware backends drive PWM; sim is a stub.

pub trait BacklightBackend {
    fn set_brightness(&self, percent: u8);
    fn brightness(&self) -> u8;
}

pub struct BacklightImpl<B: BacklightBackend> {
    be: B,
}

impl<B: BacklightBackend> BacklightImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be }
    }

    #[inline]
    pub fn set_brightness(&self, percent: u8) {
        self.be.set_brightness(percent)
    }

    #[inline]
    pub fn brightness(&self) -> u8 {
        self.be.brightness()
    }
}

#[cfg(feature = "mock")]
pub type Backlight = BacklightImpl<crate::mock::MockBacklight>;
