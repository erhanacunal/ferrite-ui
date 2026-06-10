// BSP USART module (sim) — re-exports framework traits + free-fn ctor.

pub use ferrite_core::usart::*;

pub mod sim;

pub use sim::{dbg, dbg_u16, rx_clear, rx_has_data, rx_len, rx_read_byte};

pub type Usart = UsartImpl<sim::Stdio>;

pub fn init() -> Usart {
    Usart::with_backend(sim::Stdio::new())
}
