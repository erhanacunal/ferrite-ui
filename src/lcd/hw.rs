use super::LcdBackend;
use crate::gpio::Gpio;

/// FPGA command codes (Ghidra reverse engineering)
const CMD_FRONT_SWAP: u16 = 0x04;
const CMD_BACK_SELECT: u16 = 0x05;
const CMD_Y_START: u16 = 0x02;
const CMD_X_START: u16 = 0x03;
const CMD_Y_END: u16 = 0x06;
const CMD_X_END: u16 = 0x07;
const CMD_PIXEL_WRITE: u16 = 0x0F;

pub struct FpgaLcd {
    gpio: Gpio,
    /// Front buffer index (what FPGA displays). Tracks CMD4 value.
    lcd4: u8,
    /// Back buffer index (what CPU writes to). Tracks CMD5 value.
    lcd5: u8,
}

impl FpgaLcd {
    pub fn new(gpio: Gpio) -> Self {
        Self {
            gpio,
            lcd4: 0,
            lcd5: 0,
        }
    }

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
}

impl LcdBackend for FpgaLcd {
    fn begin_frame(&mut self) {
        if self.lcd4 == self.lcd5 {
            self.lcd5 ^= 1;
            self.send_command(CMD_BACK_SELECT);
            self.send_data(self.lcd5 as u16);
        }
    }

    fn end_frame(&mut self) {
        self.lcd4 = self.lcd5;
        self.send_command(CMD_FRONT_SWAP);
        self.send_data(self.lcd4 as u16);
    }

    fn back_buf(&self) -> u8 {
        self.lcd5
    }

    #[inline(always)]
    fn send_command(&self, cmd: u16) {
        self.gpio.set_cmd_data(false);
        self.gpio.write_data_bus(cmd);
        self.gpio.clock_pulse();
    }

    #[inline(always)]
    fn send_data(&self, data: u16) {
        self.gpio.set_cmd_data(true);
        self.gpio.write_data_bus(data);
        self.gpio.clock_pulse();
    }

    #[inline]
    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        self.set_address(x, y, x + w - 1, y + h - 1);
    }

    #[inline(always)]
    fn write_pixel(&self, color: u16) {
        self.send_data(color);
    }

    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        let x2 = x + w - 1;
        let y2 = y + h - 1;
        self.set_address(x, y, x2, y2);
        let pixel_count = w as u32 * h as u32;
        for _ in 0..pixel_count {
            self.send_data(color);
        }
    }

    #[inline]
    fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        if x < super::WIDTH && y < super::HEIGHT {
            self.set_address(x, y, x, y);
            self.send_data(color);
        }
    }
}
