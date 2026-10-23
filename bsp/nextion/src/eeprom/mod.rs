pub use ferrite_core::eeprom::*;

pub mod hw;

pub type Eeprom = EepromImpl<hw::At24c08>;

pub fn init() -> Eeprom {
    Eeprom::with_backend(hw::At24c08::init())
}
