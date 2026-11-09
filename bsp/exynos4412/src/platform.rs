//! `Exynos4412Platform` — binds the backends to ferrite-core and implements
//! the runtime hooks.

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::standby::{self, PowerButton, Press};
use exynos_4412_rs::{println, soc};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::heap;
use ferrite_core::ctx::Ctx;
use ferrite_core::platform::Platform;
use core::sync::atomic::{AtomicU32, Ordering};

use ferrite_core::pool::ObjPool;
use ferrite_core::sysinfo;
use ferrite_core::runtime::{ERR_EMPTY_RUN, FullInput, PlatformRuntime};
use ferrite_core::touch::{TouchBackend, TouchImpl};
use ferrite_core::types::{COLOR_BLACK, COLOR_RED, COLOR_WHITE};

use crate::lcd::{self, PANEL};

const SYS_REBOOT: u8 = 0x01;

/// Main-loop passes since boot (`sysinfo::SYS_PASSES`), counted in `frame_end`.
static PASSES: AtomicU32 = AtomicU32::new(0);

/// Main-loop passes since boot (for `net`'s status page).
#[cfg(feature = "wifi")]
pub fn passes() -> u32 {
    PASSES.load(Ordering::Relaxed)
}

/// Backlight at boot, in percent.
const BOOT_BRIGHTNESS: u8 = 50;

pub struct Exynos4412Platform;

impl Platform for Exynos4412Platform {
    type LcdB = lcd::FimdLcd;
    type FlashB = crate::flash::RamFlash;
    type TouchB = crate::touch::MxtTouch;
    type BacklightB = crate::backlight::GpioBacklight;
    type RtcB = crate::rtc::PmicRtc;
    type UsartB = crate::usart::HwUart;
    type SystickB = crate::systick::MctSystick;
    type GpioB = ferrite_core::gpio::NoGpio;
    type EepromB = ferrite_core::eeprom::NoEeprom;
    type AudioB = crate::audio::I2sAudio;
    type Renderer = crate::renderer::ExynosRenderer;

    // Default layout (config sector 0, FS from 0x1000) inside the RAM region.

    // WM8758 codec on I2S0 + pdma0 (audio.rs). JPEG → software decoder behind
    // `jpeg` later (no HW codec driver yet). Mirrored in
    // tools/devices/exynos4412.json (caps.*).
    const CAPS: u32 = ferrite_core::platform::CAP_AUDIO
        | ferrite_core::platform::CAP_MULTI_PROGRAM
        | ferrite_core::platform::CAP_SUPEROPS;

    const ANIM_SLOTS: usize = 64;
    // Glyph bitmaps in RAM at font load: text drawing then needs no
    // `FlashBackend` call (bounds check + copy out of the region) per glyph.
    const CACHE_FONT_BITMAPS: bool = true;

    // 1 GB of DRAM (512 MB heap): pools far past the pool_large defaults
    // (2 MB / 8192), and the deepest VM stacks the u8 indices allow. A
    // program's `"memory"` block still overrides each one. 256K object
    // pointers (`wide_obj`): 1 MB table + 1 MB DFS cache.
    const DEFAULT_ARENA_BYTES: usize = 16 << 20;
    const DEFAULT_MAX_OBJECTS: usize = 256 * 1024;
    const DEFAULT_EVAL_STACK: usize = 255;
    const DEFAULT_CALL_STACK: usize = 255;
    const DEFAULT_CB_QUEUE: usize = 64;
    // RAM-backed: a CRC of the image takes a few ms.
    const VERIFY_FS_ON_BOOT: bool = true;
}

static mut KEYS: Option<PowerKeys> = None;
/// Short presses of the power key → [`standby`]. Shares the pin with
/// [`KEYS`] (both read it as a plain input): KEYS acts on 2 s holds, the
/// button on presses released before that.
static mut BUTTON: Option<PowerButton> = None;

/// Temporary: a status line at the bottom of the screen for the writefs
/// "flash image corrupted" investigation. Remove once resolved.
/// Off: it CRCs the whole FS image on every received byte, which skews the
/// `perf` profile (each query sends bytes).
const DIAG: bool = false;
static mut DIAG_LAST: [u32; 7] = [u32::MAX; 7];

/// `rx` bytes received / dropped, UART errors by kind (`ovr` overrun = IRQ
/// too late, bytes lost; `par` parity; `frm` frame = corrupted on the line /
/// baud mismatch; `brk` break), and the CRC-32 of what landed in the RAM
/// flash from FS_BASE to the highest written address (zlib-compatible:
/// compare with `zlib.crc32(open(img,'rb').read())`).
fn draw_diag(ctx: &mut Ctx<Exynos4412Platform>) {
    use core::fmt::Write;
    use core::sync::atomic::Ordering::Relaxed;
    use crate::usart::{RX_DROPPED, RX_ERR, RX_TOTAL};
    let end = unsafe { crate::flash::WRITE_END };
    let now = [
        RX_TOTAL.load(Relaxed),
        RX_DROPPED.load(Relaxed),
        RX_ERR[0].load(Relaxed),
        RX_ERR[1].load(Relaxed),
        RX_ERR[2].load(Relaxed),
        RX_ERR[3].load(Relaxed),
        end as u32,
    ];
    if unsafe { DIAG_LAST } == now {
        return;
    }
    unsafe { DIAG_LAST = now };
    let [rx, drop, ovr, par, frm, brk, _] = now;
    let base = Exynos4412Platform::FS_BASE as usize;
    let len = end.saturating_sub(base);
    let crc = crate::flash::crc(base, end);
    let mut text = alloc::string::String::new();
    let _ = write!(
        text,
        "rx={rx} drop={drop} ovr={ovr} par={par} frm={frm} brk={brk} fs_len={len} crc={crc:08x}"
    );
    let y = PANEL.height as u16 - 24;
    ctx.lcd.fill_rect(0, y, PANEL.width as u16, 24, COLOR_BLACK);
    let font = ctx.fonts.embedded();
    font.draw_str(&ctx.lcd, &ctx.flash, text.as_bytes(), 8, y as i16 + 16, 0xFFE0, Some(COLOR_BLACK));
}

impl PlatformRuntime for Exynos4412Platform {
    const BG_COLOR: u16 = COLOR_BLACK;
    const FG_COLOR: u16 = COLOR_WHITE;
    // Cortex-A9 at 1.4 GHz: long bursts per main-loop pass.
    const VM_STEPS_PER_TICK: u32 = 20_000;
    // Programs up to 16 MB are copied to RAM (larger ones run from the region).
    const MAX_CODE_SIZE: usize = 16 << 20;
    type Input = FullInput;

    fn reset() -> ! {
        // A reply to a network client (bridge.rs: the pong of a writefs or a
        // Restart) leaves before the board goes away.
        if crate::bridge::active() {
            crate::bridge::tx_drain(500, crate::systick::millis);
            let t = crate::systick::millis();
            while crate::systick::millis().wrapping_sub(t) < 200 {
                core::hint::spin_loop();
            }
        }
        // A finished writefs (or a config write) reaches the eMMC first.
        crate::flash::save_if_dirty(true);
        exynos_4412_rs::console::flush();
        power::restart(BootMode::Normal)
    }

    fn syscall(id: u8, args: &[i32], objs: &mut ObjPool) -> Option<i32> {
        match id {
            SYS_REBOOT => Self::reset(),
            crate::battery::SYS_BATTERY_MV | crate::battery::SYS_BATTERY_PERCENT | crate::battery::SYS_CHARGER => {
                crate::battery::syscall(id)
            }
            crate::model3d::SYS_SHAPE..=crate::model3d::SYS_MESH => crate::model3d::syscall(id, args, objs),
            _ => sysinfo::syscall(id, args, objs, || {
                let heap = heap::stats();
                sysinfo::Info {
                    passes: PASSES.load(Ordering::Relaxed),
                    frames: lcd::frames(),
                    heap_used: heap.used,
                    heap_total: heap.size,
                    cpu_mhz: crate::cpuclock::mhz(),
                }
            }),
        }
    }

    fn heap_stats() -> (usize, usize) {
        let s = heap::stats();
        (s.size - s.used, 0)
    }

    fn heap_total() -> usize {
        heap::stats().size
    }

    #[cfg(feature = "perf")]
    fn cycles() -> u32 {
        crate::perf::cycles()
    }

    #[cfg(feature = "perf")]
    fn cycle_hz() -> u32 {
        crate::perf::hz()
    }

    fn render_due() -> bool {
        lcd::render_due()
    }

    #[cfg(feature = "perf")]
    fn perf_take_wait() -> u64 {
        crate::perf::take_wait()
    }

    fn boot(ctx: &mut Ctx<Self>, touch: &mut TouchImpl<Self::TouchB>) -> u8 {
        touch.cal = crate::touch::CAL;
        unsafe { *(&raw mut KEYS) = Some(PowerKeys::new()) };
        unsafe { *(&raw mut BUTTON) = Some(PowerButton::new()) };

        ctx.lcd.fill_rect(0, 0, PANEL.width as u16, PANEL.height as u16, COLOR_BLACK);
        // Black on screen before the backlight comes up. Half brightness, like the HAL's
        // `DEFAULT_BRIGHTNESS`: full drains the battery and heats the panel; programs set
        // their own with `setBrightness`.
        lcd::flush_wait();
        ctx.backlight.set_brightness(BOOT_BRIGHTNESS);

        // Nothing saved on CACHE yet (or storage off): recovery, so the host
        // can writefs.
        if recovery_requested(ctx) || !fs_present(ctx) {
            recovery_screen(ctx);
            return ERR_EMPTY_RUN;
        }
        0
    }

    fn frame_end(ctx: &mut Ctx<Self>) {
        PASSES.fetch_add(1, Ordering::Relaxed);
        if DIAG {
            draw_diag(ctx);
        }
        // Show what this pass drew (shadow -> scanned buffer, at a vsync).
        lcd::flush();
        // The compositor switched on/off over HTTP: repaint everything.
        crate::compose::service(ctx);
        // A CPU clock asked for over HTTP.
        crate::cpuclock::service();
        // Config writes (touch calibration…): save once they settle.
        crate::flash::save_if_dirty(false);
        // An FS image uploaded over HTTP, a restart request (net/).
        #[cfg(feature = "wifi")]
        crate::net::service();
        // FIMD window 1 test picture (HTTP `POST /api/overlay`).
        #[cfg(feature = "wifi")]
        crate::overlay_test::service();
        // Power key: hold = off, + vol up = restart, + vol down = fastboot.
        if let Some(keys) = unsafe { (*(&raw mut KEYS)).as_mut() } {
            keys.service(Mct::new());
        }
        // Short press = light standby until the next press.
        if let Some(button) = unsafe { (*(&raw mut BUTTON)).as_mut() }
            && button.poll(Mct::new()) == Some(Press::Short)
        {
            enter_standby(ctx, button);
        }
    }
}

/// Light standby (HAL `standby::enter`): backlight, LVDS and FIMD off, CPU
/// at 200 MHz, CPU0 asleep in `wfi` (IRQs masked) until the power key.
/// The runtime simply resumes on the next pass.
///
/// What the HAL leaves to the caller:
/// - display: no present may be in flight (its G2D / vsync completion is a
///   CPU0 interrupt): `flush_wait` first.
/// - audio: stopped, its ring silenced: the DMA keeps looping the ring while
///   the refill interrupt (CPU0) cannot run.
/// - WiFi stays joined; CPU3 keeps serving at the lower clock. Its timers
///   need the MCT comparator interrupt, which normally goes to CPU0: during
///   standby it goes to CPU0 and CPU3, so CPU3 services it and CPU0 is only
///   woken by it (which also lets the HAL time a long press). HTTP requests
///   that need the runtime (bridge, `/flash`) wait until the wake.
/// - UART2 RX runs on CPU1 and keeps filling its ring. Touch is polled, so
///   nothing touched meanwhile is delivered.
fn enter_standby(ctx: &mut Ctx<Exynos4412Platform>, button: &mut PowerButton) {
    let mct = Mct::new();
    lcd::flush_wait();
    ctx.audio.stop();
    let gic = Gic::new();
    let shared_timer = crate::net_running();
    if shared_timer {
        gic.set_targets(soc::MCT_G0_IRQ, 0b1001);
    }
    let pmic = Max77686::new(I2c::new(Bus::I2c0));
    let result = standby::enter(mct, Some(pmic), button);
    // Asleep for possibly longer than the counter's low word wraps (~179 s).
    crate::systick::resync();
    if shared_timer {
        gic.set_target(soc::MCT_G0_IRQ, 0);
    }
    match result {
        Ok(r) => {
            println!(
                "ferrite: standby {} s, asleep {}%, {} -> {} -> {} MHz",
                r.standby_us / 1_000_000,
                r.slept_us * 100 / r.standby_us.max(1),
                r.before_hz / 1_000_000,
                r.standby_hz / 1_000_000,
                r.after_hz / 1_000_000
            );
            if r.press == Press::Long {
                // Held 2 s while asleep: power off, as when awake.
                crate::flash::save_if_dirty(true);
                exynos_4412_rs::console::flush();
                power::power_off();
            }
        }
        Err(e) => println!("ferrite: standby ERROR {:?}", e),
    }
}

fn fs_present(ctx: &Ctx<Exynos4412Platform>) -> bool {
    let mut magic = [0u8; 4];
    ctx.flash.read(Exynos4412Platform::FS_BASE, &mut magic);
    &magic == b"FERR"
}

/// Top-left 80×80 held for 3 s at boot.
fn recovery_requested(ctx: &mut Ctx<Exynos4412Platform>) -> bool {
    let be = crate::touch::MxtTouch;
    let cal = crate::touch::CAL;
    let held = || be.is_active() && matches!(be.read_screen(&cal), Some((x, y)) if x < 80 && y < 80);
    if !held() {
        return false;
    }
    let start = ctx.systick.millis();
    loop {
        let t = ctx.systick.millis().wrapping_sub(start);
        if t >= 3000 {
            return true;
        }
        ctx.lcd.fill_rect(440, 390, (t * 400 / 3000) as u16, 8, COLOR_RED);
        lcd::flush();
        if !held() {
            ctx.lcd.fill_rect(440, 390, 400, 8, COLOR_BLACK);
            lcd::flush();
            return false;
        }
        ctx.systick.delay_ms(20);
    }
}

fn recovery_screen(ctx: &mut Ctx<Exynos4412Platform>) {
    let (w, h) = (PANEL.width as i16, PANEL.height as i16);
    ctx.lcd.fill_rect(0, 0, w as u16, h as u16, COLOR_BLACK);
    let font = ctx.fonts.embedded();
    let line = |text: &[u8], y: i16, color: u16| {
        let x = w / 2 - font.text_width(text) as i16 / 2;
        font.draw_str(&ctx.lcd, &ctx.flash, text, x, y, color, Some(COLOR_BLACK));
    };
    line(b"RECOVERY MODE", h / 2 - 10, COLOR_RED);
    line(b"Use writefs to flash a new program", h / 2 + 20, 0x8410);
    lcd::flush_wait();
}
