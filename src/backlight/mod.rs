#[cfg(not(feature = "host"))]
pub mod hw;
#[cfg(feature = "host")]
pub mod sim;

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

#[cfg(not(feature = "host"))]
pub type Backlight = BacklightImpl<hw::Pwm>;

#[cfg(feature = "host")]
pub type Backlight = BacklightImpl<sim::StubBacklight>;

#[cfg(not(feature = "host"))]
impl Backlight {
    pub fn init() -> Self {
        Self::with_backend(hw::Pwm::init())
    }
}

#[cfg(feature = "host")]
impl Backlight {
    pub fn new() -> Self {
        Self::with_backend(sim::StubBacklight::new())
    }
}
