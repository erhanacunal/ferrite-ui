/// XPT2046 Touch Controller — SPI bit-bang driver (firmware backend).
///
/// Pin assignment (Ghidra RE):
///   PA0 = MISO (XPT2046 DATA OUT → MCU input)
///   PA1 = MOSI (MCU → XPT2046 DATA IN, output)
///   PA2 = CS (chip select, active low, output)
///   PA3 = CLK (SPI clock, output)
///
/// Touch detection: double-read debounce (matches proven C implementation).
/// Coordinate: raw ADC → linear calibration → screen pixel.
///
/// Sampling is interrupt-driven (`IRQ_TOUCH`):
/// - idle: EXTI14 waits for PENIRQ (PC14) to fall; an untouched panel costs
///   nothing;
/// - pen down: the EXTI ISR masks EXTI14 (PENIRQ also toggles during every
///   conversion) and starts sampling;
/// - every `SAMPLE_PERIOD_MS` the SysTick ISR takes one double-read sample
///   and queues it in a ring buffer;
/// - the XPT2046 has no pen-up interrupt: after `UP_SAMPLES` pen-up samples
///   in a row it queues `UP` and re-arms EXTI14.
/// Both ISRs run below USART0's priority: a sample takes ~0.3 ms, longer than
/// one byte time at 115200 baud, and USART0 has a 1-byte RX register.
/// Main-loop SPI users (boot recovery check, calibration) run while the
/// sampler is not armed yet or paused.
use core::cell::Cell;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};

use super::{CalParams, TouchBackend};
use crate::lcd::Lcd;
use crate::systick::delay_ms;

// --- PENIRQ pin (PC14) — direct GPIO read, no interrupt ---

const GPIOC_IDR: u32 = 0x4001_1008;
const PENIRQ_PIN: u32 = 14;

#[inline]
fn penirq_active() -> bool {
    unsafe { core::ptr::read_volatile(GPIOC_IDR as *const u32) & (1 << PENIRQ_PIN) == 0 }
}

/// Public PENIRQ check for boot-time recovery gate.
#[inline]
pub fn penirq_active_pub() -> bool {
    penirq_active()
}

// --- Hardware addresses ---

const GPIOA_BASE: u32 = 0x4001_0800;
const GPIO_IDR: u32 = GPIOA_BASE + 0x08;
const GPIO_BOP: u32 = GPIOA_BASE + 0x10;
const GPIO_BC: u32 = GPIOA_BASE + 0x14;

const MISO_PIN: u32 = 0;
const MOSI_PIN: u32 = 1;
const CS_PIN: u32 = 2;
const CLK_PIN: u32 = 3;

const CMD_X: u8 = 0x90;
const CMD_Y: u8 = 0xD0;
const CMD_Z1: u8 = 0xB0;

const RAW_ERROR: u16 = 50;
const Z_THRESHOLD: u16 = 50;

const SCREEN_W: u16 = 800;
const SCREEN_H: u16 = 480;

const SPI_HALF_CLK: u32 = 54;
const SPI_CS_DELAY: u32 = 10;
const SPI_ADC_WAIT: u32 = 600;
const SPI_BUSY_LOW: u32 = 14;
const SPI_BUSY_HIGH: u32 = SPI_HALF_CLK;

const CAL_P1: (u16, u16) = (60, 60);
const CAL_P2: (u16, u16) = (740, 60);
const CAL_P3: (u16, u16) = (60, 420);

// --- Interrupt-driven sampling ---

/// `false` falls back to polling the XPT2046 from the main loop.
const IRQ_TOUCH: bool = true;

/// Sample period while the pen is down (100 Hz).
const SAMPLE_PERIOD_MS: u32 = 10;
/// Pen-up samples in a row that end a contact (30 ms).
const UP_SAMPLES: u8 = 3;

const AFIO_EXTISS3: u32 = 0x4001_0014; // EXTI12..15 source select
const EXTISS_PORT_C: u32 = 0x2;
const EXTI_BASE: u32 = 0x4001_0400;
const EXTI_INTEN: u32 = EXTI_BASE + 0x00;
const EXTI_FTEN: u32 = EXTI_BASE + 0x0C;
const EXTI_PD: u32 = EXTI_BASE + 0x14;
const EXTI_LINE: u32 = 1 << PENIRQ_PIN;

/// EXTI10..15 is IRQ 40: NVIC ISER1 bit 8, priority byte IPR + 40.
const NVIC_ISER1: u32 = 0xE000_E104;
const NVIC_IPR_EXTI10_15: u32 = 0xE000_E400 + 40;
/// SysTick priority byte (SHPR3[31:24]).
const SCB_SHPR3_SYSTICK: u32 = 0xE000_ED23;
/// Below USART0 (priority 0); the GD32 implements the top 4 bits.
const SAMPLER_PRIORITY: u8 = 0xC0;

// Ring buffer: the SysTick ISR produces (owns the head), the main loop
// consumes (owns the tail). Entries are raw points `(x << 16) | y`, or `UP`.
// Aligned word accesses with one writer each, volatile, single core.
const RING_SIZE: usize = 16;
const RING_MASK: usize = RING_SIZE - 1;
/// End of a contact (raw values are 12-bit, so no point encodes to this).
const UP: u32 = u32::MAX;

static mut RING: [u32; RING_SIZE] = [0; RING_SIZE];
static mut RING_HEAD: usize = 0;
static mut RING_TAIL: usize = 0;

/// EXTI / NVIC set up (on the first poll, after boot).
static mut ARMED: bool = false;
/// A contact is being sampled (EXTI14 masked meanwhile).
static mut SAMPLING: bool = false;
/// Consecutive pen-up samples (ISR).
static mut UP_COUNT: u8 = 0;
/// A point of this contact was queued, so `UP` must follow.
static mut QUEUED: bool = false;
/// Milliseconds since the last sample (ISR).
static mut TICKS: u32 = 0;

/// Queue `v` if more than `reserve` slots stay free. Points keep one slot in
/// reserve so the `UP` that closes a contact is never dropped.
fn ring_push(v: u32, reserve: usize) -> bool {
    unsafe {
        let head = read_volatile(addr_of!(RING_HEAD));
        let used = head.wrapping_sub(read_volatile(addr_of!(RING_TAIL))) & RING_MASK;
        if RING_SIZE - 1 - used <= reserve {
            return false;
        }
        write_volatile(addr_of_mut!(RING[head]), v);
        write_volatile(addr_of_mut!(RING_HEAD), (head + 1) & RING_MASK);
    }
    true
}

fn ring_peek() -> Option<u32> {
    unsafe {
        let tail = read_volatile(addr_of!(RING_TAIL));
        if tail == read_volatile(addr_of!(RING_HEAD)) {
            return None;
        }
        Some(read_volatile(addr_of!(RING[tail])))
    }
}

fn ring_pop() {
    unsafe {
        let tail = read_volatile(addr_of!(RING_TAIL));
        write_volatile(addr_of_mut!(RING_TAIL), (tail + 1) & RING_MASK);
    }
}

fn reg_write(addr: u32, v: u32) {
    unsafe { write_volatile(addr as *mut u32, v) };
}

fn reg_read(addr: u32) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

/// Pen went down: mask EXTI14 and start sampling.
fn start_contact() {
    reg_write(EXTI_INTEN, 0); // EXTI14 is the only line in use
    unsafe {
        TICKS = 0;
        write_volatile(addr_of_mut!(SAMPLING), true);
    }
}

/// Wait for the next pen-down. Catches a pen that is already down.
fn listen() {
    // PENIRQ needs a moment to return high after the last conversion; an
    // early check (or a pending flag set by that edge) would start a
    // spurious contact.
    spin(SPI_ADC_WAIT * 4);
    reg_write(EXTI_PD, EXTI_LINE);
    reg_write(EXTI_INTEN, EXTI_LINE);
    if penirq_active() {
        start_contact();
    }
}

/// End the current contact: stop sampling and queue `UP` if needed.
fn close_contact() {
    unsafe {
        write_volatile(addr_of_mut!(SAMPLING), false);
        if QUEUED {
            ring_push(UP, 0);
        }
        QUEUED = false;
        UP_COUNT = 0;
    }
}

/// Set up EXTI14 on PC14 (falling edge) and the priorities, then start
/// listening. Main loop, once.
fn arm() {
    unsafe {
        if read_volatile(addr_of!(ARMED)) {
            return;
        }
        let v = reg_read(AFIO_EXTISS3) & !(0xF << 8);
        reg_write(AFIO_EXTISS3, v | (EXTISS_PORT_C << 8));
        reg_write(EXTI_FTEN, reg_read(EXTI_FTEN) | EXTI_LINE);
        write_volatile(NVIC_IPR_EXTI10_15 as *mut u8, SAMPLER_PRIORITY);
        write_volatile(SCB_SHPR3_SYSTICK as *mut u8, SAMPLER_PRIORITY);
        reg_write(NVIC_ISER1, 1 << 8);
        write_volatile(addr_of_mut!(ARMED), true);
    }
    listen();
}

/// Stop interrupt sampling so the main loop can drive the XPT2046 itself.
/// A contact in progress is closed.
fn pause() {
    reg_write(EXTI_INTEN, 0);
    close_contact();
}

/// Undo `pause`.
fn resume() {
    if unsafe { read_volatile(addr_of!(ARMED)) } {
        listen();
    }
}

/// EXTI10..15 (IRQ 40): PENIRQ fell.
#[unsafe(no_mangle)]
extern "C" fn EXTI10_15() {
    reg_write(EXTI_PD, EXTI_LINE);
    start_contact();
}

/// Called from the SysTick ISR every millisecond.
#[inline]
pub fn on_tick() {
    unsafe {
        if !read_volatile(addr_of!(SAMPLING)) {
            return;
        }
        TICKS += 1;
        if TICKS < SAMPLE_PERIOD_MS {
            return;
        }
        TICKS = 0;
    }
    sample();
}

/// One sample of the current contact. SysTick ISR.
fn sample() {
    unsafe {
        match read_raw_sample() {
            Some(p) => {
                UP_COUNT = 0;
                if ring_push(encode(p), 1) {
                    QUEUED = true;
                }
            }
            // Still pressed, unstable reading: skip it.
            None if is_pressed() => UP_COUNT = 0,
            None => {
                UP_COUNT += 1;
                if UP_COUNT >= UP_SAMPLES {
                    close_contact();
                    listen();
                }
            }
        }
    }
}

fn encode(p: (u16, u16)) -> u32 {
    ((p.0 as u32) << 16) | p.1 as u32
}

fn decode(v: u32) -> (u16, u16) {
    ((v >> 16) as u16, v as u16)
}

// --- Backend struct ---

pub struct XptTouch {
    /// Last raw point of the current contact, `None` between contacts.
    contact: Cell<Option<(u16, u16)>>,
}

impl XptTouch {
    pub fn new() -> Self {
        Self { contact: Cell::new(None) }
    }

    /// Consume the queue up to the next `UP`: the latest point if any moved
    /// in, else the last one of the contact, else `None` once `UP` is read.
    fn current_raw(&self) -> Option<(u16, u16)> {
        let mut moved = false;
        while let Some(v) = ring_peek() {
            if v == UP {
                // Report the final point first; the release on the next poll.
                if !moved {
                    ring_pop();
                    self.contact.set(None);
                }
                break;
            }
            ring_pop();
            self.contact.set(Some(decode(v)));
            moved = true;
        }
        self.contact.get()
    }
}

impl TouchBackend for XptTouch {
    fn is_active(&self) -> bool {
        if IRQ_TOUCH {
            arm();
            ring_peek().is_some()
        } else {
            penirq_active()
        }
    }

    fn read_screen(&self, cal: &CalParams) -> Option<(u16, u16)> {
        let (rx, ry) = if IRQ_TOUCH { self.current_raw()? } else { read_raw_sample()? };
        let (cx, cy) = if cal.xy_swap { (ry, rx) } else { (rx, ry) };
        let x = apply_cal(cx, cal.x_min, cal.x_max, SCREEN_W, cal.x_flip);
        let y = apply_cal(cy, cal.y_min, cal.y_max, SCREEN_H, cal.y_flip);
        Some((x, y))
    }

    /// The ISR debounces lift-off: release on the first `None`.
    fn release_debounce(&self) -> u8 {
        if IRQ_TOUCH { 1 } else { super::RELEASE_DEBOUNCE }
    }
}

// --- Calibration math ---

fn apply_cal(raw: u16, cal_min: u16, cal_max: u16, screen_size: u16, flip: bool) -> u16 {
    if cal_max <= cal_min {
        return 0;
    }
    let raw = if raw < cal_min {
        cal_min
    } else if raw > cal_max {
        cal_max
    } else {
        raw
    };
    let val = ((raw - cal_min) as u32 * screen_size as u32 / (cal_max - cal_min) as u32) as u16;
    let val = if val > screen_size { screen_size } else { val };
    if flip { screen_size - val } else { val }
}

pub fn run_calibration(touch: &mut super::Touch, lcd: &Lcd) -> CalParams {
    let bg = 0x0000u16;
    let cross_color = 0xF800u16;
    let done_color = 0x07E0u16;

    // The crosshair waits read the XPT2046 directly.
    pause();
    lcd.fill_rect(0, 0, SCREEN_W, SCREEN_H, bg);

    draw_crosshair(lcd, CAL_P1.0, CAL_P1.1, cross_color);
    let raw1 = wait_raw_touch();
    draw_crosshair(lcd, CAL_P1.0, CAL_P1.1, done_color);

    draw_crosshair(lcd, CAL_P2.0, CAL_P2.1, cross_color);
    let raw2 = wait_raw_touch();
    draw_crosshair(lcd, CAL_P2.0, CAL_P2.1, done_color);

    draw_crosshair(lcd, CAL_P3.0, CAL_P3.1, cross_color);
    let raw3 = wait_raw_touch();
    draw_crosshair(lcd, CAL_P3.0, CAL_P3.1, done_color);
    wait_release();

    let cal = compute_calibration(raw1, raw2, raw3);
    touch.cal = cal;
    resume();

    lcd.fill_rect(0, 0, SCREEN_W, SCREEN_H, bg);

    cal
}

fn compute_calibration(raw1: (u16, u16), raw2: (u16, u16), raw3: (u16, u16)) -> CalParams {
    let dx_rx = abs_diff(raw1.0, raw2.0);
    let dx_ry = abs_diff(raw1.1, raw2.1);
    let xy_swap = dx_ry > dx_rx;

    let (sx1, sx2) = if xy_swap {
        (raw1.1, raw2.1)
    } else {
        (raw1.0, raw2.0)
    };
    let (sy1, sy3) = if xy_swap {
        (raw1.0, raw3.0)
    } else {
        (raw1.1, raw3.1)
    };

    let sx1_i = sx1 as i32;
    let sx2_i = sx2 as i32;
    let dx = sx2_i - sx1_i;
    let raw_at_x0 = sx1_i - 60 * dx / 680;
    let raw_at_x800 = sx1_i + 740 * dx / 680;
    let x_flip = raw_at_x0 > raw_at_x800;
    let (x_min, x_max) = if x_flip {
        (raw_at_x800, raw_at_x0)
    } else {
        (raw_at_x0, raw_at_x800)
    };

    let sy1_i = sy1 as i32;
    let sy3_i = sy3 as i32;
    let dy = sy3_i - sy1_i;
    let raw_at_y0 = sy1_i - 60 * dy / 360;
    let raw_at_y480 = sy1_i + 420 * dy / 360;
    let y_flip = raw_at_y0 > raw_at_y480;
    let (y_min, y_max) = if y_flip {
        (raw_at_y480, raw_at_y0)
    } else {
        (raw_at_y0, raw_at_y480)
    };

    CalParams {
        xy_swap,
        x_flip,
        y_flip,
        x_min: (x_min.max(0) as u16).min(4095),
        x_max: (x_max.max(0) as u16).min(4095),
        y_min: (y_min.max(0) as u16).min(4095),
        y_max: (y_max.max(0) as u16).min(4095),
    }
}

fn draw_crosshair(lcd: &Lcd, x: u16, y: u16, color: u16) {
    let x1 = if x >= 15 { x - 15 } else { 0 };
    let y1 = if y >= 15 { y - 15 } else { 0 };
    lcd.fill_rect(x1, y, 31, 1, color);
    lcd.fill_rect(x, y1, 1, 31, color);
}

const STABLE_COUNT: u8 = 8;

fn is_pressed() -> bool {
    let z1 = read_raw_axis_cmd(CMD_Z1);
    z1 > Z_THRESHOLD
}

fn wait_release() {
    loop {
        delay_ms(20);
        if !is_pressed() {
            break;
        }
    }
    delay_ms(200);
}

fn wait_raw_touch() -> (u16, u16) {
    wait_release();

    let mut count: u8 = 0;
    let mut acc_x: u32 = 0;
    let mut acc_y: u32 = 0;

    loop {
        delay_ms(10);
        if is_pressed() {
            if let Some((rx, ry)) = read_raw_sample() {
                acc_x += rx as u32;
                acc_y += ry as u32;
                count += 1;
                if count >= STABLE_COUNT {
                    let x = (acc_x / count as u32) as u16;
                    let y = (acc_y / count as u32) as u16;
                    return (x, y);
                }
            }
        } else {
            count = 0;
            acc_x = 0;
            acc_y = 0;
        }
    }
}

// === Boot recovery check ===

pub fn check_recovery_touch(cal: &CalParams, hold_ms: u32) -> bool {
    let start = crate::systick::millis();

    loop {
        let elapsed = crate::systick::millis().wrapping_sub(start);
        if elapsed >= hold_ms {
            return true;
        }

        let sample = read_raw_sample();
        match sample {
            Some((rx, ry)) => {
                let (cx, cy) = if cal.xy_swap { (ry, rx) } else { (rx, ry) };
                let x = apply_cal(cx, cal.x_min, cal.x_max, SCREEN_W, cal.x_flip);
                let y = apply_cal(cy, cal.y_min, cal.y_max, SCREEN_H, cal.y_flip);
                if x >= 50 || y >= 50 {
                    return false;
                }
            }
            None => {
                return false;
            }
        }
    }
}

// === Low-level SPI ===

fn read_raw_sample() -> Option<(u16, u16)> {
    let x1 = read_raw_axis(true);
    let y1 = read_raw_axis(false);
    let x2 = read_raw_axis(true);
    let y2 = read_raw_axis(false);

    if abs_diff(x1, x2) > RAW_ERROR || abs_diff(y1, y2) > RAW_ERROR {
        return None;
    }
    if x1 == 0 || x1 >= 4095 || y1 == 0 || y1 >= 4095 {
        return None;
    }
    Some((x1, y1))
}

fn read_raw_axis(axis: bool) -> u16 {
    read_raw_axis_cmd(if axis { CMD_X } else { CMD_Y })
}

fn read_raw_axis_cmd(cmd: u8) -> u16 {
    pin_high(CS_PIN);
    spin(SPI_CS_DELAY);

    pin_low(CLK_PIN);
    spin(SPI_HALF_CLK);
    pin_low(MOSI_PIN);
    spin(SPI_CS_DELAY);

    pin_low(CS_PIN);
    spin(SPI_CS_DELAY);

    spi_write_byte(cmd);

    spin(SPI_ADC_WAIT);

    pin_low(CLK_PIN);
    spin(SPI_BUSY_LOW);
    pin_high(CLK_PIN);
    spin(SPI_BUSY_HIGH);
    pin_low(CLK_PIN);

    let mut data: u16 = 0;
    for _ in 0..16 {
        data <<= 1;
        pin_low(CLK_PIN);
        spin(SPI_HALF_CLK);
        pin_high(CLK_PIN);
        spin(SPI_HALF_CLK);
        if pin_read(MISO_PIN) {
            data += 1;
        }
    }

    pin_high(CS_PIN);
    spin(SPI_CS_DELAY);

    data >> 4
}

fn spi_write_byte(byte: u8) {
    let mut data = byte as u32;
    for _ in 0..8 {
        if data & 0x80 != 0 {
            pin_high(MOSI_PIN);
        } else {
            pin_low(MOSI_PIN);
        }
        data <<= 1;
        pin_low(CLK_PIN);
        spin(SPI_HALF_CLK);
        pin_high(CLK_PIN);
        spin(SPI_HALF_CLK);
    }
}

fn abs_diff(a: u16, b: u16) -> u16 {
    if a > b { a - b } else { b - a }
}

// === GPIO helpers ===

#[inline(always)]
fn pin_high(pin: u32) {
    unsafe {
        let bop = GPIO_BOP as *mut u32;
        core::ptr::write_volatile(bop, 1 << pin);
    }
}

#[inline(always)]
fn pin_low(pin: u32) {
    unsafe {
        let bc = GPIO_BC as *mut u32;
        core::ptr::write_volatile(bc, 1 << pin);
    }
}

#[inline(always)]
fn pin_read(pin: u32) -> bool {
    unsafe {
        let idr = GPIO_IDR as *const u32;
        core::ptr::read_volatile(idr) & (1 << pin) != 0
    }
}

#[inline(always)]
fn spin(cycles: u32) {
    for _ in 0..cycles {
        cortex_m::asm::nop();
    }
}
