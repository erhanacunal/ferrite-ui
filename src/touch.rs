/// XPT2046 Touch Controller — SPI bit-bang driver
///
/// Pin ataması (Ghidra RE):
///   PA0 = MISO (XPT2046 DATA OUT → MCU input)
///   PA1 = MOSI (MCU → XPT2046 DATA IN, output)
///   PA2 = CS (chip select, active low, output)
///   PA3 = CLK (SPI clock, output)
///   PC14 = PENIRQ (şimdilik kullanılmıyor, polling tabanlı)
///
/// Dokunma algılama: Z-pressure tabanlı (ekstra IRQ pini gereksiz).
/// Koordinat: 5 sample median filtre → lineer kalibrasyon → ekran pikseli.
/// GPIO ve pin initial state konfigürasyonu init_ports() tarafından yapılır.

use crate::widget::{WidgetId, WidgetTree, FLAG_CLICKABLE, FLAG_VISIBLE};

// --- Donanım adresleri ---

const GPIOA_BASE: u32 = 0x4001_0800;
const GPIO_IDR: u32 = GPIOA_BASE + 0x08;
const GPIO_BOP: u32 = GPIOA_BASE + 0x10;
const GPIO_BC: u32 = GPIOA_BASE + 0x14;

// --- Pin tanımları (Ghidra RE) ---

const MISO_PIN: u32 = 0; // PA0 — XPT2046 DATA OUT (input)
const MOSI_PIN: u32 = 1; // PA1 — XPT2046 DATA IN (output)
const CS_PIN: u32 = 2;   // PA2 — chip select (output)
const CLK_PIN: u32 = 3;  // PA3 — SPI clock (output)

// --- XPT2046 komutları ---
// Format: 1_A2A1A0_MODE_SER_PD1PD0
// 12-bit, differential, power-down + PENIRQ enabled

const CMD_X: u8 = 0xD0; // 1_101_0_0_00
const CMD_Y: u8 = 0x90; // 1_001_0_0_00
const CMD_Z1: u8 = 0xB0; // 1_011_0_0_00
const CMD_Z2: u8 = 0xC0; // 1_100_0_0_00

// --- Eşik ve filtre ---

/// Z-pressure eşiği (ham ADC). Altındaysa dokunma yok.
const PRESSURE_THRESHOLD: u16 = 100;

/// Filtre: sample sayısı (median için)
const SAMPLE_COUNT: usize = 5;

// --- Kalibrasyon ---
// XPT2046 ham değerleri (0–4095) → ekran pikseli
// Gerçek donanımda kalibre edilmeli.

const CAL_X_MIN: u16 = 200;
const CAL_X_MAX: u16 = 3900;
const CAL_Y_MIN: u16 = 200;
const CAL_Y_MAX: u16 = 3900;

const SCREEN_W: u16 = 800;
const SCREEN_H: u16 = 480;

// --- Debounce ---

/// Basılı kalma süresi (poll döngüsü sayısı). Bunun altında press kabul edilmez.
const DEBOUNCE_COUNT: u8 = 3;

// --- Touch state machine ---

#[derive(Clone, Copy, PartialEq)]
enum TouchState {
    Idle,
    Debounce,
    Pressed,
    Held,
}

#[derive(Clone, Copy, PartialEq)]
pub enum TouchEventKind {
    Press,
    Hold,
    Release,
}

#[derive(Clone, Copy)]
pub struct TouchEvent {
    pub kind: TouchEventKind,
    pub x: u16,
    pub y: u16,
}

pub struct Touch {
    state: TouchState,
    debounce_counter: u8,
    last_x: u16,
    last_y: u16,
}

impl Touch {
    /// Touch controller'ı başlat.
    /// GPIO ve pin initial state init_ports()'ta yapıldı.
    pub fn init() -> Self {
        Touch {
            state: TouchState::Idle,
            debounce_counter: 0,
            last_x: 0,
            last_y: 0,
        }
    }

    /// Ana döngüden çağrılır. Dokunma durumunu yoklar, event döndürür.
    ///
    /// - `Press`: parmak yeni değdi (debounce sonrası)
    /// - `Hold`: parmak hâlâ basılı (koordinat güncellenmiş olabilir)
    /// - `Release`: parmak kalktı
    pub fn poll(&mut self) -> Option<TouchEvent> {
        let sample = read_touch();

        match self.state {
            TouchState::Idle => {
                if let Some((x, y)) = sample {
                    self.last_x = x;
                    self.last_y = y;
                    self.debounce_counter = 1;
                    self.state = TouchState::Debounce;
                }
                None
            }

            TouchState::Debounce => {
                if let Some((x, y)) = sample {
                    self.last_x = x;
                    self.last_y = y;
                    self.debounce_counter += 1;
                    if self.debounce_counter >= DEBOUNCE_COUNT {
                        self.state = TouchState::Pressed;
                        Some(TouchEvent {
                            kind: TouchEventKind::Press,
                            x: self.last_x,
                            y: self.last_y,
                        })
                    } else {
                        None
                    }
                } else {
                    // Gürültüydü — geri idle'a
                    self.state = TouchState::Idle;
                    self.debounce_counter = 0;
                    None
                }
            }

            TouchState::Pressed => {
                if let Some((x, y)) = sample {
                    self.last_x = x;
                    self.last_y = y;
                    self.state = TouchState::Held;
                    Some(TouchEvent {
                        kind: TouchEventKind::Hold,
                        x,
                        y,
                    })
                } else {
                    self.state = TouchState::Idle;
                    self.debounce_counter = 0;
                    Some(TouchEvent {
                        kind: TouchEventKind::Release,
                        x: self.last_x,
                        y: self.last_y,
                    })
                }
            }

            TouchState::Held => {
                if let Some((x, y)) = sample {
                    self.last_x = x;
                    self.last_y = y;
                    Some(TouchEvent {
                        kind: TouchEventKind::Hold,
                        x,
                        y,
                    })
                } else {
                    self.state = TouchState::Idle;
                    self.debounce_counter = 0;
                    Some(TouchEvent {
                        kind: TouchEventKind::Release,
                        x: self.last_x,
                        y: self.last_y,
                    })
                }
            }
        }
    }
}

// --- Hit test ---

/// Ekran koordinatında (x, y) en üstteki CLICKABLE widget'ı bul.
/// DFS pre-order = z-order (son eşleşen = en üst).
pub fn hit_test(tree: &WidgetTree, x: u16, y: u16) -> WidgetId {
    let (dfs, count) = tree.dfs_order();
    let mut result = WidgetId::NONE;

    for i in 0..count {
        let id = dfs[i];
        let w = tree.get(id);
        if w.flags & FLAG_CLICKABLE == 0 || w.flags & FLAG_VISIBLE == 0 {
            continue;
        }
        let abs = tree.absolute_rect(id);
        if (x as i16) >= abs.x
            && (x as i16) < abs.right()
            && (y as i16) >= abs.y
            && (y as i16) < abs.bottom()
        {
            result = id; // Son eşleşen = en üstteki
        }
    }

    result
}

// === Düşük seviye SPI ===

/// Dokunma oku: basınç yeterliyse kalibre edilmiş (x, y) döndür.
fn read_touch() -> Option<(u16, u16)> {
    // Basınç kontrolü
    let z1 = read_channel(CMD_Z1);
    let z2 = read_channel(CMD_Z2);

    // Basınç formülü (basitleştirilmiş): z1 büyük, z2 küçük → basınç var
    // z1 > threshold VE z2 < (4095 - threshold) → dokunma var
    if z1 < PRESSURE_THRESHOLD || z2 > (4095 - PRESSURE_THRESHOLD) {
        return None;
    }

    // X ve Y oku — median filtre
    let raw_x = read_filtered(CMD_X);
    let raw_y = read_filtered(CMD_Y);

    // Kalibrasyon: ham ADC → ekran pikseli
    let x = calibrate(raw_x, CAL_X_MIN, CAL_X_MAX, SCREEN_W);
    let y = calibrate(raw_y, CAL_Y_MIN, CAL_Y_MAX, SCREEN_H);

    Some((x, y))
}

/// N sample oku, sırala, median al.
fn read_filtered(cmd: u8) -> u16 {
    let mut samples = [0u16; SAMPLE_COUNT];
    for s in samples.iter_mut() {
        *s = read_channel(cmd);
    }

    // Insertion sort (N küçük, branch-friendly)
    for i in 1..SAMPLE_COUNT {
        let key = samples[i];
        let mut j = i;
        while j > 0 && samples[j - 1] > key {
            samples[j] = samples[j - 1];
            j -= 1;
        }
        samples[j] = key;
    }

    // Median
    samples[SAMPLE_COUNT / 2]
}

/// Ham ADC değerini ekran piksel koordinatına dönüştür.
fn calibrate(raw: u16, cal_min: u16, cal_max: u16, screen_size: u16) -> u16 {
    if raw <= cal_min {
        return 0;
    }
    if raw >= cal_max {
        return screen_size - 1;
    }

    let range = (cal_max - cal_min) as u32;
    let offset = (raw - cal_min) as u32;
    let result = offset * (screen_size as u32) / range;

    result as u16
}

/// XPT2046'dan tek kanal oku (12-bit sonuç).
fn read_channel(cmd: u8) -> u16 {
    pin_low(CS_PIN);

    // 8-bit komut gönder (MSB first)
    spi_write_byte(cmd);

    // 1 clock busy cycle
    pin_high(CLK_PIN);
    spin(4);
    pin_low(CLK_PIN);
    spin(4);

    // 12-bit sonuç oku (MSB first)
    let mut result: u16 = 0;
    for _ in 0..12 {
        pin_high(CLK_PIN);
        spin(4);
        result <<= 1;
        if pin_read(MISO_PIN) {
            result |= 1;
        }
        pin_low(CLK_PIN);
        spin(4);
    }

    // 3 clock daha — 24 clock cycle tamamla
    for _ in 0..3 {
        pin_high(CLK_PIN);
        spin(4);
        pin_low(CLK_PIN);
        spin(4);
    }

    pin_high(CS_PIN);

    result
}

/// 8-bit SPI yaz (MSB first).
fn spi_write_byte(byte: u8) {
    for i in (0..8).rev() {
        if byte & (1 << i) != 0 {
            pin_high(MOSI_PIN);
        } else {
            pin_low(MOSI_PIN);
        }
        pin_high(CLK_PIN);
        spin(4);
        pin_low(CLK_PIN);
        spin(4);
    }
}

// === GPIO yardımcıları ===

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

/// Kısa gecikme — SPI timing için.
#[inline(always)]
fn spin(cycles: u32) {
    for _ in 0..cycles {
        cortex_m::asm::nop();
    }
}
