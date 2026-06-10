/// Backend abstraction for USART/serial communication.
/// Hardware backends provide byte-level TX; the framework provides UsartImpl<B>.

pub trait UsartBackend {
    fn write_byte(&self, byte: u8);
    fn flush(&self);

    /// Pop one received byte from the backend's RX ring buffer, if any.
    /// Hardware backends drain an interrupt-filled ring; stubs return `None`.
    fn rx_read_byte(&self) -> Option<u8> {
        None
    }
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

    #[inline]
    pub fn rx_read_byte(&self) -> Option<u8> {
        self.be.rx_read_byte()
    }
}

#[cfg(feature = "mock")]
pub type Usart = UsartImpl<crate::mock::MockUsart>;
