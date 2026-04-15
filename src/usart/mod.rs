#[cfg(not(feature = "host"))]
pub mod hw;
#[cfg(feature = "host")]
pub mod sim;

#[cfg(not(feature = "host"))]
pub use hw::{dbg, dbg_u16, rx_clear, rx_has_data, rx_len, rx_read_byte};
#[cfg(feature = "host")]
pub use sim::{dbg, dbg_u16, rx_clear, rx_has_data, rx_len, rx_read_byte};

pub trait UsartBackend {
    fn write_byte(&self, byte: u8);
    fn flush(&self);
}

pub struct UsartImpl<B: UsartBackend> {
    be: B,
}

impl<B: UsartBackend> UsartImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be }
    }

    #[inline]
    pub fn write_byte(&self, byte: u8) {
        self.be.write_byte(byte)
    }

    pub fn write(&self, data: &[u8]) {
        for &b in data {
            self.be.write_byte(b);
        }
    }

    #[inline]
    pub fn flush(&self) {
        self.be.flush()
    }
}

#[cfg(not(feature = "host"))]
pub type Usart = UsartImpl<hw::Hw>;

#[cfg(feature = "host")]
pub type Usart = UsartImpl<sim::Stdio>;

#[cfg(not(feature = "host"))]
impl Usart {
    pub fn init() -> Self {
        Self::with_backend(hw::Hw::init())
    }
}

#[cfg(feature = "host")]
impl Usart {
    pub fn init() -> Self {
        Self::with_backend(sim::Stdio::new())
    }
}
