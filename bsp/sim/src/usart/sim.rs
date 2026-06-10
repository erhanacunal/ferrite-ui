use std::io::Write;

use super::UsartBackend;

/// Stdout-backed sim USART. TX bytes are written to stdout; RX is unimplemented
/// for now (`rx_read_byte()` always returns `None`). Wire TCP in later if the
/// companion protobuf tool is ever pointed at the sim.
pub struct Stdio;

impl Stdio {
    pub fn new() -> Self {
        Stdio
    }
}

impl UsartBackend for Stdio {
    fn write_byte(&self, byte: u8) {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = lock.write_all(&[byte]);
    }

    fn flush(&self) {
        let _ = std::io::stdout().flush();
    }
}

// --- Free-function RX stubs (match firmware API shape) ---

pub fn rx_has_data() -> bool {
    false
}

pub fn rx_read_byte() -> Option<u8> {
    None
}

pub fn rx_len() -> u8 {
    0
}

pub fn rx_clear() {}

pub fn dbg(data: &[u8]) {
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = lock.write_all(data);
}

pub fn dbg_u16(val: u16) {
    let s = val.to_string();
    dbg(s.as_bytes());
}
