// BSP USART module — re-exports framework traits, provides epaper type aliases.

pub use ferrite_core::usart::*;

pub mod epaper;

pub use epaper::{dbg, dbg_u16, rx_clear, rx_has_data, rx_len, rx_read_byte};

pub type Usart = UsartImpl<epaper::EspUart>;

pub fn init() -> Usart {
    Usart::with_backend(epaper::EspUart::init())
}
