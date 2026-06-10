//! `NextionPlatform` — binds the GD32F103 / NX8048K070 backends to the
//! framework and implements the runtime hooks (`PlatformRuntime`) that
//! `ferrite_core::run` calls.

use ferrite_core::platform::Platform;
use ferrite_core::touch::{CalParams, TouchImpl};

use ferrite_core::ctx::Ctx;
use ferrite_core::protocol::RxEvent;
use ferrite_core::runtime::{ERR_EMPTY_RUN, FullInput, PlatformRuntime};
use ferrite_core::strpool::StringPool;
use ferrite_core::types::{COLOR_BLACK, COLOR_RED, COLOR_WHITE};
use ferrite_core::vm::Vm;
use ferrite_core::{config, fs, protocol, render};

use crate::gpio::Gpio;
use crate::systick::delay_ms;
use crate::{fat, sdcard, touch};

// === Device syscalls (GPIOB / FPGA bus poking from Ferrite programs) ===

const SYS_GPIOB_MODE: u8 = 0x30; // (0=input floating, 1=output PP 50MHz) -> 0
const SYS_GPIOB_READ: u8 = 0x31; // () -> GPIOB[15:0]
const SYS_GPIOB_WRITE: u8 = 0x32; // (u16 value) -> 0
const SYS_FPGA_CMD_ONLY: u8 = 0x33; // (cmd) -> 0; command strobe without data word
const SYS_FPGA_CLOCK: u8 = 0x34; // () -> 0; pulse PA12 once

/// Marker type implementing `Platform` + `PlatformRuntime` for the Nextion board.
pub struct NextionPlatform;

impl Platform for NextionPlatform {
    type LcdB = crate::lcd::hw::FpgaLcd;
    type FlashB = crate::flash::hw::FpgaFlash;
    type TouchB = crate::touch::hw::XptTouch;
    type BacklightB = crate::backlight::hw::Pwm;
    type RtcB = crate::rtc::hw::At8563;
    type SdCardB = crate::sdcard::hw::SpiSd;
    type UsartB = crate::usart::hw::Hw;
    type SystickB = crate::systick::Gd32Systick;
}

impl PlatformRuntime for NextionPlatform {
    const BG_COLOR: u16 = COLOR_BLACK;
    const FG_COLOR: u16 = COLOR_WHITE;
    type Input = FullInput;

    fn reset() -> ! {
        cortex_m::peripheral::SCB::sys_reset();
    }

    fn syscall(id: u8, args: &[i32], _strpool: &StringPool) -> Option<i32> {
        match id {
            SYS_GPIOB_MODE => {
                if args.len() != 1 {
                    return None;
                }
                match args[0] {
                    0 => Gpio::set_data_bus_input(),
                    1 => Gpio::set_data_bus_output(),
                    _ => return None,
                }
                Some(0)
            }
            SYS_GPIOB_READ => {
                if !args.is_empty() {
                    return None;
                }
                Some(Gpio::read_data_bus() as i32)
            }
            SYS_GPIOB_WRITE => {
                if args.len() != 1 {
                    return None;
                }
                Gpio::write_data_bus_raw(args[0] as u16);
                Some(0)
            }
            SYS_FPGA_CMD_ONLY => {
                if args.len() != 1 {
                    return None;
                }
                Gpio::set_data_bus_output();
                Gpio::set_cmd_data_raw(false);
                Gpio::write_data_bus_raw(args[0] as u16);
                Gpio::clock_pulse_raw();
                Some(0)
            }
            SYS_FPGA_CLOCK => {
                if !args.is_empty() {
                    return None;
                }
                Gpio::clock_pulse_raw();
                Some(0)
            }
            _ => None,
        }
    }

    fn stack_info() -> (u32, u32) {
        let sp: u32;
        unsafe { core::arch::asm!("mov {}, sp", out(reg) sp) };
        const RAM_END: u32 = 0x2000_5000;
        let used = RAM_END.saturating_sub(sp);
        let free = sp.saturating_sub(0x2000_0000);
        (used, free)
    }

    fn boot(ctx: &mut Ctx<Self>, touch: &mut TouchImpl<Self::TouchB>) -> u8 {
        // Load saved touch calibration from config (flash sector 0).
        let cfg = config::ConfigStore::mount_or_format(&ctx.flash);
        let mut cal_buf = [0u8; 9];
        if let Some(len) = cfg.read(&ctx.flash, config::KEY_TOUCH_CAL, &mut cal_buf) {
            if len >= 9 {
                if let Some(cal) = CalParams::from_bytes(&cal_buf) {
                    touch.cal = cal;
                }
            }
        }

        // Startup: clear both buffers black, then bring the backlight up.
        delay_ms(500);
        ctx.lcd.fill_rect(0, 0, 800, 480, COLOR_BLACK);
        ctx.lcd.begin_frame();
        ctx.lcd.fill_rect(0, 0, 800, 480, COLOR_BLACK);
        ctx.lcd.end_frame();
        ctx.backlight.set_brightness(100);

        // Recovery mode: hold the top-left corner for 3 seconds at boot.
        if touch::penirq_active_pub() && touch::check_recovery_touch(&touch.cal, 200) {
            if recovery_hold(ctx, &touch.cal) {
                return ERR_EMPTY_RUN;
            }
        }

        // SD card boot: flash PROGRAM.BIN or enter recovery via EMPTY.BIN.
        sd_boot_check(ctx)
    }

    fn on_extra_rx(
        ev: RxEvent,
        ctx: &mut Ctx<Self>,
        vm: &mut Vm,
        touch: &mut TouchImpl<Self::TouchB>,
    ) -> bool {
        match ev {
            RxEvent::TouchCalibrate => {
                let cal = touch::run_calibration(touch, &ctx.lcd);
                let mut cfg = config::ConfigStore::mount_or_format(&ctx.flash);
                cfg.write(&ctx.flash, config::KEY_TOUCH_CAL, &cal.to_bytes());
                protocol::send_touch_cal(&ctx.usart, &cal);
                render::render_all(ctx, vm);
                true
            }
            _ => false,
        }
    }
}

/// Animate the recovery progress bar; return `true` if held the full 3 seconds.
fn recovery_hold(ctx: &mut Ctx<NextionPlatform>, cal: &CalParams) -> bool {
    let start = crate::systick::millis();
    let hold_ms: u32 = 3000;
    let bar_h: u16 = 6;
    let mut entered = false;

    loop {
        let elapsed = crate::systick::millis().wrapping_sub(start);
        if elapsed >= hold_ms {
            entered = true;
            break;
        }
        let fill = ((elapsed * 800) / hold_ms) as u16;
        ctx.lcd.fill_rect(0, 0, fill, bar_h, COLOR_RED);
        if !touch::check_recovery_touch(cal, 50) {
            break; // released or moved out
        }
    }

    if entered {
        ctx.lcd.fill_rect(0, 0, 800, bar_h, COLOR_RED);
        ctx.lcd.fill_rect(0, bar_h, 800, 480 - bar_h, COLOR_BLACK);
        ctx.fonts.embedded().draw_str(
            &ctx.lcd,
            &ctx.flash,
            b"RECOVERY MODE",
            290,
            230,
            COLOR_RED,
            Some(COLOR_BLACK),
        );
        ctx.fonts.embedded().draw_str(
            &ctx.lcd,
            &ctx.flash,
            b"Use writefs to flash new program",
            210,
            260,
            0x4208,
            Some(COLOR_BLACK),
        );
        true
    } else {
        ctx.lcd.fill_rect(0, 0, 800, bar_h, COLOR_BLACK);
        false
    }
}

/// SD card boot: EMPTY.BIN → recovery, PROGRAM.BIN → flash update.
/// Returns `ERR_EMPTY_RUN` for recovery, otherwise 0.
fn sd_boot_check(ctx: &Ctx<NextionPlatform>) -> u8 {
    if !sdcard::probe() {
        return 0; // no SD card — continue normal boot
    }

    let font = ctx.fonts.embedded();
    font.draw_str(
        &ctx.lcd,
        &ctx.flash,
        b"SD card found...",
        300,
        235,
        0x4208,
        Some(0x0000),
    );

    let sd = match sdcard::init() {
        Ok(sd) => sd,
        Err(_) => return 0,
    };

    let mut fat = match fat::Fat::mount(&sd) {
        Ok(f) => f,
        Err(_) => {
            sd.release_bus();
            return 0;
        }
    };

    // EMPTY.BIN — recovery mode.
    if fat.find_by_name(&sd, b"EMPTY.BIN").is_some() {
        ctx.lcd.fill_rect(0, 200, 800, 30, 0x0000);
        font.draw_str(
            &ctx.lcd,
            &ctx.flash,
            b"SD: recovery mode",
            280,
            220,
            COLOR_WHITE,
            Some(0x0000),
        );
        sd.release_bus();
        delay_ms(1000);
        return ERR_EMPTY_RUN;
    }

    // PROGRAM.BIN — flash update.
    let entry = match fat.find_by_name(&sd, b"PROGRAM.BIN") {
        Some(e) => e,
        None => {
            sd.release_bus();
            return 0;
        }
    };

    let file_size = entry.size;
    if file_size == 0 {
        sd.release_bus();
        return 0;
    }

    ctx.lcd.fill_rect(0, 200, 800, 60, 0x0000);
    font.draw_str(
        &ctx.lcd,
        &ctx.flash,
        b"SD: flashing program...",
        250,
        220,
        COLOR_WHITE,
        Some(0x0000),
    );

    const SECTOR: usize = 4096;
    const PAGE: usize = 256;
    let mut buf = alloc::vec![0u8; SECTOR];
    let mut offset: u32 = 0;
    let dest_base: u32 = fs::FS_BASE;

    while offset < file_size {
        let remaining = (file_size - offset) as usize;
        let chunk = remaining.min(SECTOR);

        let read = fat.read_file(&sd, &entry, offset, &mut buf[..chunk]);
        if read == 0 {
            break;
        }

        ctx.flash.erase_sector(dest_base + offset);

        let mut page_off = 0;
        while page_off < read {
            let page_len = (read - page_off).min(PAGE);
            ctx.flash.write(
                dest_base + offset + page_off as u32,
                &buf[page_off..page_off + page_len],
            );
            page_off += page_len;
        }

        offset += read as u32;
        let progress = (offset * 700 / file_size) as u16;
        ctx.lcd.fill_rect(50, 240, progress, 10, 0x07E0);
    }

    sd.release_bus();

    ctx.lcd.fill_rect(0, 200, 800, 60, 0x0000);
    font.draw_str(
        &ctx.lcd,
        &ctx.flash,
        b"SD: flash complete!",
        270,
        220,
        0x07E0,
        Some(0x0000),
    );
    delay_ms(1000);

    0
}
