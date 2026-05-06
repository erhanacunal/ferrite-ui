//! Low-level ED047TC1 e-paper controller for LilyGo T5-ePaper-S3.
//!
//! Ported from <https://github.com/fridolin-koch/lilygo-epd47-rs>, adapted for
//! the ferrite-ui global-static architecture and blocking-only RMT.
//!
//! Key differences from ferrite-ui's previous bit-bang / Command::None approach:
//!   - I8080 pin mapping is **scrambled** to match the board PCB layout; pixel
//!     data is compensated by `line_buffer_reorder()` before DMA (same as C ref).
//!   - `Command::One(0)` is sent per row so I8080 drives DC HIGH for one CKH
//!     clock before the pixel bytes — this is the STH start-of-horizontal pulse.
//!   - CD edge config: idle=LOW, cmd=HIGH, data=LOW (matches the EPD expectation).
//!   - Frame timing uses 10 µs and 100 µs pulses (reference values, not 1/10 µs).
//!   - Row skip is 3-state: blank row → short row → CKV-only pulse.

#![cfg(feature = "epaper")]

use esp_hal::{
    dma::DmaTxBuf,
    gpio::{Level, Output, OutputConfig},
    lcd_cam::{
        lcd::i8080::{self, Command, I8080},
        LcdCam,
    },
    peripherals,
    rmt::PulseCode,
    time::Rate,
    Blocking,
};

use crate::lcd::rmt;

// ---- DMA buffer ----
// 240 bytes of pixel data (960 pixels / 4 px per byte) + 8 trailing zeros.
// The trailing zeros ensure the EPD source driver finishes cleanly.
pub(crate) const DMA_BUFFER_SIZE: usize = 248;
pub(crate) const BYTES_PER_LINE: usize = 240; // pixel bytes per row

// ---- Timing helper ----

macro_rules! pulse {
    ($high:expr, $low:expr) => {
        if $high > 0 {
            [
                PulseCode::new(Level::High, $high, Level::Low, $low),
                PulseCode::end_marker(),
            ]
        } else {
            [
                PulseCode::new(Level::High, $low, Level::Low, 0),
                PulseCode::end_marker(),
            ]
        }
    };
}

// ---- Config shift register ----

struct ConfigRegister {
    latch_enable: bool,
    power_disable: bool,
    pos_power_enable: bool,
    neg_power_enable: bool,
    stv: bool,
    power_enable: bool, // scan direction
    mode: bool,
    output_enable: bool,
}

impl Default for ConfigRegister {
    fn default() -> Self {
        Self {
            latch_enable: false,
            power_disable: true,
            pos_power_enable: false,
            neg_power_enable: false,
            stv: true,
            power_enable: false,
            mode: false,
            output_enable: false,
        }
    }
}

struct ConfigWriter {
    pin_data: Output<'static>,
    pin_clk: Output<'static>,
    pin_str: Output<'static>,
    config: ConfigRegister,
}

impl ConfigWriter {
    fn new(
        data: peripherals::GPIO13<'static>,
        clk: peripherals::GPIO12<'static>,
        str_: peripherals::GPIO0<'static>,
    ) -> Self {
        let cfg = OutputConfig::default();
        Self {
            pin_data: Output::new(data, Level::High, cfg),
            pin_clk: Output::new(clk, Level::High, cfg),
            pin_str: Output::new(str_, Level::Low, cfg),
            config: ConfigRegister::default(),
        }
    }

    fn write(&mut self) {
        self.pin_str.set_low();
        // MSB-first: output_enable is the first bit shifted in
        self.write_bit(self.config.output_enable);
        self.write_bit(self.config.mode);
        self.write_bit(self.config.power_enable);
        self.write_bit(self.config.stv);
        self.write_bit(self.config.neg_power_enable);
        self.write_bit(self.config.pos_power_enable);
        self.write_bit(self.config.power_disable);
        self.write_bit(self.config.latch_enable);
        self.pin_str.set_high();
    }

    #[inline(always)]
    fn write_bit(&mut self, v: bool) {
        self.pin_clk.set_low();
        if v {
            self.pin_data.set_high();
        } else {
            self.pin_data.set_low();
        }
        self.pin_clk.set_high();
    }
}

// ---- ED047TC1 controller ----

pub(crate) struct ED047TC1 {
    i8080: Option<I8080<'static, Blocking>>,
    dma_buf: Option<DmaTxBuf>,
    rmt: rmt::Rmt<'static>,
    cfg_writer: ConfigWriter,
    pub(crate) skipping: u16,
}

impl ED047TC1 {
    pub(crate) fn new(
        cfg_data: peripherals::GPIO13<'static>,
        cfg_clk: peripherals::GPIO12<'static>,
        cfg_str: peripherals::GPIO0<'static>,
        data0: peripherals::GPIO8<'static>,    // EPD D0
        data1: peripherals::GPIO1<'static>,    // EPD D1
        data2: peripherals::GPIO2<'static>,    // EPD D2
        data3: peripherals::GPIO3<'static>,    // EPD D3
        data4: peripherals::GPIO4<'static>,    // EPD D4
        data5: peripherals::GPIO5<'static>,    // EPD D5
        data6: peripherals::GPIO6<'static>,    // EPD D6
        data7: peripherals::GPIO7<'static>,    // EPD D7
        lcd_dc: peripherals::GPIO40<'static>,  // STH (DC)
        lcd_wrx: peripherals::GPIO41<'static>, // CKH (WRX)
        dma_ch: peripherals::DMA_CH0<'static>,
        lcd_cam: peripherals::LCD_CAM<'static>,
        rmt_periph: peripherals::RMT<'static>,
    ) -> Self {
        // Config shift register
        let mut cfg_writer = ConfigWriter::new(cfg_data, cfg_clk, cfg_str);
        cfg_writer.write();
        crate::usart::dbg(b"  cfg ok\r\n");

        // I8080 — scrambled GPIO→bit mapping to match PCB layout.
        // Compensated by line_buffer_reorder() before every DMA transfer.
        // CD edges: idle=LOW, cmd=HIGH, data=LOW.
        // Command::One(0) generates one CKH clock with DC HIGH = STH pulse.
        let lcd = LcdCam::new(lcd_cam);
        let i8080_cfg = i8080::Config::default()
            .with_frequency(Rate::from_mhz(10))
            .with_cd_idle_edge(false)
            .with_cd_cmd_edge(true)
            .with_cd_dummy_edge(false)
            .with_cd_data_edge(false);
        crate::usart::dbg(b"  i8080 init\r\n");
        let i8080 = I8080::new(lcd.lcd, dma_ch, i8080_cfg)
            .expect("I8080 init failed")
            .with_dc(lcd_dc)
            .with_wrx(lcd_wrx)
            .with_data0(data6) // I8080 bit0 ← GPIO6 (EPD D6)
            .with_data1(data7) // I8080 bit1 ← GPIO7 (EPD D7)
            .with_data2(data4) // I8080 bit2 ← GPIO4 (EPD D4)
            .with_data3(data5) // I8080 bit3 ← GPIO5 (EPD D5)
            .with_data4(data2) // I8080 bit4 ← GPIO2 (EPD D2)
            .with_data5(data3) // I8080 bit5 ← GPIO3 (EPD D3)
            .with_data6(data0) // I8080 bit6 ← GPIO8 (EPD D0)
            .with_data7(data1); // I8080 bit7 ← GPIO1 (EPD D1)
        crate::usart::dbg(b"  i8080 ok\r\n");

        // DMA TX buffer (248 bytes: 240 pixel + 8 trailing zeros)
        crate::usart::dbg(b"  dma buf\r\n");
        let dma_buf = esp_hal::dma_tx_buffer!(DMA_BUFFER_SIZE).expect("DMA buffer init failed");
        crate::usart::dbg(b"  dma ok\r\n");

        Self {
            i8080: Some(i8080),
            dma_buf: Some(dma_buf),
            rmt: rmt::Rmt::new(rmt_periph),
            cfg_writer,
            skipping: 0,
        }
    }

    // ---- Power sequencing ----

    pub(crate) fn power_on(&mut self) {
        self.cfg_writer.config.power_enable = true;
        self.cfg_writer.config.power_disable = false;
        self.cfg_writer.write();
        busy_delay(100 * 240);

        self.cfg_writer.config.neg_power_enable = true;
        self.cfg_writer.write();
        busy_delay(500 * 240);

        self.cfg_writer.config.pos_power_enable = true;
        self.cfg_writer.write();
        busy_delay(100 * 240);

        self.cfg_writer.config.stv = true;
        self.cfg_writer.write();
    }

    pub(crate) fn power_off(&mut self) {
        self.cfg_writer.config.pos_power_enable = false;
        self.cfg_writer.write();
        busy_delay(10 * 240);

        self.cfg_writer.config.neg_power_enable = false;
        self.cfg_writer.write();
        busy_delay(100 * 240);

        self.cfg_writer.config.power_disable = true;
        self.cfg_writer.write();
        self.cfg_writer.config.stv = false;
        self.cfg_writer.write();
    }

    // ---- Frame control ----

    pub(crate) fn frame_start(&mut self) {
        self.cfg_writer.config.mode = true;
        self.cfg_writer.write();

        let data = pulse!(10, 10);
        self.rmt
            .pulse(&data, true)
            .expect("frame_start pulse failed");

        self.cfg_writer.config.stv = false;
        self.cfg_writer.write();
        busy_delay(240);

        // Start 100 µs CKV pulse async so the config write overlaps.
        let data = pulse!(100, 100);
        let rmt_tx = self.rmt.pulse(&data, false).expect("rmt_tx failed");

        self.cfg_writer.config.stv = true;
        self.cfg_writer.write();

        if let Some(rmt_tx) = rmt_tx {
            self.rmt
                .reclaim_channel(rmt_tx)
                .expect("reclaim_channel failed");
        }

        let data = pulse!(0, 100);
        self.rmt.pulse(&data, true).expect("pulse failed");

        self.cfg_writer.config.output_enable = true;
        self.cfg_writer.write();

        let data = pulse!(10, 10);
        self.rmt.pulse(&data, true).expect("pulse failed");
    }

    pub(crate) fn frame_end(&mut self) {
        self.cfg_writer.config.output_enable = false;
        self.cfg_writer.write();
        self.cfg_writer.config.mode = false;
        self.cfg_writer.write();
        let data = pulse!(10, 10);
        self.rmt.pulse(&data, true).expect("pulse failed");
        self.rmt.pulse(&data, true).expect("pulse failed");
    }

    // ---- Row operations ----

    pub(crate) fn latch_row(&mut self) {
        self.cfg_writer.config.latch_enable = true;
        self.cfg_writer.write();
        self.cfg_writer.config.latch_enable = false;
        self.cfg_writer.write();
    }

    /// Transmit the DMA buffer as one EPD row.
    /// `output_time_us`: CKV high-pulse duration in microseconds.
    /// RMT and I8080 DMA run concurrently for throughput.
    pub(crate) fn output_row(&mut self, output_time: u16) {
        self.latch_row();

        // Start RMT pulse (async) — runs while I8080 DMA is in progress.
        let data = pulse!(output_time, 50);
        let rmt_tx = self.rmt.pulse(&data, false).expect("rmt_tx failed");

        // I8080 DMA: Command::One(0) = one CKH clock with DC HIGH (STH strobe),
        // then 248 bytes of pixel data with DC LOW.
        let dev = unsafe { (*(&raw mut self.i8080)).take().unwrap_unchecked() };
        let buf = unsafe { (*(&raw mut self.dma_buf)).take().unwrap_unchecked() };
        let tx = dev
            .send(Command::<u8>::One(0), 0, buf)
            .map_err(|(_, d, b)| unsafe {
                *(&raw mut self.i8080) = Some(d);
                *(&raw mut self.dma_buf) = Some(b);
            })
            .expect("I8080 send failed");
        let (_, dev_back, buf_back) = tx.wait();
        if let Some(rmt_tx) = rmt_tx {
            self.rmt
                .reclaim_channel(rmt_tx)
                .expect("reclaim_channel failed");
        }
        unsafe {
            *(&raw mut self.i8080) = Some(dev_back);
            *(&raw mut self.dma_buf) = Some(buf_back);
        }
    }

    /// CKV-only gate advance — no data clocked. Used for fast row skipping.
    pub(crate) fn skip(&mut self) {
        let data = pulse!(45, 5);
        self.rmt.pulse(&data, true).expect("rmt_tx failed");
    }

    /// Copy pixel data into the DMA buffer (zeroes remainder).
    pub(crate) fn set_buffer(&mut self, data: &[u8]) {
        let buf = unsafe { (*(&raw mut self.dma_buf)).as_mut().unwrap_unchecked() };
        buf.as_mut_slice().fill(0);
        let n = data.len().min(DMA_BUFFER_SIZE);
        buf.as_mut_slice()[..n].copy_from_slice(&data[..n]);
    }
}

// ---- Global static ----

static mut CONTROLLER: Option<ED047TC1> = None;

pub(crate) unsafe fn ctrl() -> &'static mut ED047TC1 {
    (*(&raw mut CONTROLLER)).as_mut().unwrap_unchecked()
}

pub(crate) fn install(epd: ED047TC1) {
    unsafe {
        CONTROLLER = Some(epd);
    }
}

// ---- Xtensa cycle-count busy delay ----

#[inline(always)]
fn busy_delay(wait_cycles: u32) {
    let target = cycles() + wait_cycles as u64;
    while cycles() < target {}
}

#[inline(always)]
fn cycles() -> u64 {
    esp_hal::xtensa_lx::timer::get_cycle_count() as u64
}
