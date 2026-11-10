//! WiFi (BCM4334 over SDIO) + HTTP server + TCP protocol bridge, on CPU3.
//!
//! The HAL's `uart_boot` / `wifi_async_test` steps, as a service next to the
//! runtime:
//!
//! 1. CPU0 at boot ([`start`]): reads the settings (and, if the firmware
//!    image embeds no chip firmware, the firmware + NVRAM) ([`wifi_file`]),
//!    powers the chip (`bcm4334::power_on`: PMIC over I2C0, which CPU0 owns, ~0.5 s),
//!    then starts CPU3. Nothing configured → no WiFi, no delay.
//! 2. CPU3, polled ([`cpu3`]): SDIO + firmware, scan, WPA2 join (retried
//!    every [`RETRY_MS`] until the network is there).
//! 3. CPU3, Embassy: the WiFi runner (host wake interrupt routed to CPU3),
//!    embassy-net with DHCP (hostname from the config, default
//!    [`DEFAULT_HOSTNAME`], as uart_boot, so `ferrite_cli --host` finds
//!    both), [`http`] on port 80 and the protocol bridge on [`BRIDGE_PORT`].
//!
//! The UI core never waits for the network: CPU0 only touches the bridge
//! rings (`bridge.rs`) and, once per pass, [`service`] (an uploaded FS image
//! to save, a restart request).
//!
//! The files are `RES_FILE`s of the FS image (`"files"` in project.json, so
//! they arrive with `writefs` / `bootflash`): [`CFG_RES`], [`FW_RES`],
//! [`NVRAM_RES`]. An image that has them also leaves a copy on CACHE
//! (`FERRITE/`[`CFG_FILE`] …, written by the firmware), and an image without
//! them uses that copy: WiFi survives uploading an app that doesn't carry
//! them. The settings file is text, `key=value` per line, `#` comments:
//! ```text
//! ssid=MyNetwork
//! password=passphrase
//! band=2.4          # optional: 2.4 or 5, only join an AP on that band
//! hostname=exynos   # optional DHCP hostname
//! ```
//! Build-time `WIFI_SSID` / `WIFI_PASSWORD` / `WIFI_BAND` (and
//! `WIFI_FIRMWARE_DIR`, build.rs) are the last fallback.
//! A lost link is not rejoined (the runner ends; restart the board, or go
//! through standby).
//!
//! Standby ([`suspend`] / [`resume`], `platform::enter_standby`): CPU3's tasks
//! end ([`GROUP`]), CPU3 turns the chip off (`bcm4334::power_off`) and stops
//! itself; at the wake CPU0 powers the chip again and starts CPU3, which joins
//! as at boot. While CPU3 is still joining, standby leaves it running.

mod http;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, DhcpConfig, Stack, StackResources};
use embassy_sync::blocking_mutex::CriticalSectionMutex;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use exynos_4412_rs::crypto::{self, Hmac};
use exynos_4412_rs::drivers::bcm4334::{self, Activity, Bcm4334, Error};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::sdhci::Sdhci;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::embassy::wifi;
use exynos_4412_rs::wpa::{self, Supplicant};
use exynos_4412_rs::{cpu, embassy, println, smp, soc};

use crate::bridge;
use crate::hotplug::Group;

/// Network settings / chip firmware / NVRAM as FS image resources
/// (`RES_FILE`, ≤ 15 characters) …
pub const CFG_RES: &str = "wifi_cfg";
pub const FW_RES: &str = "wifi_fw";
pub const NVRAM_RES: &str = "wifi_nvram";
/// … and their copies on CACHE, in `storage::DIR` (8.3 names).
pub const CFG_FILE: &str = "WIFI.CFG";
pub const FW_FILE: &str = "WIFIFW.BIN";
pub const NVRAM_FILE: &str = "WIFINV.TXT";
pub const DEFAULT_HOSTNAME: &str = "exynos";
/// The USART protocol as a raw TCP stream (`ferrite_cli -p tcp://HOST:7000`).
pub const BRIDGE_PORT: u16 = 7000;
pub const HTTP_PORT: u16 = 80;
/// The core the network runs on (parked otherwise).
const CORE: usize = 3;
/// CPU3's tasks, ended for standby.
static GROUP: Group = Group::new();
/// How long [`suspend`] waits for CPU3 to end its tasks and lose power.
const STOP_MS: u64 = 3000;

static FIRMWARE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wifi_firmware.bin"));
static NVRAM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wifi_nvram.txt"));

const LAST_2G_CHANNEL: u8 = 14;
const SCAN_TIMEOUT_MS: u64 = 20_000;
const JOIN_TIMEOUT_MS: u64 = 30_000;
/// Wait between join attempts (network not found / refused).
const RETRY_MS: u64 = 10_000;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

const RX_FRAMES: usize = 16;
const TX_FRAMES: usize = 16;
/// DHCP + HTTP servers + the bridge, with room to spare.
const SOCKETS: usize = 8;
const BRIDGE_RX: usize = 16 * 1024;
const BRIDGE_TX: usize = 8 * 1024;
/// A bridge client silent this long is dropped (a writefs never pauses
/// that long; `ferrite_cli` reconnects per command).
const BRIDGE_IDLE: Duration = Duration::from_secs(120);

static mut WIFI_STATE: wifi::State<RX_FRAMES, TX_FRAMES> = wifi::State::new();
static mut RESOURCES: StackResources<SOCKETS> = StackResources::new();

// --- configuration (CPU0 at boot) ---------------------------------------------------------

struct WifiConfig {
    ssid: String,
    password: String,
    /// 2 = 2.4 GHz only, 5 = 5 GHz only, 0 = either.
    band: u8,
    hostname: String,
    firmware: &'static [u8],
    nvram: &'static [u8],
}

/// Written once by CPU0 before CPU3 starts, only read after.
static mut CONFIG: Option<WifiConfig> = None;

fn config() -> &'static WifiConfig {
    // SAFETY: see CONFIG.
    unsafe { (*(&raw const CONFIG)).as_ref().expect("set before CPU3 starts") }
}

fn parse_band(v: &str) -> u8 {
    match v.trim() {
        "2.4" | "2" => 2,
        "5" => 5,
        _ => 0,
    }
}

/// RES_FILE `name` of the FS image in the RAM region, if present and at
/// most `max` bytes.
fn image_file(name: &str, max: usize) -> Option<Vec<u8>> {
    use ferrite_core::platform::Platform;
    let flash = ferrite_core::flash::FlashImpl::with_backend(crate::flash::RamFlash);
    let fs = ferrite_core::fs::Fs::mount(&flash, crate::platform::Exynos4412Platform::FS_BASE).ok()?;
    let entry = fs.find_kind(&flash, name.as_bytes(), ferrite_core::fs::RES_FILE)?;
    if entry.size as usize > max {
        println!("ferrite: {} too large ({} bytes)", name, entry.size);
        return None;
    }
    let mut buf = alloc::vec![0u8; entry.size as usize];
    fs.read_resource(&flash, &entry, 0, &mut buf);
    Some(buf)
}

/// A WiFi file: the FS image's `res` (copied to CACHE `file` when it
/// differs, so later images without it keep WiFi), else that CACHE copy.
fn wifi_file(res: &str, file: &str, max: usize) -> Option<Vec<u8>> {
    let cached = crate::storage::read_named(file, max);
    match image_file(res, max) {
        Some(data) => {
            if cached.as_deref() != Some(&data[..]) && crate::storage::available() {
                if crate::storage::save_named(file, &data) {
                    println!("ferrite: {} kept as {}/{}", res, crate::storage::DIR, file);
                }
            }
            Some(data)
        }
        None => cached,
    }
}

fn load_config() -> Option<WifiConfig> {
    let mut cfg = WifiConfig {
        ssid: String::new(),
        password: String::new(),
        band: option_env!("WIFI_BAND").map_or(0, parse_band),
        hostname: String::from(DEFAULT_HOSTNAME),
        firmware: FIRMWARE,
        nvram: NVRAM,
    };
    match wifi_file(CFG_RES, CFG_FILE, 4096) {
        Some(text) => {
            for line in text.split(|&b| b == b'\n') {
                let Ok(line) = core::str::from_utf8(line) else { continue };
                let line = line.split('#').next().unwrap_or("").trim();
                let Some((key, value)) = line.split_once('=') else { continue };
                let value = value.trim();
                match key.trim().to_ascii_lowercase().as_str() {
                    "ssid" => cfg.ssid = String::from(value),
                    "password" | "psk" => cfg.password = String::from(value),
                    "band" => cfg.band = parse_band(value),
                    "hostname" if !value.is_empty() => cfg.hostname = String::from(value),
                    _ => {}
                }
            }
        }
        None => {
            cfg.ssid = String::from(option_env!("WIFI_SSID").unwrap_or(""));
            cfg.password = String::from(option_env!("WIFI_PASSWORD").unwrap_or(""));
        }
    }
    if cfg.ssid.is_empty() {
        println!("ferrite: wifi off (no {} file in the FS image)", CFG_RES);
        return None;
    }
    if cfg.firmware.is_empty() {
        let fw = wifi_file(FW_RES, FW_FILE, 2 << 20);
        let nv = wifi_file(NVRAM_RES, NVRAM_FILE, 64 << 10);
        let (Some(fw), Some(nv)) = (fw, nv) else {
            println!("ferrite: wifi off (no chip firmware: add {} + {} to the FS image, or build with WIFI_FIRMWARE_DIR)", FW_RES, NVRAM_RES);
            return None;
        };
        cfg.firmware = Box::leak(fw.into_boxed_slice());
        cfg.nvram = Box::leak(nv.into_boxed_slice());
    }
    Some(cfg)
}

/// CPU0, at boot after `storage::init`, `Gic::init` and `embassy::init`:
/// powers the chip and hands the rest to CPU3. Not fatal on any error.
pub fn start(pmic: Max77686, mct: Mct) {
    let Some(cfg) = load_config() else { return };
    // PMIC rails + 32 kHz clock over I2C0: on CPU0, which owns that bus.
    if let Err(e) = bcm4334::power_on(pmic, mct) {
        println!("ferrite: wifi power ERROR {:?}", e);
        return;
    }
    // SAFETY: CPU3 is not running yet.
    unsafe { *(&raw mut CONFIG) = Some(cfg) };
    set_state(State::Joining);
    GROUP.arm();
    if let Err(e) = smp::start_cpu(CORE, cpu3) {
        println!("ferrite: CPU{} ERROR {:?}, wifi off", CORE, e);
        set_state(State::Failed);
    }
}

// --- status, read by /api/status ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Off = 0,
    Joining = 1,
    /// Joined, waiting for DHCP.
    Joined = 2,
    Up = 3,
    Failed = 4,
    /// The WiFi runner stopped (link lost).
    Down = 5,
    /// Off for standby ([`suspend`]).
    Asleep = 6,
}

static STATE: AtomicU8 = AtomicU8::new(State::Off as u8);

fn set_state(s: State) {
    STATE.store(s as u8, Ordering::Release);
}

/// Whether CPU3's executor runs (joined, with or without an address).
pub fn running() -> bool {
    matches!(STATE.load(Ordering::Acquire), 2 | 3 | 5)
}

pub fn state_name() -> &'static str {
    match STATE.load(Ordering::Acquire) {
        0 => "off",
        1 => "joining",
        2 => "dhcp",
        3 => "up",
        4 => "failed",
        5 => "down",
        _ => "asleep",
    }
}

#[derive(Clone, Copy, Default)]
pub struct Link {
    pub mac: [u8; 6],
    pub bssid: [u8; 6],
    pub channel: u8,
    pub rssi: i16,
    pub address: [u8; 4],
    pub prefix: u8,
    pub joined_ms: u64,
}

static LINK: CriticalSectionMutex<Cell<Link>> = CriticalSectionMutex::new(Cell::new(Link {
    mac: [0; 6],
    bssid: [0; 6],
    channel: 0,
    rssi: 0,
    address: [0; 4],
    prefix: 0,
    joined_ms: 0,
}));

pub fn link() -> Link {
    LINK.lock(Cell::get)
}

fn update_link(f: impl FnOnce(&mut Link)) {
    LINK.lock(|c| {
        let mut l = c.get();
        f(&mut l);
        c.set(l);
    });
}

// --- CPU3: polled bring-up + join -----------------------------------------------------------

fn report(activity: Activity) {
    if let Activity::Event(event) = activity
        && (event.code == bcm4334::EVENT_DEAUTH_IND || event.code == bcm4334::EVENT_DISASSOC_IND)
    {
        println!("ferrite: wifi event {}, reason {}", event.code, event.reason);
    }
}

fn delay_ms(mct: Mct, ms: u64) {
    let start = mct.count();
    while mct.count() - start < ms * COUNTS_PER_MS {
        core::hint::spin_loop();
    }
}

struct Target {
    bssid: [u8; 6],
    rssi: i16,
    channel: u8,
    rsn: [u8; wpa::RSN_MAX],
    rsn_length: usize,
}

struct Joined {
    chip: Bcm4334,
    supplicant: Supplicant,
    seed: u64,
}

/// SDIO + firmware. Returns the chip with its radio prepared and its MAC.
fn bring_up(cfg: &WifiConfig, mct: Mct) -> Result<(Bcm4334, [u8; 6]), Error> {
    let (mut chip, _rate) = Bcm4334::start(Sdhci::new()?, mct, bcm4334::HIGH_SPEED_MAX_HZ)?;
    chip.attach()?;
    chip.load_firmware(cfg.firmware, cfg.nvram)?;
    chip.start_firmware()?;
    chip.enable_host_wake()?;
    chip.disable_glomming()?;
    let mut address = [0; 32];
    chip.get_variable("cur_etheraddr", &mut address)?;
    let mut own = [0; 6];
    own.copy_from_slice(&address[..6]);
    chip.prepare_radio()?;
    Ok((chip, own))
}

/// One scan + join. `Ok(None)`: the network is not there (or asks for what
/// this cannot do): try again later.
fn associate(cfg: &WifiConfig, chip: &mut Bcm4334, own: [u8; 6], mct: Mct) -> Result<Option<(Supplicant, u64)>, Error> {
    let band_allows = |channel: u8| match cfg.band {
        2 => channel <= LAST_2G_CHANNEL,
        5 => channel > LAST_2G_CHANNEL,
        _ => true,
    };
    let mut target: Option<Target> = None;
    let mut pool = Hmac::new(&own).sign(&[&mct.count().to_le_bytes()[..]]);
    chip.scan(SCAN_TIMEOUT_MS, |bss, rsn| {
        pool = Hmac::new(&pool).sign(&[&mct.count().to_le_bytes()[..], &bss.bssid, &bss.rssi.to_le_bytes()]);
        let named = cfg.ssid.as_bytes() == &bss.ssid[..usize::from(bss.ssid_length)];
        if named
            && band_allows(bss.channel)
            && wpa::rsn_supported(rsn)
            && rsn.len() <= wpa::RSN_MAX
            && target.as_ref().is_none_or(|known| bss.rssi > known.rssi)
        {
            let mut copy = [0; wpa::RSN_MAX];
            copy[..rsn.len()].copy_from_slice(rsn);
            target = Some(Target { bssid: bss.bssid, rssi: bss.rssi, channel: bss.channel, rsn: copy, rsn_length: rsn.len() });
        }
    })?;
    let Some(target) = target else {
        println!("ferrite: wifi network {} not found", cfg.ssid);
        return Ok(None);
    };
    let key = crypto::wpa_key(cfg.password.as_bytes(), cfg.ssid.as_bytes());
    let mut nonce = [0; 32];
    crypto::prf(&pool, b"Init Counter", &mct.count().to_le_bytes(), &mut nonce);
    let mut seed = [0; 8];
    crypto::prf(&pool, b"Stack seed", &mct.count().to_le_bytes(), &mut seed);
    let Some(mut supplicant) = Supplicant::new(key, own, target.bssid, &target.rsn[..target.rsn_length], nonce) else {
        println!("ferrite: wifi access point asks for what this cannot do");
        return Ok(None);
    };
    chip.connect(cfg.ssid.as_bytes(), &mut supplicant, JOIN_TIMEOUT_MS, report)?;
    update_link(|l| {
        l.bssid = target.bssid;
        l.channel = target.channel;
        l.rssi = target.rssi;
    });
    Ok(Some((supplicant, u64::from_le_bytes(seed))))
}

fn join(cfg: &WifiConfig, mct: Mct) -> Result<Joined, Error> {
    let begin = mct.count();
    let (mut chip, own) = bring_up(cfg, mct)?;
    println!("ferrite: wifi chip up in {} ms", (mct.count() - begin) / COUNTS_PER_MS);
    update_link(|l| l.mac = own);
    loop {
        match associate(cfg, &mut chip, own, mct) {
            Ok(Some((supplicant, seed))) => {
                let l = link();
                println!(
                    "ferrite: wifi joined {} in {} ms, channel {}, {} dBm",
                    cfg.ssid,
                    (mct.count() - begin) / COUNTS_PER_MS,
                    l.channel,
                    l.rssi
                );
                return Ok(Joined { chip, supplicant, seed });
            }
            Ok(None) => {}
            // A refused / timed-out handshake: the chip is still usable.
            Err(e) => println!("ferrite: wifi join ERROR {:?}", e),
        }
        delay_ms(mct, RETRY_MS);
    }
}

/// CPU3: join, then the network tasks.
fn cpu3(_core: usize) -> ! {
    println!("ferrite: CPU{} up, joining", CORE);
    Gic::new().init_this_cpu();
    cpu::enable_irq();
    let cfg = config();
    let joined = match join(cfg, Mct::new()) {
        Ok(j) => j,
        Err(e) => {
            println!("ferrite: wifi ERROR {:?}, wifi off", e);
            set_state(State::Failed);
            cpu::halt();
        }
    };
    set_state(State::Joined);
    update_link(|l| l.joined_ms = embassy::now() / 1000);

    let mut dhcp = DhcpConfig::default();
    dhcp.hostname = cfg.hostname.as_str().try_into().ok();
    // SAFETY: the only references to these, taken here once per start of this core; the last
    // start's tasks have ended (`GROUP`), and with them every use of the old ones.
    let (state, resources) = unsafe {
        *addr_of_mut!(WIFI_STATE) = wifi::State::new();
        *addr_of_mut!(RESOURCES) = StackResources::new();
        (&mut *addr_of_mut!(WIFI_STATE), &mut *addr_of_mut!(RESOURCES))
    };
    let (device, runner) = wifi::new(state, joined.chip, joined.supplicant, report);
    // `wifi::new` routes the host wake interrupt to CPU0: its task runs here.
    if let Some(eint) = bcm4334::HOST_WAKE.eint() {
        Gic::new().set_target(eint.irq(), CORE);
    }
    let (stack, net_runner) = embassy_net::new(device, Config::dhcpv4(dhcp), resources, joined.seed);

    embassy::run_until(
        |spawner| {
            GROUP.begin(4 + http::SERVERS as u32);
            spawner.spawn(wifi_task(runner).unwrap());
            spawner.spawn(net_task(net_runner).unwrap());
            spawner.spawn(dhcp_task(stack).unwrap());
            spawner.spawn(bridge_task(stack).unwrap());
            for _ in 0..http::SERVERS {
                spawner.spawn(http_task(stack).unwrap());
            }
        },
        || GROUP.done(),
        // Standby: every task has ended, the chip's runner with them; no wake-up from the
        // chip's host wake for it.
        || {
            if let Some(eint) = bcm4334::HOST_WAKE.eint() {
                Gic::new().disable(eint.irq());
                eint.mask();
                eint.clear();
            }
        },
    );
    bcm4334::power_off();
    update_link(|l| {
        l.address = [0; 4];
        l.prefix = 0;
    });
    set_state(State::Asleep);
    println!("ferrite: wifi off, CPU{} stopping", CORE);
    smp::stop_this_cpu()
}

/// CPU0, entering standby: ends CPU3's tasks and waits for it to stop. True
/// if it did (then [`resume`] at the wake); false while it is still joining
/// (left running: its polled join cannot be ended) or did not stop in time.
pub fn suspend() -> bool {
    if !matches!(STATE.load(Ordering::Acquire), 2 | 3 | 5) {
        return false;
    }
    GROUP.stop();
    match smp::wait_offline(CORE, STOP_MS) {
        Ok(_) => true,
        Err(status) => {
            println!("ferrite: CPU{} did not stop (status {:#x}), wifi left as it is", CORE, status);
            false
        }
    }
}

/// CPU0, at the wake after a [`suspend`] that returned true: powers the chip
/// (~0.45 s: PMIC over I2C0, which CPU0 owns, then the chip's reset) and
/// starts CPU3, which joins again.
pub fn resume(pmic: Max77686, mct: Mct) {
    if let Err(e) = bcm4334::power_on(pmic, mct) {
        println!("ferrite: wifi power ERROR {:?}", e);
        set_state(State::Failed);
        return;
    }
    set_state(State::Joining);
    GROUP.arm();
    if let Err(e) = smp::start_cpu(CORE, cpu3) {
        println!("ferrite: CPU{} ERROR {:?}, wifi off", CORE, e);
        set_state(State::Failed);
    }
}

#[embassy_executor::task]
async fn wifi_task(runner: wifi::Runner<'static>) {
    GROUP
        .run(async {
            let error = runner.run().await;
            set_state(State::Down);
            println!("ferrite: wifi stopped: {:?}", error);
        })
        .await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, wifi::Device<'static>>) {
    GROUP.run(runner.run()).await
}

#[embassy_executor::task(pool_size = http::SERVERS)]
async fn http_task(stack: Stack<'static>) {
    GROUP.run(http::server(stack)).await
}

/// Logs (and records) the address each time DHCP gives one.
#[embassy_executor::task]
async fn dhcp_task(stack: Stack<'static>) {
    GROUP.run(dhcp_loop(stack)).await
}

async fn dhcp_loop(stack: Stack<'static>) {
    loop {
        stack.wait_config_up().await;
        if let Some(c) = stack.config_v4() {
            update_link(|l| {
                l.address = c.address.address().octets();
                l.prefix = c.address.prefix_len();
            });
            set_state(State::Up);
            println!(
                "ferrite: http://{}/ ({}), protocol bridge tcp://{}:{}",
                c.address.address(),
                config().hostname,
                c.address.address(),
                BRIDGE_PORT
            );
        }
        stack.wait_config_down().await;
        set_state(State::Joined);
    }
}

// --- TCP protocol bridge ----------------------------------------------------------------------

#[embassy_executor::task]
async fn bridge_task(stack: Stack<'static>) {
    GROUP.run(bridge_loop(stack)).await
}

async fn bridge_loop(stack: Stack<'static>) {
    let mut rx = [0; BRIDGE_RX];
    let mut tx = [0; BRIDGE_TX];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(BRIDGE_IDLE));
        if socket.accept(BRIDGE_PORT).await.is_err() {
            continue;
        }
        socket.set_nagle_enabled(false);
        match bridge::Session::take(bridge::TCP) {
            Some(session) => bridge_serve(&mut socket, &session).await,
            None => println!("ferrite: bridge busy (an HTTP request holds the protocol)"),
        }
        socket.close();
        let _ = with_timeout(Duration::from_secs(1), socket.flush()).await;
        socket.abort();
    }
}

/// Moves bytes both ways until the client closes or goes quiet.
async fn bridge_serve(socket: &mut TcpSocket<'_>, session: &bridge::Session) {
    // The runtime drops an earlier session's leftovers first.
    let begin = Instant::now();
    while !session.ready() && begin.elapsed() < Duration::from_secs(1) {
        Timer::after(Duration::from_millis(1)).await;
    }
    let mut buf = [0u8; 2048];
    let mut pending = 0usize; // received, not yet in bridge::RX
    let mut last = Instant::now();
    loop {
        // Runtime → client.
        let mut out = [0u8; 1024];
        let n = bridge::TX.pop_slice(&mut out);
        if n > 0 {
            if write_all(socket, &out[..n]).await.is_err() {
                return;
            }
        }
        // Client → runtime, as far as the ring takes it.
        if pending > 0 {
            let taken = bridge::RX.push_slice(&buf[..pending]);
            buf.copy_within(taken..pending, 0);
            pending -= taken;
        }
        if pending == 0 && socket.can_recv() {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    pending = n;
                    last = Instant::now();
                    continue;
                }
            }
        }
        if !socket.may_recv() && bridge::TX.is_empty() {
            return;
        }
        if last.elapsed() > BRIDGE_IDLE {
            return;
        }
        if n == 0 {
            Timer::after(Duration::from_millis(1)).await;
        }
    }
}

pub(crate) async fn write_all(socket: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), embassy_net::tcp::Error> {
    while !data.is_empty() {
        let written = socket.write(data).await?;
        data = &data[written..];
    }
    Ok(())
}

// --- CPU0 mailbox: uploaded FS images, restarts -----------------------------------------------

/// `POST /flash`: an image for CPU0 to check and save ([`service`]).
static UPLOAD: CriticalSectionMutex<RefCell<Option<Vec<u8>>>> = CriticalSectionMutex::new(RefCell::new(None));
/// [`UPLOAD`]'s outcome, `FLASH_*`.
static FLASH_STATUS: AtomicU8 = AtomicU8::new(FLASH_IDLE);
pub const FLASH_IDLE: u8 = 0;
pub const FLASH_PENDING: u8 = 1;
pub const FLASH_SAVED: u8 = 2;
pub const FLASH_BAD_IMAGE: u8 = 3;
pub const FLASH_TOO_LARGE: u8 = 4;
pub const FLASH_NO_STORAGE: u8 = 5;
pub const FLASH_SAVE_FAILED: u8 = 6;
/// The HTTP answer has gone out: CPU0 may restart.
static ANSWERED: AtomicBool = AtomicBool::new(false);
/// `POST /restart` (set after its answer has gone out).
static RESTART: AtomicBool = AtomicBool::new(false);

/// CPU3: hands an uploaded image over. False if one is already pending.
pub(crate) fn submit_image(image: Vec<u8>) -> bool {
    if FLASH_STATUS.compare_exchange(FLASH_IDLE, FLASH_PENDING, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return false;
    }
    ANSWERED.store(false, Ordering::Release);
    UPLOAD.lock(|u| *u.borrow_mut() = Some(image));
    true
}

pub(crate) fn flash_status() -> u8 {
    FLASH_STATUS.load(Ordering::Acquire)
}

/// CPU3: the answer to a finished upload has been sent. A failed upload
/// frees the mailbox for the next one.
pub(crate) fn answered() {
    if FLASH_STATUS.load(Ordering::Acquire) != FLASH_SAVED {
        FLASH_STATUS.store(FLASH_IDLE, Ordering::Release);
    }
    ANSWERED.store(true, Ordering::Release);
}

pub(crate) fn request_restart() {
    RESTART.store(true, Ordering::Release);
}

/// A `FlashBackend` over an uploaded image, to check it before saving.
struct SliceFlash<'a>(&'a [u8]);

impl ferrite_core::flash::FlashBackend for SliceFlash<'_> {
    fn read(&self, addr: u32, buf: &mut [u8]) {
        let start = (addr as usize).min(self.0.len());
        let n = buf.len().min(self.0.len() - start);
        buf[..n].copy_from_slice(&self.0[start..start + n]);
        buf[n..].fill(0xFF);
    }
    fn write(&self, _addr: u32, _data: &[u8]) {}
    fn erase_sector(&self, _addr: u32) {}
    fn read_id(&self) -> (u8, u8, u8) {
        (0, 0, 0)
    }
}

/// CPU0: checks and saves an uploaded image as `FERRITE/FLASH.BIN`. An FS
/// image from `ferrite_build` (starts with `FERR`) keeps the region's config
/// sector in front, as a `writefs` does; anything else is a whole
/// `FLASH.BIN`. The running program keeps its region until the restart.
fn install(image: Vec<u8>) -> u8 {
    use ferrite_core::platform::Platform;
    let base = crate::platform::Exynos4412Platform::FS_BASE as usize;
    let full = if image.starts_with(b"FERR") {
        let mut full = Vec::new();
        if full.try_reserve_exact(base + image.len()).is_err() {
            return FLASH_TOO_LARGE;
        }
        full.extend_from_slice(crate::flash::prefix(base));
        full.extend_from_slice(&image);
        drop(image);
        full
    } else {
        image
    };
    if full.len() > crate::flash::SIZE {
        return FLASH_TOO_LARGE;
    }
    let flash = ferrite_core::flash::FlashImpl::with_backend(SliceFlash(&full));
    if !ferrite_core::fs::Fs::mount(&flash, base as u32).is_ok_and(|f| f.verify(&flash)) {
        return FLASH_BAD_IMAGE;
    }
    if !crate::storage::available() {
        return FLASH_NO_STORAGE;
    }
    // The old region's unsaved changes must not overwrite the new file at
    // the restart.
    crate::flash::forget_changes();
    if crate::storage::save(&full) { FLASH_SAVED } else { FLASH_SAVE_FAILED }
}

/// Screenshot mailbox (HTTP `GET /api/screen`): CPU3 raises [`SCREEN_WANTED`], CPU0 copies the
/// shadow between passes into [`SCREEN`] (None = no memory) and raises [`SCREEN_READY`].
static SCREEN_WANTED: AtomicBool = AtomicBool::new(false);
static SCREEN_READY: AtomicBool = AtomicBool::new(false);
static SCREEN: CriticalSectionMutex<RefCell<Option<Vec<u32>>>> = CriticalSectionMutex::new(RefCell::new(None));

/// CPU3: the screen as CPU0 holds it between passes (XRGB8888, rows top to bottom), or None
/// (no memory, or CPU0 did not answer within 3 s).
pub async fn screenshot() -> Option<Vec<u32>> {
    SCREEN_READY.store(false, Ordering::Release);
    SCREEN.lock(|s| *s.borrow_mut() = None);
    SCREEN_WANTED.store(true, Ordering::Release);
    let start = Instant::now();
    while !SCREEN_READY.load(Ordering::Acquire) {
        if start.elapsed() > Duration::from_secs(3) {
            SCREEN_WANTED.store(false, Ordering::Release);
            return None;
        }
        Timer::after(Duration::from_millis(2)).await;
    }
    SCREEN.lock(|s| s.borrow_mut().take())
}

/// CPU0, once per main-loop pass (`frame_end`): cheap unless the network
/// asked for something.
pub fn service() {
    if SCREEN_WANTED.swap(false, Ordering::AcqRel) {
        let shot = crate::lcd::grab();
        SCREEN.lock(|s| *s.borrow_mut() = shot);
        SCREEN_READY.store(true, Ordering::Release);
    }
    if FLASH_STATUS.load(Ordering::Acquire) == FLASH_PENDING
        && let Some(image) = UPLOAD.lock(|u| u.borrow_mut().take())
    {
        let len = image.len();
        let status = install(image);
        println!("ferrite: HTTP flash upload, {} bytes: status {}", len, status);
        FLASH_STATUS.store(status, Ordering::Release);
        if status == FLASH_SAVED {
            // Let CPU3 answer before the board goes away.
            let start = crate::systick::millis();
            while !ANSWERED.load(Ordering::Acquire) && crate::systick::millis().wrapping_sub(start) < 3000 {
                core::hint::spin_loop();
            }
            <crate::platform::Exynos4412Platform as ferrite_core::runtime::PlatformRuntime>::reset();
        }
    }
    if RESTART.load(Ordering::Acquire) {
        <crate::platform::Exynos4412Platform as ferrite_core::runtime::PlatformRuntime>::reset();
    }
}
