pub use ferrite_core::usart::*;

pub mod hw;

pub type Usart = UsartImpl<hw::Hw>;

pub fn init() -> Usart {
    Usart::with_backend(hw::Hw::init())
}
