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

    /// Resize the RX ring to `bytes` (0 = keep the current size). Called once
    /// at boot from RES_SYSCFG. Returns false if the buffer could not be
    /// allocated (the old one stays in use). Backends with a fixed or
    /// host-side buffer keep the default, which ignores the request.
    fn set_rx_capacity(&self, bytes: usize) -> bool {
        let _ = bytes;
        true
    }

    /// `(bytes waiting, ring size)` of the RX ring, for diagnostics.
    /// `(0, 0)` = not reported by this backend.
    fn rx_stats(&self) -> (usize, usize) {
        (0, 0)
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

    #[inline]
    pub fn set_rx_capacity(&self, bytes: usize) -> bool {
        self.be.set_rx_capacity(bytes)
    }

    #[inline]
    pub fn rx_stats(&self) -> (usize, usize) {
        self.be.rx_stats()
    }

    /// The wrapped backend (tests read recorded bytes through it).
    #[cfg(test)]
    pub(crate) fn backend(&self) -> &B {
        &self.be
    }
}

#[cfg(feature = "mock")]
pub type Usart = UsartImpl<crate::mock::MockUsart>;
