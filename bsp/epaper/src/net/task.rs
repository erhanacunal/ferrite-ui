//! Core-1 side of the network service: the Wi-Fi link, DHCP, and one HTTP(S)
//! or SNTP request at a time. See `net/mod.rs` for the mailbox protocol.

use alloc::string::String;
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr};
use core::ptr::addr_of_mut;

use embassy_futures::select::{Either, Either3, select, select3};
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, Stack, StackResources};
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::peripherals::{CPU_CTRL, SW_INTERRUPT, TIMG0, WIFI};
use esp_hal::rng::Rng;
use esp_hal::system::Stack as CoreStack;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{AuthenticationMethod, Config, ControllerConfig, WifiController};
use esp_rtos::embassy::Executor;
use reqwless::client::{HttpClient, TlsConfig, TlsVerify};
use reqwless::request::{Method, RequestBuilder};
use static_cell::StaticCell;

use super::*;

/// Core-1 stack. TLS certificate checks (RSA / P-256) are stack-hungry.
const CORE1_STACK_BYTES: usize = 32 * 1024;
/// Pause between failed association attempts.
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// DHCP must finish within this after association, else reconnect.
const DHCP_TIMEOUT: Duration = Duration::from_secs(20);
/// Whole HTTP request: DNS + connect + TLS handshake + response.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// One SNTP query; up to `NTP_TRIES` are sent.
const NTP_REPLY_TIMEOUT: Duration = Duration::from_secs(3);
const NTP_TRIES: usize = 3;
const NTP_LOCAL_PORT: u16 = 50123;
/// Seconds between 1900-01-01 (NTP epoch) and 1970-01-01 (Unix epoch).
const NTP_UNIX_OFFSET: u32 = 2_208_988_800;
/// TLS record buffers (embedded-tls): a full 16 KB record plus overhead must
/// fit in the read buffer; requests are small.
const TLS_READ_BYTES: usize = 16 * 1024 + 256;
const TLS_WRITE_BYTES: usize = 4096;
/// How often the link task wakes on its own to refresh the RSSI.
const IDLE_POLL: Duration = Duration::from_secs(2);
/// A lookup through the DHCP-offered DNS server gives up after this (smoltcp
/// alone waits 10 s per server).
const DNS_STACK_TIMEOUT: Duration = Duration::from_secs(5);
/// Public resolvers asked when the DHCP one fails or none was offered.
const DNS_FALLBACK: [Ipv4Addr; 2] = [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)];
const DNS_REPLY_TIMEOUT: Duration = Duration::from_millis(1500);
const DNS_TRIES: usize = 2;

static mut CORE1_STACK: CoreStack<CORE1_STACK_BYTES> = CoreStack::new();
static EXECUTOR: StaticCell<Executor> = StaticCell::new();

/// Start the scheduler `esp-radio` needs (TIMG0 + software interrupts) and the
/// network executor on core 1. The radio stays off until a program calls
/// `SYS_WIFI_CONNECT`. Call once from `main`, after the heaps are set up.
pub fn start(
    timg0: TIMG0<'static>,
    sw_interrupt: SW_INTERRUPT<'static>,
    cpu_ctrl: CPU_CTRL<'static>,
    wifi: WIFI<'static>,
) {
    let timg0 = TimerGroup::new(timg0);
    let sw = SoftwareInterruptControl::new(sw_interrupt);
    esp_rtos::start(timg0.timer0, sw.software_interrupt0);

    // SAFETY: `start` runs once, so this is the only reference to the stack.
    let stack = unsafe { &mut *addr_of_mut!(CORE1_STACK) };
    esp_rtos::start_second_core(cpu_ctrl, sw.software_interrupt1, stack, move || {
        EXECUTOR.init(Executor::new()).run(|spawner| {
            spawner.spawn(net_task(wifi).unwrap());
        });
    });
}

fn seed() -> u64 {
    let rng = Rng::new();
    (rng.random() as u64) << 32 | rng.random() as u64
}

/// Fail requests that cannot run (radio off / link lost).
fn fail_pending(err: i32) {
    with(|s| {
        if s.http_pending || s.http_state == OP_RUNNING {
            s.http_pending = false;
            s.http_state = err;
        }
        if s.ntp_pending || s.ntp_state == OP_RUNNING {
            s.ntp_pending = false;
            s.ntp_state = err;
        }
    });
}

fn set_wifi(state: i32) {
    with(|s| {
        s.wifi = state;
        if state != WIFI_CONNECTED {
            s.ip = 0;
            s.dns = 0;
            s.rssi = 0;
        }
    });
}

/// Resolves once the VM asked for a different Wi-Fi state (connect with new
/// credentials, or disconnect).
async fn changed(generation: u32) {
    while with(|s| s.generation == generation) {
        WAKE.wait().await;
    }
}

#[embassy_executor::task]
async fn net_task(mut wifi: WIFI<'static>) {
    loop {
        // Radio off until a connect is requested.
        let generation = loop {
            if let Some(g) = with(|s| s.want_on.then_some(s.generation)) {
                break g;
            }
            set_wifi(WIFI_OFF);
            fail_pending(ERR_OFFLINE);
            WAKE.wait().await;
        };
        session(wifi.reborrow(), generation).await;
    }
}

/// One radio power-up: bring the controller and IP stack up with the current
/// credentials and keep the link alive until the request generation changes.
/// Everything is dropped on return, which turns the radio off.
async fn session(wifi: WIFI<'_>, generation: u32) {
    let (ssid, pass) = with(|s| (s.ssid.clone(), s.pass.clone()));
    let auth = if pass.is_empty() {
        AuthenticationMethod::None
    } else {
        AuthenticationMethod::Wpa2Personal
    };
    let station = StationConfig::default()
        .with_ssid(ssid.as_str())
        .with_password(pass)
        .with_auth_method(auth);
    let config = ControllerConfig::default().with_initial_config(Config::Station(station));
    let Ok((mut ctrl, interfaces)) = esp_radio::wifi::new(wifi, config) else {
        set_wifi(WIFI_FAILED);
        changed(generation).await;
        return;
    };

    let mut resources = StackResources::<6>::new();
    let (stack, mut runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(Default::default()),
        &mut resources,
        seed(),
    );
    select(runner.run(), link(&mut ctrl, stack, generation)).await;
    set_wifi(WIFI_OFF);
}

/// Associate, wait for DHCP, serve requests; reconnect when the link drops.
async fn link(ctrl: &mut WifiController<'_>, stack: Stack<'_>, generation: u32) {
    loop {
        set_wifi(WIFI_CONNECTING);
        match select(ctrl.connect_async(), changed(generation)).await {
            Either::Second(()) => return,
            Either::First(Ok(_)) => {}
            Either::First(Err(_)) => {
                set_wifi(WIFI_FAILED);
                fail_pending(ERR_OFFLINE);
                if let Either::Second(()) = select(Timer::after(RETRY_DELAY), changed(generation)).await {
                    return;
                }
                continue;
            }
        }

        match select(with_timeout(DHCP_TIMEOUT, stack.wait_config_up()), changed(generation)).await {
            Either::Second(()) => return,
            Either::First(Err(_)) => {
                let _ = with_timeout(Duration::from_secs(2), ctrl.disconnect_async()).await;
                set_wifi(WIFI_FAILED);
                continue;
            }
            Either::First(Ok(())) => {}
        }
        let (ip, dns) = stack.config_v4().map_or((0, 0), |c| {
            let ip = u32::from_be_bytes(c.address.address().octets());
            let dns = c.dns_servers.first().map_or(0, |d| u32::from_be_bytes(d.octets()));
            (ip, dns)
        });
        with(|s| {
            s.ip = ip;
            s.dns = dns;
            s.wifi = WIFI_CONNECTED;
        });

        if serve(ctrl, stack, generation).await {
            let _ = with_timeout(Duration::from_secs(2), ctrl.disconnect_async()).await;
            return;
        }
        // Link lost: requests in flight fail, then reconnect.
        fail_pending(ERR_OFFLINE);
    }
}

/// Run requests while associated. Returns `true` when the VM changed the
/// Wi-Fi request (leave the session), `false` when the link dropped.
async fn serve(ctrl: &mut WifiController<'_>, stack: Stack<'_>, generation: u32) -> bool {
    loop {
        if with(|s| s.generation != generation) {
            return true;
        }
        if let Ok(rssi) = ctrl.rssi() {
            with(|s| s.rssi = rssi);
        }

        let http = with(|s| {
            s.http_pending.then(|| {
                s.http_pending = false;
                (core::mem::take(&mut s.http_url), s.http_ca.clone())
            })
        });
        if let Some((url, ca)) = http {
            run_http(stack, url, ca).await;
            continue;
        }
        let ntp = with(|s| {
            s.ntp_pending.then(|| {
                s.ntp_pending = false;
                core::mem::take(&mut s.ntp_server)
            })
        });
        if let Some(server) = ntp {
            match ntp_query(stack, &server).await {
                Ok(unix) => ntp_record(unix),
                Err(e) => with(|s| s.ntp_state = e),
            }
            continue;
        }

        match select3(ctrl.wait_for_disconnect_async(), WAKE.wait(), Timer::after(IDLE_POLL)).await {
            Either3::First(_) => return false,
            Either3::Second(()) | Either3::Third(()) => {}
        }
    }
}

// ============================================================================
//  HTTP(S)
// ============================================================================

async fn run_http(stack: Stack<'_>, url: String, ca: Option<Vec<u8>>) {
    let result = with_timeout(HTTP_TIMEOUT, http_get(stack, &url, ca.as_deref())).await;
    let (state, code, body) = match result {
        Err(_) => (ERR_TIMEOUT, 0, None),
        Ok(Err(e)) => (e, 0, None),
        Ok(Ok((code, body))) => (OP_DONE, code, Some(body)),
    };
    with(|s| {
        s.http_state = state;
        s.http_code = code;
        s.http_body = body;
    });
}

fn http_err(e: reqwless::Error) -> i32 {
    use reqwless::Error as E;
    match e {
        E::Dns => ERR_DNS,
        E::Network(_) | E::ConnectionAborted => ERR_NET,
        E::Tls(_) => ERR_TLS,
        E::BufferTooSmall => ERR_TOO_LARGE,
        E::InvalidUrl(_) => ERR_ARG,
        _ => ERR_HTTP,
    }
}

/// GET `url` → (status code, body). The response is received into a PSRAM
/// buffer, then the body is copied into one of its exact size.
async fn http_get(stack: Stack<'_>, url: &str, ca: Option<&[u8]>) -> Result<(i32, PsBuf), i32> {
    let tcp_state = TcpClientState::<1, 2048, 2048>::new();
    let tcp = TcpClient::new(stack, &tcp_state);
    let dns = Resolver { stack };
    let mut rx = ps_buf(HTTP_MAX_RESPONSE).ok_or(ERR_NO_MEM)?;

    let https = url.starts_with("https://");
    let (mut tls_rx, mut tls_tx) = if https {
        (
            ps_buf(TLS_READ_BYTES).ok_or(ERR_NO_MEM)?,
            ps_buf(TLS_WRITE_BYTES).ok_or(ERR_NO_MEM)?,
        )
    } else {
        (PsBuf::new_in(ExternalMemory), PsBuf::new_in(ExternalMemory))
    };
    let mut client = if https {
        let verify = match ca {
            Some(ca) => TlsVerify::Certificate { ca, cert: None, key: None },
            None => TlsVerify::None,
        };
        HttpClient::new_with_tls(&tcp, &dns, TlsConfig::new(seed(), &mut tls_rx, &mut tls_tx, verify))
    } else {
        HttpClient::new(&tcp, &dns)
    };

    let headers = [("Connection", "close"), ("User-Agent", "ferrite-ui")];
    let mut request = client
        .request(Method::GET, url)
        .await
        .map_err(http_err)?
        .headers(&headers);
    let response = request.send(&mut rx).await.map_err(http_err)?;
    let code = response.status.0 as i32;
    let body = response.body().read_to_end().await.map_err(http_err)?;
    let mut out = ps_buf(body.len()).ok_or(ERR_NO_MEM)?;
    out.copy_from_slice(body);
    Ok((code, out))
}

// ============================================================================
//  SNTP
// ============================================================================

/// Ask `server` (host name or dotted IPv4) for the time → UTC Unix seconds.
async fn ntp_query(stack: Stack<'_>, server: &str) -> Result<u32, i32> {
    let addr = IpAddress::Ipv4(resolve(stack, server).await.ok_or(ERR_DNS)?);

    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buf = [0u8; 128];
    let mut tx_buf = [0u8; 128];
    let mut sock = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    sock.bind(NTP_LOCAL_PORT).map_err(|_| ERR_NET)?;

    // LI = 0, version 4, mode 3 (client); the rest stays zero.
    let mut request = [0u8; 48];
    request[0] = 0x23;
    let mut reply = [0u8; 64];
    for _ in 0..NTP_TRIES {
        sock.send_to(&request, (addr, 123)).await.map_err(|_| ERR_NET)?;
        if let Ok(Ok((n, _))) = with_timeout(NTP_REPLY_TIMEOUT, sock.recv_from(&mut reply)).await {
            // Mode 4 = server reply; stratum 0 = kiss-of-death, no time.
            if n >= 48 && reply[0] & 0x07 == 4 && reply[1] != 0 {
                let secs = u32::from_be_bytes([reply[40], reply[41], reply[42], reply[43]]);
                return Ok(secs.wrapping_sub(NTP_UNIX_OFFSET));
            }
        }
    }
    Err(ERR_TIMEOUT)
}

// ============================================================================
//  DNS
// ============================================================================

/// Host name (or dotted IPv4) → IPv4 address. Tries the DHCP-offered server
/// first, then `DNS_FALLBACK`, so a router whose DNS proxy is slow, broken
/// or missing from the DHCP offer still works.
async fn resolve(stack: Stack<'_>, host: &str) -> Option<Ipv4Addr> {
    if let Ok(a) = host.parse::<Ipv4Addr>() {
        return Some(a);
    }
    if let Ok(Ok(addrs)) = with_timeout(DNS_STACK_TIMEOUT, stack.dns_query(host, DnsQueryType::A)).await {
        if let Some(IpAddress::Ipv4(a)) = addrs.first() {
            return Some(*a);
        }
    }
    for server in DNS_FALLBACK {
        if let Some(a) = dns_query_udp(stack, server, host).await {
            return Some(a);
        }
    }
    None
}

/// One A-record query to `server` over a plain UDP socket.
async fn dns_query_udp(stack: Stack<'_>, server: Ipv4Addr, host: &str) -> Option<Ipv4Addr> {
    let mut query = [0u8; 300];
    let txid = Rng::new().random() as u16;
    let len = dns_build_query(&mut query, txid, host)?;

    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buf = [0u8; 512];
    let mut tx_buf = [0u8; 320];
    let mut sock = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    sock.bind(0).ok()?;

    let mut reply = [0u8; 512];
    for _ in 0..DNS_TRIES {
        sock.send_to(&query[..len], (IpAddress::Ipv4(server), 53)).await.ok()?;
        if let Ok(Ok((n, _))) = with_timeout(DNS_REPLY_TIMEOUT, sock.recv_from(&mut reply)).await {
            if let Some(a) = dns_parse_reply(&reply[..n], txid) {
                return Some(a);
            }
        }
    }
    None
}

/// Header (recursion desired, one question) + QNAME + QTYPE A + QCLASS IN.
fn dns_build_query(out: &mut [u8], txid: u16, host: &str) -> Option<usize> {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > 253 {
        return None;
    }
    out[..12].copy_from_slice(&[0, 0, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    out[..2].copy_from_slice(&txid.to_be_bytes());
    let mut n = 12;
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        out[n] = label.len() as u8;
        out[n + 1..n + 1 + label.len()].copy_from_slice(label.as_bytes());
        n += 1 + label.len();
    }
    out[n..n + 5].copy_from_slice(&[0, 0, 1, 0, 1]);
    Some(n + 5)
}

/// First A record in a reply to `txid` (a recursive resolver puts the
/// records for any CNAME chain in the same answer section).
fn dns_parse_reply(p: &[u8], txid: u16) -> Option<Ipv4Addr> {
    if p.len() < 12 || u16::from_be_bytes([p[0], p[1]]) != txid {
        return None;
    }
    // QR = response, RCODE = no error.
    if p[2] & 0x80 == 0 || p[3] & 0x0F != 0 {
        return None;
    }
    let questions = u16::from_be_bytes([p[4], p[5]]);
    let answers = u16::from_be_bytes([p[6], p[7]]);
    let mut i = 12;
    for _ in 0..questions {
        i = dns_skip_name(p, i)? + 4;
    }
    for _ in 0..answers {
        i = dns_skip_name(p, i)?;
        let rr = p.get(i..i + 10)?;
        let rtype = u16::from_be_bytes([rr[0], rr[1]]);
        let rdlen = u16::from_be_bytes([rr[8], rr[9]]) as usize;
        i += 10;
        let data = p.get(i..i + rdlen)?;
        if rtype == 1 && rdlen == 4 {
            return Some(Ipv4Addr::new(data[0], data[1], data[2], data[3]));
        }
        i += rdlen;
    }
    None
}

/// Index just past the (possibly compressed) name at `i`.
fn dns_skip_name(p: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let len = *p.get(i)? as usize;
        if len == 0 {
            return Some(i + 1);
        }
        if len & 0xC0 == 0xC0 {
            return Some(i + 2);
        }
        i += 1 + len;
    }
}

/// `resolve` as the `Dns` implementation reqwless needs for HTTP(S).
struct Resolver<'a> {
    stack: Stack<'a>,
}

impl embedded_nal_async::Dns for Resolver<'_> {
    type Error = ();

    async fn get_host_by_name(&self, host: &str, _: embedded_nal_async::AddrType) -> Result<IpAddr, ()> {
        resolve(self.stack, host).await.map(IpAddr::V4).ok_or(())
    }

    async fn get_host_by_address(&self, _: IpAddr, _: &mut [u8]) -> Result<usize, ()> {
        Err(())
    }
}
