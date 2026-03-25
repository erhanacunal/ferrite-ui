use crate::gpio::Gpio;

/// FPGA komut kodları (Ghidra reverse engineering)
const CMD_Y_START: u16 = 0x02;
const CMD_X_START: u16 = 0x03;
const CMD_Y_END: u16 = 0x06;
const CMD_X_END: u16 = 0x07;
const CMD_PIXEL_WRITE: u16 = 0x0F;

/// LCD boyutları
pub const WIDTH: u16 = 800;
pub const HEIGHT: u16 = 480;

pub struct Lcd {
    gpio: Gpio,
}

impl Lcd {
    pub fn new(gpio: Gpio) -> Self {
        Self { gpio }
    }

    /// FPGA'ya komut gönder
    #[inline(always)]
    fn send_command(&self, cmd: u16) {
        self.gpio.set_cmd_data(false); // command mode
        self.gpio.write_data_bus(cmd);
        self.gpio.clock_pulse();
    }

    /// FPGA'ya data gönder
    #[inline(always)]
    fn send_data(&self, data: u16) {
        self.gpio.set_cmd_data(true); // data mode
        self.gpio.write_data_bus(data);
        self.gpio.clock_pulse();
    }

    /// Çizim alanını ayarla (x1, y1) → (x2, y2)
    fn set_address(&self, x1: u16, y1: u16, x2: u16, y2: u16) {
        self.send_command(CMD_X_START);
        self.send_data(x1);

        self.send_command(CMD_Y_START);
        self.send_data(y1);

        self.send_command(CMD_X_END);
        self.send_data(x2);

        self.send_command(CMD_Y_END);
        self.send_data(y2);

        self.send_command(CMD_PIXEL_WRITE);
    }

    /// Piksel yazma penceresi aç. Sonraki `write_pixel` çağrıları bu alana yazar.
    /// Font rendering gibi tek tek piksel yazan kodlar için.
    #[inline]
    pub fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        self.set_address(x, y, x + w - 1, y + h - 1);
    }

    /// Tek piksel yaz (begin_pixels sonrası). Piksel sırayla yazılır (soldan sağa, yukarıdan aşağı).
    #[inline(always)]
    pub fn write_pixel(&self, color: u16) {
        self.send_data(color);
    }

    /// Dikdörtgen doldur — RGB565 renk
    pub fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        let x2 = x + w - 1;
        let y2 = y + h - 1;

        self.set_address(x, y, x2, y2);

        let pixel_count = w as u32 * h as u32;
        for _ in 0..pixel_count {
            self.send_data(color);
        }
    }
}
