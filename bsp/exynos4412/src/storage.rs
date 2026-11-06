//! Persistent resource storage — the FAT32 file system on the eMMC CACHE
//! partition (p8), through `exynos_4412_rs::fat` (as its `fat_test`).
//!
//! The ferrite "flash" (`flash.rs`) is a RAM region; this module keeps it in
//! one file, [`DIR`]/[`FILE`]:
//! - boot: [`init`] mounts CACHE and [`load`] reads the file into the region
//! - after writes: [`save_if_dirty`] writes the used part of the region back
//!   (from `reset`, so a finished `writefs` is kept before the restart, and
//!   from the main loop once writes have been quiet for [`IDLE_SAVE_MS`])
//!
//! CACHE is never formatted here: without a FAT32 on it persistence is off
//! (logged at boot) and the board runs as before, from RAM only.
//!
//! The file is rewritten in place: power lost during a save leaves a torn
//! image, which the boot-time CRC check (`VERIFY_FS_ON_BOOT`) turns into
//! Code 9 + recovery, so a new `writefs` fixes it.

use alloc::boxed::Box;
#[cfg(feature = "wifi")]
use alloc::vec::Vec;
use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Rtc;
use exynos_4412_rs::drivers::mmc::{self, Mmc};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::fat::embedded_sdmmc::{Mode, VolumeIdx, VolumeManager};
use exynos_4412_rs::fat::{self, MctClock, PartitionDevice};
use exynos_4412_rs::println;
use exynos_4412_rs::time::DateTime;

/// CACHE.
const PARTITION: u32 = 8;
pub const DIR: &str = "FERRITE";
pub const FILE: &str = "FLASH.BIN";
/// Save this long after the last write (a writefs in progress keeps writing
/// every chunk, so it is not saved half done unless the host stalls).
pub const IDLE_SAVE_MS: u32 = 1500;
/// Device buffer as `fat_test`: read-ahead + single blocks + a 256 KiB write run.
const DEVICE_BYTES: usize = fat::MIN_BUFFER - 512 + 256 * 1024;
/// Addresses U-Boot may have given the card (it does not report it).
const RCA_GUESSES: [u16; 2] = [1, 2];
/// Clock start when the RTC holds no valid time (only file timestamps).
const FALLBACK: DateTime = DateTime { year: 2026, month: 1, day: 1, hour: 0, minute: 0, second: 0 };

type FsError = fat::embedded_sdmmc::Error<fat::Error>;
type Volumes = VolumeManager<PartitionDevice<'static>, MctClock, 4, 4, 1>;

#[repr(C, align(32))]
struct Buffer(UnsafeCell<[u8; DEVICE_BYTES]>);

// SAFETY: taken once, by `init`.
unsafe impl Sync for Buffer {}

static DEVICE: Buffer = Buffer(UnsafeCell::new([0; DEVICE_BYTES]));

/// The mounted CACHE volume manager; None = persistence off.
static mut VOLUMES: Option<Box<Volumes>> = None;

fn volumes() -> Option<&'static Volumes> {
    unsafe { (*(&raw const VOLUMES)).as_deref() }
}

/// Whether CACHE was mounted and saves are possible.
pub fn available() -> bool {
    volumes().is_some()
}

/// Opens the eMMC and checks CACHE holds a FAT32. Call once, after `heap::init`.
pub fn init() {
    match open() {
        Ok(v) => unsafe { *(&raw mut VOLUMES) = Some(v) },
        Err(why) => println!("ferrite: storage off ({}), resources live in RAM only", why),
    }
}

fn open() -> Result<Box<Volumes>, &'static str> {
    let card = Mmc::new().map_err(|_| "eMMC controller")?;
    match card.read_ext_csd() {
        Ok(ext) if ext.reads_user_area() => {}
        Ok(_) => return Err("eMMC not on its user area"),
        Err(_) => return Err("EXT_CSD read"),
    }
    let rca = RCA_GUESSES
        .into_iter()
        .find(|rca| card.read_status(*rca).is_ok_and(|s| mmc::card_state(s) == 4))
        .ok_or("no card in transfer state")?;
    // SAFETY: the only reference to the buffer, taken once.
    let buffer: &'static mut [u8] = unsafe { &mut *DEVICE.0.get() };
    let device = PartitionDevice::new(card, rca, PARTITION, buffer).map_err(|_| "CACHE partition")?;
    if !device.probe().map_err(|_| "CACHE read")? {
        return Err("no FAT32 on CACHE (not formatted here)");
    }
    let p = device.partition();
    println!("ferrite: CACHE p{} FAT32, {} MiB", p.number, p.count / 2048);
    Ok(Box::new(VolumeManager::new(device, clock())))
}

/// File timestamps: the PMIC RTC (which `settime` sets to the host's local time).
fn clock() -> MctClock {
    let mct = Mct::new();
    let now = Rtc::new(I2c::new(Bus::I2c0)).read(mct).ok().and_then(|r| r.time());
    MctClock::new(now.unwrap_or(FALLBACK), mct)
}

/// Reads the image file into `region`; returns its length (0 = no file yet).
pub fn load(region: &mut [u8]) -> usize {
    let Some(v) = volumes() else { return 0 };
    match read_file(v, FILE, region) {
        Ok(n) => {
            println!("ferrite: loaded {}/{}, {} bytes", DIR, FILE, n);
            n
        }
        Err(FsError::NotFound) => {
            println!("ferrite: no {}/{} yet", DIR, FILE);
            0
        }
        Err(e) => {
            println!("ferrite: load {}/{} ERROR {:?}", DIR, FILE, e);
            0
        }
    }
}

fn read_file(v: &Volumes, name: &str, region: &mut [u8]) -> Result<usize, FsError> {
    let volume = v.open_volume(VolumeIdx(0))?;
    let root = volume.open_root_dir()?;
    let dir = root.open_dir(DIR)?;
    let file = dir.open_file_in_dir(name, Mode::ReadOnly)?;
    let len = (file.length() as usize).min(region.len());
    let mut done = 0;
    while done < len {
        let n = file.read(&mut region[done..len])?;
        if n == 0 {
            break;
        }
        done += n;
    }
    file.close()?;
    Ok(done)
}

/// Reads [`DIR`]/`name` (an 8.3 name) whole, if it exists and is at most
/// `max` bytes. CPU0 only, like every other call here.
#[cfg(feature = "wifi")]
pub fn read_named(name: &str, max: usize) -> Option<Vec<u8>> {
    let v = volumes()?;
    let len = {
        let volume = v.open_volume(VolumeIdx(0)).ok()?;
        let root = volume.open_root_dir().ok()?;
        let dir = root.open_dir(DIR).ok()?;
        let file = dir.open_file_in_dir(name, Mode::ReadOnly).ok()?;
        let len = file.length() as usize;
        file.close().ok()?;
        len
    };
    if len > max {
        println!("ferrite: {}/{} too large ({} bytes)", DIR, name, len);
        return None;
    }
    let mut buf = alloc::vec![0u8; len];
    match read_file(v, name, &mut buf) {
        Ok(n) => {
            buf.truncate(n);
            Some(buf)
        }
        Err(_) => None,
    }
}

/// Writes `data` as the image file (made or truncated), then flushes the
/// device so it is on the card when this returns.
pub fn save(data: &[u8]) -> bool {
    save_named(FILE, data)
}

/// Writes [`DIR`]/`name` (an 8.3 name, made or truncated) and flushes.
pub fn save_named(name: &str, data: &[u8]) -> bool {
    let Some(v) = volumes() else { return false };
    match write_file(v, name, data).and_then(|()| Ok(v.device(|d| d.flush())?)) {
        Ok(()) => true,
        Err(e) => {
            println!("ferrite: save {}/{} ERROR {:?}", DIR, name, e);
            false
        }
    }
}

fn write_file(v: &Volumes, name: &str, data: &[u8]) -> Result<(), FsError> {
    let volume = v.open_volume(VolumeIdx(0))?;
    let root = volume.open_root_dir()?;
    match root.make_dir_in_dir(DIR) {
        Ok(()) | Err(FsError::DirAlreadyExists) => {}
        Err(e) => return Err(e),
    }
    let dir = root.open_dir(DIR)?;
    let file = dir.open_file_in_dir(name, Mode::ReadWriteCreateOrTruncate)?;
    file.write(data)?;
    file.close()?;
    Ok(())
}
