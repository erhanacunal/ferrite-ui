/// AT8563T Real-Time Clock — I2C bit-bang driver
///
/// PCF8563 compatible. I2C address: 0x51 (7-bit).
///
/// Pin assignment (Ghidra RE):
///   PC0 = SCL (open-drain, 50MHz)
///   PC1 = SDA (open-drain, 50MHz)
///
/// GPIO configured as open-drain output by init_ports() (CTL0 = 0x77).
/// Both lines idle HIGH (external pull-ups). Initial state set in init_ports().
///
/// I2C bit-bang at ~100kHz (spin delays tuned for 108MHz sysclk).

// --- Hardware addresses ---

const GPIOC_BASE: u32 = 0x4001_1000;
const GPIO_IDR: u32 = GPIOC_BASE + 0x08;
const GPIO_BOP: u32 = GPIOC_BASE + 0x10; // set bits HIGH
const GPIO_BC: u32 = GPIOC_BASE + 0x14;  // set bits LOW

const SCL_PIN: u32 = 0; // PC0
const SDA_PIN: u32 = 1; // PC1

// --- AT8563T / PCF8563 ---

const I2C_ADDR: u8 = 0x51; // 7-bit address

// Register addresses
const REG_CONTROL1: u8 = 0x00;
const REG_CONTROL2: u8 = 0x01;
const REG_SECONDS: u8 = 0x02;
const REG_MINUTES: u8 = 0x03;
const REG_HOURS: u8 = 0x04;
const REG_DAYS: u8 = 0x05;
const REG_WEEKDAYS: u8 = 0x06;
const REG_MONTHS: u8 = 0x07;
const REG_YEARS: u8 = 0x08;

/// Date and time.
#[derive(Clone, Copy)]
pub struct DateTime {
    pub year: u8,   // 0-99 (offset from 2000)
    pub month: u8,  // 1-12
    pub day: u8,    // 1-31
    pub weekday: u8, // 0-6 (0=Sunday)
    pub hour: u8,   // 0-23
    pub minute: u8, // 0-59
    pub second: u8, // 0-59
}

impl DateTime {
    pub const fn zero() -> Self {
        Self {
            year: 0, month: 1, day: 1, weekday: 0,
            hour: 0, minute: 0, second: 0,
        }
    }
}

/// RTC driver. Zero-sized — all state is in the hardware.
pub struct Rtc;

impl Rtc {
    /// Initialize the RTC. Clears any stop flag so the clock runs.
    /// GPIO pins must already be configured (init_ports).
    pub fn init() -> Self {
        let rtc = Self;
        // Clear STOP bit in Control1 to ensure clock is running
        let ctrl1 = rtc.read_reg(REG_CONTROL1);
        if ctrl1 & 0x20 != 0 {
            rtc.write_reg(REG_CONTROL1, ctrl1 & !0x20);
        }
        rtc
    }

    /// Read current date and time.
    pub fn read_time(&self) -> DateTime {
        let mut buf = [0u8; 7];
        self.read_regs(REG_SECONDS, &mut buf);

        DateTime {
            second: bcd_to_bin(buf[0] & 0x7F),
            minute: bcd_to_bin(buf[1] & 0x7F),
            hour: bcd_to_bin(buf[2] & 0x3F),
            day: bcd_to_bin(buf[3] & 0x3F),
            weekday: buf[4] & 0x07,
            month: bcd_to_bin(buf[5] & 0x1F),
            year: bcd_to_bin(buf[6]),
        }
    }

    /// Set date and time.
    pub fn set_time(&self, dt: &DateTime) {
        let buf = [
            bin_to_bcd(dt.second) & 0x7F,
            bin_to_bcd(dt.minute) & 0x7F,
            bin_to_bcd(dt.hour) & 0x3F,
            bin_to_bcd(dt.day) & 0x3F,
            dt.weekday & 0x07,
            bin_to_bcd(dt.month) & 0x1F,
            bin_to_bcd(dt.year),
        ];
        self.write_regs(REG_SECONDS, &buf);
    }

    /// Check if the clock integrity is guaranteed (VL bit in seconds register).
    /// Returns false if power was lost and time may be invalid.
    pub fn is_valid(&self) -> bool {
        let sec = self.read_reg(REG_SECONDS);
        sec & 0x80 == 0 // VL=0 means clock integrity guaranteed
    }

    /// Clear the voltage-low flag (call after setting time following power loss).
    pub fn clear_vl_flag(&self) {
        let sec = self.read_reg(REG_SECONDS);
        if sec & 0x80 != 0 {
            self.write_reg(REG_SECONDS, sec & 0x7F);
        }
    }

    // --- Register access ---

    fn read_reg(&self, reg: u8) -> u8 {
        let mut val = [0u8; 1];
        self.read_regs(reg, &mut val);
        val[0]
    }

    fn write_reg(&self, reg: u8, val: u8) {
        self.write_regs(reg, &[val]);
    }

    fn read_regs(&self, reg: u8, buf: &mut [u8]) {
        // Write register address
        i2c_start();
        i2c_write_byte((I2C_ADDR << 1) | 0); // write mode
        i2c_write_byte(reg);
        // Repeated start + read
        i2c_start();
        i2c_write_byte((I2C_ADDR << 1) | 1); // read mode
        for i in 0..buf.len() {
            let last = i == buf.len() - 1;
            buf[i] = i2c_read_byte(!last); // NACK on last byte
        }
        i2c_stop();
    }

    fn write_regs(&self, reg: u8, data: &[u8]) {
        i2c_start();
        i2c_write_byte((I2C_ADDR << 1) | 0); // write mode
        i2c_write_byte(reg);
        for &b in data {
            i2c_write_byte(b);
        }
        i2c_stop();
    }
}

// --- BCD conversion ---

#[inline]
fn bcd_to_bin(bcd: u8) -> u8 {
    (bcd >> 4) * 10 + (bcd & 0x0F)
}

#[inline]
fn bin_to_bcd(bin: u8) -> u8 {
    ((bin / 10) << 4) | (bin % 10)
}

// --- I2C bit-bang primitives ---

/// Half-period delay (~5μs for ~100kHz I2C at 108MHz sysclk).
#[inline]
fn i2c_delay() {
    spin(80);
}

#[inline]
fn scl_high() {
    unsafe { core::ptr::write_volatile(GPIO_BOP as *mut u32, 1 << SCL_PIN); }
}

#[inline]
fn scl_low() {
    unsafe { core::ptr::write_volatile(GPIO_BC as *mut u32, 1 << SCL_PIN); }
}

#[inline]
fn sda_high() {
    unsafe { core::ptr::write_volatile(GPIO_BOP as *mut u32, 1 << SDA_PIN); }
}

#[inline]
fn sda_low() {
    unsafe { core::ptr::write_volatile(GPIO_BC as *mut u32, 1 << SDA_PIN); }
}

#[inline]
fn sda_read() -> bool {
    let idr = unsafe { core::ptr::read_volatile(GPIO_IDR as *const u32) };
    idr & (1 << SDA_PIN) != 0
}

/// I2C START condition: SDA goes LOW while SCL is HIGH.
fn i2c_start() {
    sda_high();
    i2c_delay();
    scl_high();
    i2c_delay();
    sda_low();
    i2c_delay();
    scl_low();
    i2c_delay();
}

/// I2C STOP condition: SDA goes HIGH while SCL is HIGH.
fn i2c_stop() {
    sda_low();
    i2c_delay();
    scl_high();
    i2c_delay();
    sda_high();
    i2c_delay();
}

/// Write one byte on I2C. Returns ACK (true = acknowledged).
fn i2c_write_byte(byte: u8) -> bool {
    for i in (0..8).rev() {
        if byte & (1 << i) != 0 {
            sda_high();
        } else {
            sda_low();
        }
        i2c_delay();
        scl_high();
        i2c_delay();
        scl_low();
    }
    // Read ACK: release SDA, clock one bit
    sda_high(); // release SDA for slave to drive
    i2c_delay();
    scl_high();
    i2c_delay();
    let ack = !sda_read(); // ACK = SDA pulled LOW by slave
    scl_low();
    i2c_delay();
    ack
}

/// Read one byte from I2C. send_ack=true sends ACK, false sends NACK.
fn i2c_read_byte(send_ack: bool) -> u8 {
    sda_high(); // release SDA for slave to drive
    let mut byte: u8 = 0;
    for _ in 0..8 {
        i2c_delay();
        scl_high();
        i2c_delay();
        byte <<= 1;
        if sda_read() {
            byte |= 1;
        }
        scl_low();
    }
    // Send ACK or NACK
    if send_ack {
        sda_low(); // ACK
    } else {
        sda_high(); // NACK
    }
    i2c_delay();
    scl_high();
    i2c_delay();
    scl_low();
    sda_high(); // release SDA
    i2c_delay();
    byte
}

#[inline]
fn spin(cycles: u32) {
    for _ in 0..cycles {
        cortex_m::asm::nop();
    }
}
