/// GD32F103 GPIO — LCD FPGA arayüzü
///
/// GPIOB[15:0] = 16-bit data bus
/// PA15 = LCD_CMD_DATA (1=data, 0=command)
/// PA12 = LCD_CLK (BOP=rising edge, BC=falling edge)
///
/// GPIO port ve pin konfigürasyonu init_ports() tarafından yapılır.

const GPIOA_BASE: u32 = 0x4001_0800;
const GPIOB_BASE: u32 = 0x4001_0C00;

const GPIO_BOP_OFFSET: u32 = 0x10;
const GPIO_BC_OFFSET: u32 = 0x14;

/// PA15: LCD_CMD_DATA (1=data, 0=command)
const LCD_CMD_DATA_PIN: u32 = 15;

/// PA12: LCD_CLK (BOP=rising, BC=falling)
const LCD_CLK_PIN: u32 = 12;

/// FPGA data bus: GPIOB[15:0]
pub struct Gpio;

impl Gpio {
    /// GPIO yapısını oluştur. Pin konfigürasyonu init_ports()'ta yapıldı.
    pub fn init() -> Self {
        Gpio
    }

    /// GPIOB[15:0]'a 16-bit veri yaz
    #[inline(always)]
    pub fn write_data_bus(&self, value: u16) {
        unsafe {
            let bop = (GPIOB_BASE + GPIO_BOP_OFFSET) as *mut u32;
            // Üst 16 bit = reset, alt 16 bit = set
            let bop_val = ((0xFFFF_u32) << 16) | (value as u32);
            core::ptr::write_volatile(bop, bop_val);
        }
    }

    /// PA15: command/data seç (0=command, 1=data)
    #[inline(always)]
    pub fn set_cmd_data(&self, is_data: bool) {
        unsafe {
            if is_data {
                let bop = (GPIOA_BASE + GPIO_BOP_OFFSET) as *mut u32;
                core::ptr::write_volatile(bop, 1 << LCD_CMD_DATA_PIN);
            } else {
                let bc = (GPIOA_BASE + GPIO_BC_OFFSET) as *mut u32;
                core::ptr::write_volatile(bc, 1 << LCD_CMD_DATA_PIN);
            }
        }
    }

    /// PA12: clock pulse — falling edge (BC) ile latch
    #[inline(always)]
    pub fn clock_pulse(&self) {
        unsafe {
            let bop = (GPIOA_BASE + GPIO_BOP_OFFSET) as *mut u32;
            core::ptr::write_volatile(bop, 1 << LCD_CLK_PIN);

            spin(2);

            let bc = (GPIOA_BASE + GPIO_BC_OFFSET) as *mut u32;
            core::ptr::write_volatile(bc, 1 << LCD_CLK_PIN);

            spin(2);
        }
    }
}

/// 108MHz CPU'yu FPGA'ya senkronize etmek için kısa gecikme
#[inline(always)]
fn spin(cycles: u32) {
    for _ in 0..cycles {
        cortex_m::asm::nop();
    }
}
