//! Wi-Fi station + HTTP(S) / SNTP client for ferrite programs.
//!
//! The ESP32-S3 Wi-Fi driver (`esp-radio`) and the IP stack (`embassy-net`,
//! `reqwless`) are async-only, while the ferrite runtime is a blocking
//! super-loop. So the network side runs as an embassy executor on **core 1**
//! (`task.rs`) and the VM on core 0 only talks to it through [`SHARED`], a
//! mailbox behind a critical-section mutex:
//!
//! * core 0 (syscalls, `platform.rs`) writes requests (credentials, URL, NTP
//!   server) and pokes [`WAKE`]; every syscall returns immediately.
//! * core 1 picks the request up, does the slow work and writes the result
//!   (state, HTTP body, NTP time) back; the program polls a status syscall.
//!
//! Nothing about the network is stored in the firmware: SSID, password, URLs
//! and the optional TLS CA come from the program at run time.

mod json;
pub mod task;

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use allocator_api2::vec::Vec as PsVec;
use critical_section::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use esp_alloc::ExternalMemory;

use ferrite_core::pool::ObjPool;
use ferrite_core::rtc::DateTime;

// --- Wi-Fi state (SYS_WIFI_STATUS) ---
pub const WIFI_OFF: i32 = 0;
pub const WIFI_CONNECTING: i32 = 1;
pub const WIFI_CONNECTED: i32 = 2;
pub const WIFI_FAILED: i32 = 3;

// --- Request state (SYS_HTTP_STATUS / SYS_NTP_STATUS); errors are < 0 ---
pub const OP_IDLE: i32 = 0;
pub const OP_RUNNING: i32 = 1;
pub const OP_DONE: i32 = 2;

// --- Error codes (kept in sync with lib/epaper.fl NET_ERR_*) ---
pub const ERR_BUSY: i32 = -1;
pub const ERR_ARG: i32 = -2;
pub const ERR_OFFLINE: i32 = -3;
pub const ERR_DNS: i32 = -10;
pub const ERR_NET: i32 = -11;
pub const ERR_TLS: i32 = -12;
pub const ERR_HTTP: i32 = -13;
pub const ERR_TOO_LARGE: i32 = -14;
pub const ERR_TIMEOUT: i32 = -15;
pub const ERR_NO_MEM: i32 = -16;

/// Largest HTTP response (headers + body) kept in PSRAM.
pub const HTTP_MAX_RESPONSE: usize = 64 * 1024;
/// Largest URL / NTP host name accepted from a program.
const MAX_URL: usize = 512;

/// PSRAM byte buffer (big buffers stay out of the internal RAM Wi-Fi needs).
pub type PsBuf = PsVec<u8, ExternalMemory>;

/// Allocate a zeroed PSRAM buffer, `None` when PSRAM is exhausted.
pub fn ps_buf(len: usize) -> Option<PsBuf> {
    let mut v = PsVec::new_in(ExternalMemory);
    v.try_reserve_exact(len).ok()?;
    v.resize(len, 0);
    Some(v)
}

pub struct Shared {
    // --- Wi-Fi: requested by the VM ---
    /// Radio wanted on (a connect was requested and no disconnect since).
    pub want_on: bool,
    pub ssid: String,
    pub pass: String,
    /// Bumped on every connect / disconnect request, so the net task notices
    /// new credentials even while it is busy connecting.
    pub generation: u32,
    // --- Wi-Fi: reported by the net task ---
    pub wifi: i32,
    pub rssi: i32,
    /// IPv4 address, big-endian (`a.b.c.d` = `a << 24 | …`), 0 = none.
    pub ip: u32,
    /// First DNS server from DHCP, same encoding, 0 = none offered.
    pub dns: u32,

    // --- HTTP ---
    pub http_state: i32,
    /// A request is waiting for the net task to pick it up.
    pub http_pending: bool,
    pub http_url: String,
    /// DER CA certificate for `https://` verification (`SYS_HTTPS_CA`).
    /// `None` = TLS without server authentication.
    pub http_ca: Option<Vec<u8>>,
    pub http_code: i32,
    pub http_body: Option<PsBuf>,

    // --- SNTP ---
    pub ntp_state: i32,
    pub ntp_pending: bool,
    pub ntp_server: String,
    /// Local time = UTC + this many minutes, applied to the RTC.
    pub ntp_tz_minutes: i32,
    /// UTC seconds at `ntp_at_ms` (core-0 `systick::millis()`), 0 = never.
    pub ntp_unix: u32,
    pub ntp_at_ms: u32,
    /// Set by the net task; core 0 writes the RTC in `frame_end`.
    pub ntp_apply: bool,
}

impl Shared {
    const fn new() -> Self {
        Self {
            want_on: false,
            ssid: String::new(),
            pass: String::new(),
            generation: 0,
            wifi: WIFI_OFF,
            rssi: 0,
            ip: 0,
            dns: 0,
            http_state: OP_IDLE,
            http_pending: false,
            http_url: String::new(),
            http_ca: None,
            http_code: 0,
            http_body: None,
            ntp_state: OP_IDLE,
            ntp_pending: false,
            ntp_server: String::new(),
            ntp_tz_minutes: 0,
            ntp_unix: 0,
            ntp_at_ms: 0,
            ntp_apply: false,
        }
    }
}

pub static SHARED: Mutex<RefCell<Shared>> = Mutex::new(RefCell::new(Shared::new()));

/// Wakes the net task after a request was written to [`SHARED`].
pub static WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub fn with<R>(f: impl FnOnce(&mut Shared) -> R) -> R {
    critical_section::with(|cs| f(&mut SHARED.borrow_ref_mut(cs)))
}

fn str_arg(pool: &ObjPool, id: i32) -> Option<&[u8]> {
    u16::try_from(id).ok().map(|id| pool.str_get(id))
}

fn utf8_arg(pool: &ObjPool, id: i32, max: usize) -> Option<String> {
    let bytes = str_arg(pool, id)?;
    if bytes.len() > max {
        return None;
    }
    core::str::from_utf8(bytes).ok().map(String::from)
}

// ============================================================================
//  Syscall side (core 0) — every call returns immediately
// ============================================================================

/// `SYS_WIFI_CONNECT(ssid, pass)`: power the radio up (if off) and join the
/// network; the net task keeps reconnecting while the radio is wanted on.
/// New credentials while connected switch networks.
pub fn wifi_connect(pool: &ObjPool, ssid: i32, pass: i32) -> i32 {
    let (Some(ssid), Some(pass)) = (utf8_arg(pool, ssid, 32), utf8_arg(pool, pass, 64)) else {
        return ERR_ARG;
    };
    if ssid.is_empty() || (!pass.is_empty() && pass.len() < 8) {
        return ERR_ARG;
    }
    with(|s| {
        s.want_on = true;
        s.ssid = ssid;
        s.pass = pass;
        s.generation = s.generation.wrapping_add(1);
        s.wifi = WIFI_CONNECTING;
    });
    WAKE.signal(());
    0
}

/// `SYS_WIFI_DISCONNECT()`: leave the network and power the radio down.
pub fn wifi_disconnect() -> i32 {
    with(|s| {
        if s.want_on {
            s.want_on = false;
            s.generation = s.generation.wrapping_add(1);
        }
    });
    WAKE.signal(());
    0
}

pub fn wifi_status() -> i32 {
    with(|s| s.wifi)
}

pub fn wifi_rssi() -> i32 {
    with(|s| if s.wifi == WIFI_CONNECTED { s.rssi } else { 0 })
}

pub fn wifi_ip() -> i32 {
    with(|s| if s.wifi == WIFI_CONNECTED { s.ip as i32 } else { 0 })
}

/// `SYS_WIFI_IP_STR()` → `"a.b.c.d"` (`"0.0.0.0"` when not connected).
pub fn wifi_ip_str(pool: &mut ObjPool) -> Option<i32> {
    ip_str(pool, wifi_ip() as u32)
}

/// `SYS_WIFI_DNS_STR()` → the DNS server DHCP offered, `"0.0.0.0"` if none
/// (lookups then go straight to the public fallback servers).
pub fn wifi_dns_str(pool: &mut ObjPool) -> Option<i32> {
    let dns = with(|s| if s.wifi == WIFI_CONNECTED { s.dns } else { 0 });
    ip_str(pool, dns)
}

fn ip_str(pool: &mut ObjPool, ip: u32) -> Option<i32> {
    let mut buf = [0u8; 15];
    let mut n = 0;
    for (i, octet) in ip.to_be_bytes().iter().enumerate() {
        if i > 0 {
            buf[n] = b'.';
            n += 1;
        }
        n += fmt_u32(*octet as u32, &mut buf[n..]);
    }
    pool.str_alloc(&buf[..n]).map(|id| id as i32)
}

fn fmt_u32(mut v: u32, out: &mut [u8]) -> usize {
    let mut tmp = [0u8; 10];
    let mut n = 0;
    loop {
        tmp[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for i in 0..n {
        out[i] = tmp[n - 1 - i];
    }
    n
}

/// `SYS_HTTP_GET(url)`: start a GET request. One request at a time; the
/// previous body is dropped.
pub fn http_get(pool: &ObjPool, url: i32) -> i32 {
    let Some(url) = utf8_arg(pool, url, MAX_URL) else {
        return ERR_ARG;
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return ERR_ARG;
    }
    let rc = with(|s| {
        if s.http_pending || s.http_state == OP_RUNNING {
            return ERR_BUSY;
        }
        if s.wifi != WIFI_CONNECTED {
            return ERR_OFFLINE;
        }
        s.http_url = url;
        s.http_body = None;
        s.http_code = 0;
        s.http_state = OP_RUNNING;
        s.http_pending = true;
        0
    });
    if rc == 0 {
        WAKE.signal(());
    }
    rc
}

pub fn http_status() -> i32 {
    with(|s| s.http_state)
}

pub fn http_code() -> i32 {
    with(|s| s.http_code)
}

pub fn http_len() -> i32 {
    with(|s| s.http_body.as_ref().map_or(0, |b| b.len() as i32))
}

/// Run `f` on the received body (empty slice when there is none).
fn with_body<R>(f: impl FnOnce(&[u8]) -> R) -> R {
    with(|s| f(s.http_body.as_deref().unwrap_or(&[])))
}

/// `SYS_HTTP_BODY(start, len)` → new string with that byte range (clamped).
pub fn http_body(pool: &mut ObjPool, start: i32, len: i32) -> Option<i32> {
    // Copy out first: the pool must not be touched inside the critical section
    // longer than needed, and `str_alloc` may compact the arena.
    let bytes: Vec<u8> = with_body(|b| {
        let start = (start.max(0) as usize).min(b.len());
        let end = start.saturating_add(len.max(0) as usize).min(b.len());
        b[start..end].to_vec()
    });
    pool.str_alloc(&bytes).map(|id| id as i32)
}

pub fn http_byte(index: i32) -> i32 {
    with_body(|b| match usize::try_from(index) {
        Ok(i) if i < b.len() => b[i] as i32,
        _ => -1,
    })
}

/// `SYS_HTTP_FIND(needle, from)` → byte index of `needle` in the body at or
/// after `from`, or -1.
pub fn http_find(pool: &ObjPool, needle: i32, from: i32) -> i32 {
    let Some(needle) = str_arg(pool, needle) else {
        return -1;
    };
    with_body(|b| {
        let from = (from.max(0) as usize).min(b.len());
        json::find(&b[from..], needle).map_or(-1, |i| (from + i) as i32)
    })
}

/// `SYS_HTTP_JSON(path)` → string value at a dotted key path (`"current.temp"`),
/// or the empty string when it is missing.
pub fn http_json(pool: &mut ObjPool, path: i32) -> Option<i32> {
    let path: Vec<u8> = str_arg(pool, path)?.to_vec();
    let value: Vec<u8> = with_body(|b| json::lookup(b, &path).map(|v| v.to_vec()).unwrap_or_default());
    pool.str_alloc(&value).map(|id| id as i32)
}

/// `SYS_HTTP_JSON_NUM(path, scale)` → the number at `path` × `scale`, rounded
/// toward zero (`12.34` with scale 10 → 123). `i32::MIN` when missing or not a
/// number.
pub fn http_json_num(pool: &ObjPool, path: i32, scale: i32) -> i32 {
    let Some(path) = str_arg(pool, path) else {
        return i32::MIN;
    };
    with_body(|b| {
        json::lookup(b, path)
            .and_then(|v| json::parse_scaled(v, scale))
            .unwrap_or(i32::MIN)
    })
}

pub fn http_free() -> i32 {
    with(|s| {
        if s.http_state != OP_RUNNING {
            s.http_body = None;
            s.http_code = 0;
            s.http_state = OP_IDLE;
        }
    });
    0
}

/// Largest CA certificate accepted by `SYS_HTTPS_CA`.
const MAX_CA_DER: u32 = 4096;

/// `SYS_HTTPS_CA(file)`: verify later `https://` requests against the DER CA
/// certificate stored as user file `file` (`"files"` in project.json). An
/// empty name turns verification off again. → 0, or `ERR_ARG` when the file
/// is missing / too large.
pub fn https_ca(pool: &ObjPool, name: i32) -> i32 {
    let Some(name) = str_arg(pool, name) else {
        return ERR_ARG;
    };
    if name.is_empty() {
        with(|s| s.http_ca = None);
        return 0;
    }
    let flash = crate::flash::new();
    let Ok(fs) = ferrite_core::fs::Fs::mount(&flash, ferrite_core::fs::DEFAULT_FS_BASE) else {
        return ERR_ARG;
    };
    let Some(entry) = fs.find_kind(&flash, name, ferrite_core::fs::RES_FILE) else {
        return ERR_ARG;
    };
    if entry.size == 0 || entry.size > MAX_CA_DER {
        return ERR_ARG;
    }
    let mut der = alloc::vec![0u8; entry.size as usize];
    fs.read_resource(&flash, &entry, 0, &mut der);
    with(|s| s.http_ca = Some(der));
    0
}

/// `SYS_NTP_SYNC(server, tz_minutes)`: query an SNTP server; on success the
/// RTC is set to UTC + `tz_minutes`.
pub fn ntp_sync(pool: &ObjPool, server: i32, tz_minutes: i32) -> i32 {
    let Some(server) = utf8_arg(pool, server, 253) else {
        return ERR_ARG;
    };
    if server.is_empty() || !(-14 * 60..=14 * 60).contains(&tz_minutes) {
        return ERR_ARG;
    }
    let rc = with(|s| {
        if s.ntp_pending || s.ntp_state == OP_RUNNING {
            return ERR_BUSY;
        }
        if s.wifi != WIFI_CONNECTED {
            return ERR_OFFLINE;
        }
        s.ntp_server = server;
        s.ntp_tz_minutes = tz_minutes;
        s.ntp_state = OP_RUNNING;
        s.ntp_pending = true;
        0
    });
    if rc == 0 {
        WAKE.signal(());
    }
    rc
}

pub fn ntp_status() -> i32 {
    with(|s| s.ntp_state)
}

/// `SYS_NTP_TIME()` → current UTC Unix seconds, 0 if never synced.
pub fn ntp_time(now_ms: u32) -> i32 {
    with(|s| {
        if s.ntp_unix == 0 {
            0
        } else {
            s.ntp_unix.wrapping_add(now_ms.wrapping_sub(s.ntp_at_ms) / 1000) as i32
        }
    })
}

/// Called once per main-loop pass on core 0: commit a finished SNTP sync to
/// the RTC (the I2C bus belongs to core 0). Returns the local time to write.
pub fn take_rtc_update(now_ms: u32) -> Option<DateTime> {
    let (unix, tz) = with(|s| {
        if !s.ntp_apply {
            return None;
        }
        s.ntp_apply = false;
        Some((s.ntp_unix, s.ntp_tz_minutes))
    })?;
    let elapsed = with(|s| now_ms.wrapping_sub(s.ntp_at_ms) / 1000);
    let local = (unix as i64) + elapsed as i64 + tz as i64 * 60;
    Some(datetime_from_unix(local))
}

/// The net task records a sync result; the core-0 timestamp is taken there so
/// `SYS_NTP_TIME` can count forward without a shared clock.
pub fn ntp_record(unix: u32) {
    let now = crate::systick::millis() as u32;
    with(|s| {
        s.ntp_unix = unix;
        s.ntp_at_ms = now;
        s.ntp_apply = true;
        s.ntp_state = OP_DONE;
    });
}

/// Unix seconds → calendar date/time (proleptic Gregorian, years 2000–2099
/// fit the RTC's two-digit year).
fn datetime_from_unix(secs: i64) -> DateTime {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    DateTime {
        year: (year - 2000).clamp(0, 99) as u8,
        month,
        day,
        // 1970-01-01 was a Thursday (4).
        weekday: (days + 4).rem_euclid(7) as u8,
        hour: (rem / 3600) as u8,
        minute: (rem / 60 % 60) as u8,
        second: (rem % 60) as u8,
    }
}
