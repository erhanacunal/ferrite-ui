/// SD card boot actions, shared by the BSPs that have a card slot.
///
/// With a FAT16/FAT32 card in the slot at boot:
/// - `EMPTY.BIN` in the root → recovery mode (returns `ERR_EMPTY_RUN`)
/// - `PROGRAM.BIN` in the root → copied to the resource flash at
///   `P::FS_BASE` (the same image `writefs` uploads), then normal boot
///
/// The BSP detects and initializes the card; this module does the rest and
/// releases the bus when done.
extern crate alloc;

use crate::ctx::Ctx;
use crate::fat::Fat;
use crate::platform::Platform;
use crate::runtime::ERR_EMPTY_RUN;
use crate::sdcard::SdCardImpl;
use crate::systick::SystickBackend;
use crate::types::COLOR_WHITE;

/// Run the SD boot actions on an initialized card. Returns `ERR_EMPTY_RUN`
/// for recovery, otherwise 0. `present` makes what was just drawn visible
/// (cached frame buffers need a write-back; direct-to-panel boards pass a
/// no-op).
pub fn sd_boot_check<P: Platform>(
    ctx: &Ctx<P>,
    sd: &SdCardImpl<P::SdCardB>,
    present: fn(),
) -> u8 {
    let font = ctx.fonts.embedded();

    let mut fat = match Fat::mount(sd) {
        Ok(f) => f,
        Err(_) => {
            sd.release_bus();
            return 0;
        }
    };

    // EMPTY.BIN — recovery mode.
    if fat.find_by_name(sd, b"EMPTY.BIN").is_some() {
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
        present();
        sd.release_bus();
        P::SystickB::delay_ms(1000);
        return ERR_EMPTY_RUN;
    }

    // PROGRAM.BIN — flash update.
    let entry = match fat.find_by_name(sd, b"PROGRAM.BIN") {
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
    present();

    const SECTOR: usize = 4096;
    const PAGE: usize = 256;
    let mut buf = alloc::vec![0u8; SECTOR];
    let mut offset: u32 = 0;
    let dest_base: u32 = P::FS_BASE;

    while offset < file_size {
        let remaining = (file_size - offset) as usize;
        let chunk = remaining.min(SECTOR);

        let read = fat.read_file(sd, &entry, offset, &mut buf[..chunk]);
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
        present();
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
    present();
    P::SystickB::delay_ms(1000);
    0
}
