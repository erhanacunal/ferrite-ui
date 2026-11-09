//! HTTP on port 80 (CPU3).
//!
//! | Request | Does |
//! |---|---|
//! | `GET /` | a small dashboard page (status, memory, profile, FS upload) |
//! | `GET /api/status` | JSON: WiFi link, address, uptime, heap, passes/frames |
//! | `GET /api/mem` | JSON: the `memdetail` table (asked through the bridge) |
//! | `GET /api/perf?ms=N` | JSON: the main-loop profile over N ms (default 1000, `perf` feature) |
//! | `POST /flash` (or `/fs`, or `PUT`) | an FS image (or a whole `FLASH.BIN`): checked, saved as `FERRITE/FLASH.BIN`, then a restart |
//! | `POST /restart` | restarts the board |
//! | `GET /api/screen` | the screen as a 32-bit BMP (the LCD shadow, copied by CPU0 between passes) |
//! | `POST /api/compose?on=0\|1` | GPU compositor off/on (`gpu_compose`), then the whole screen is repainted |
//! | `POST /api/layerblit?on=0\|1` | G2D sprite blits inside layers off/on (`lcd::LAYER_BLITS`), for A/B measurements |
//! | `POST /api/layercache?uncached=0\|1` | layer shadows drawn through the cached view or the uncached alias (`lcd::LAYER_UNCACHED`), for A/B measurements |
//! | `POST /api/overlay?mode=off\|still\|move\|fade\|full[&x=X&y=Y&alpha=A]` | FIMD window 1 test picture above the UI (`overlay_test.rs`) |
//!
//! `/flash` takes the same request as uart_boot's (`ferrite_cli bootflash
//! --host` works against both): `Content-Length` required, `X-CRC32` (8 hex
//! digits, zlib CRC-32 of the body) checked when present, `Expect:
//! 100-continue` answered. The image must mount and pass its FS checksum.
//!
//! `/api/mem` and `/api/perf` hold the protocol bridge while they ask
//! (409 while a `tcp://` bridge client is connected).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;

use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use exynos_4412_rs::embassy::wifi;
use exynos_4412_rs::{heap, println};
use ferrite_core::protocol::mem_pool;

use super::{HTTP_PORT, write_all};
use crate::bridge;

pub const SERVERS: usize = 3;
const SERVER_RX: usize = 32 * 1024;
const SERVER_TX: usize = 8 * 1024;
const REQUEST_MAX: usize = 2048;
const TCP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the runtime gets to answer a bridged query.
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// How long CPU0 gets to check and save an upload.
const SAVE_TIMEOUT: Duration = Duration::from_secs(120);
const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

// USART protocol tags (protocol.rs, ferrite_cli.py).
const TAG_MEMDETAIL: u8 = 0x58;
const TAG_MEMDETAIL_RESP: u8 = 0x32;
const TAG_PERF: u8 = 0x60;
const TAG_PERF_RESP: u8 = 0x4A;
const TAG_ERROR_RESP: u8 = 0x0A;

type TcpResult<T> = Result<T, embassy_net::tcp::Error>;

#[embassy_executor::task(pool_size = SERVERS)]
pub async fn server_task(stack: Stack<'static>) {
    let mut rx = vec![0u8; SERVER_RX];
    let mut tx = vec![0u8; SERVER_TX];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(TCP_TIMEOUT));
        if socket.accept(HTTP_PORT).await.is_err() {
            continue;
        }
        let restart = match serve(&mut socket).await {
            Ok(r) => r,
            Err(e) => {
                println!("ferrite: HTTP {:?}", e);
                false
            }
        };
        socket.close();
        let _ = with_timeout(Duration::from_secs(1), socket.flush()).await;
        socket.abort();
        if restart {
            super::request_restart();
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The value of header `name` (lower case) in a request head, trimmed.
fn header<'a>(head: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    head.split(|&b| b == b'\n').skip(1).find_map(|line| {
        let colon = line.iter().position(|&b| b == b':')?;
        line[..colon].eq_ignore_ascii_case(name).then(|| line[colon + 1..].trim_ascii())
    })
}

fn parse_decimal(text: &[u8]) -> Option<usize> {
    if text.is_empty() || text.len() > 10 {
        return None;
    }
    text.iter()
        .try_fold(0usize, |v, &b| b.is_ascii_digit().then(|| v.checked_mul(10)?.checked_add(usize::from(b - b'0')))?)
}

fn parse_hex(text: &[u8]) -> Option<u32> {
    let text = text.strip_prefix(b"0x").unwrap_or(text);
    if text.is_empty() || text.len() > 8 {
        return None;
    }
    text.iter().try_fold(0u32, |v, &b| Some(v << 4 | (b as char).to_digit(16)?))
}

/// The value of `key` in a query string `a=1&b=2`.
fn query_param<'a>(query: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    query.split(|&b| b == b'&').find_map(|kv| {
        let eq = kv.iter().position(|&b| b == b'=')?;
        (&kv[..eq] == key).then(|| &kv[eq + 1..])
    })
}

async fn answer(socket: &mut TcpSocket<'_>, status: &str, kind: &str, body: &[u8]) -> TcpResult<()> {
    let mut head = String::new();
    let _ = write!(
        head,
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        status,
        kind,
        body.len()
    );
    write_all(socket, head.as_bytes()).await?;
    write_all(socket, body).await?;
    socket.flush().await
}

/// `px` (the panel's XRGB8888 pixels, rows top to bottom) as a 32-bit BMP, top-down (negative
/// height): its rows are the pixels' bytes as they are (B, G, R, X).
async fn screen_bmp(socket: &mut TcpSocket<'_>, px: &[u32]) -> TcpResult<()> {
    let (w, h) = (crate::lcd::PANEL.width as u32, crate::lcd::PANEL.height as u32);
    let data = w * h * 4;
    let mut bmp = [0u8; 54];
    bmp[0..2].copy_from_slice(b"BM");
    bmp[2..6].copy_from_slice(&(54 + data).to_le_bytes());
    bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
    bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
    bmp[18..22].copy_from_slice(&w.to_le_bytes());
    bmp[22..26].copy_from_slice(&(-(h as i32)).to_le_bytes());
    bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
    bmp[28..30].copy_from_slice(&32u16.to_le_bytes());
    bmp[34..38].copy_from_slice(&data.to_le_bytes());
    let mut head = String::new();
    let _ = write!(
        head,
        "HTTP/1.1 200 OK
Content-Type: image/bmp
Content-Length: {}
Cache-Control: no-store
Connection: close

",
        54 + data
    );
    write_all(socket, head.as_bytes()).await?;
    write_all(socket, &bmp).await?;
    // SAFETY: plain words viewed as their bytes.
    let bytes = unsafe { core::slice::from_raw_parts(px.as_ptr() as *const u8, px.len() * 4) };
    write_all(socket, bytes).await?;
    socket.flush().await
}

async fn text(socket: &mut TcpSocket<'_>, status: &str, body: &str) -> TcpResult<()> {
    answer(socket, status, "text/plain", body.as_bytes()).await
}

async fn json(socket: &mut TcpSocket<'_>, status: &str, body: &str) -> TcpResult<()> {
    answer(socket, status, "application/json", body.as_bytes()).await
}

/// Answers one request. True = restart once the connection is closed.
async fn serve(socket: &mut TcpSocket<'_>) -> TcpResult<bool> {
    let mut request = [0u8; REQUEST_MAX];
    let mut filled = 0;
    let head_end = loop {
        if filled == request.len() {
            text(socket, "431 Request Header Fields Too Large", "Head too long\n").await?;
            return Ok(false);
        }
        let n = socket.read(&mut request[filled..]).await?;
        if n == 0 {
            return Ok(false);
        }
        filled += n;
        if let Some(at) = find(&request[..filled], b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = &request[..head_end];
    let line_end = find(head, b"\r\n").unwrap_or(head.len());
    let mut words = head[..line_end].split(|&b| b == b' ');
    let method = words.next().unwrap_or(b"");
    let target = words.next().unwrap_or(b"");
    let (path, query) = match target.iter().position(|&b| b == b'?') {
        Some(q) => (&target[..q], &target[q + 1..]),
        None => (target, &b""[..]),
    };

    match (method, path) {
        (b"GET", b"/") => answer(socket, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()).await?,
        (b"GET", b"/api/status") => json(socket, "200 OK", &status_json()).await?,
        (b"GET", b"/api/mem") => match bridged(TAG_MEMDETAIL, TAG_MEMDETAIL_RESP, None).await {
            Ok(p) => json(socket, "200 OK", &mem_json(&p)).await?,
            Err(e) => api_error(socket, e).await?,
        },
        (b"GET", b"/api/perf") => {
            let ms = query_param(query, b"ms").and_then(parse_decimal).unwrap_or(1000).clamp(50, 10_000);
            match bridged(TAG_PERF, TAG_PERF_RESP, Some(ms as u64)).await {
                Ok(p) => json(socket, "200 OK", &perf_json(&p)).await?,
                Err(e) => api_error(socket, e).await?,
            }
        }
        (b"POST" | b"PUT", b"/flash" | b"/fs") => {
            let length = header(head, b"content-length").and_then(parse_decimal);
            let want = header(head, b"x-crc32").and_then(parse_hex);
            let expect = header(head, b"expect").is_some_and(|v| v.eq_ignore_ascii_case(b"100-continue"));
            let early = &request[head_end..filled];
            upload(socket, length, want, expect, early).await?;
        }
        (b"GET", b"/api/screen") => match super::screenshot().await {
            Some(px) => screen_bmp(socket, &px).await?,
            None => text(socket, "503 Service Unavailable", "No screenshot (no memory or no answer)
").await?,
        },
        (b"POST", b"/api/compose") => {
            use core::sync::atomic::Ordering;
            match query_param(query, b"on") {
                Some(b"0") | Some(b"1") => {
                    let on = query_param(query, b"on") == Some(b"1");
                    crate::compose::ENABLED.store(on, Ordering::Relaxed);
                    crate::compose::REPAINT.store(true, Ordering::Release);
                    text(socket, "200 OK", if on { "GPU compositor on
" } else { "GPU compositor off
" }).await?;
                }
                _ => {
                    let on = crate::compose::ENABLED.load(Ordering::Relaxed);
                    text(socket, "400 Bad Request", if on { "on=0|1 (now on)
" } else { "on=0|1 (now off)
" }).await?;
                }
            }
        }
        (b"POST", b"/api/layerblit") => {
            use core::sync::atomic::Ordering;
            match query_param(query, b"on") {
                Some(v @ (b"0" | b"1")) => {
                    let on = v == b"1";
                    crate::lcd::LAYER_BLITS.store(on, Ordering::Relaxed);
                    text(socket, "200 OK", if on { "layer sprite blits on
" } else { "layer sprite blits off
" }).await?;
                }
                _ => {
                    let on = crate::lcd::LAYER_BLITS.load(Ordering::Relaxed);
                    text(socket, "400 Bad Request", if on { "on=0|1 (now on)
" } else { "on=0|1 (now off)
" }).await?;
                }
            }
        }
        (b"POST", b"/api/layercache") => {
            use core::sync::atomic::Ordering;
            match query_param(query, b"uncached") {
                Some(v @ (b"0" | b"1")) => {
                    let on = v == b"1";
                    crate::lcd::LAYER_UNCACHED.store(on, Ordering::Relaxed);
                    text(socket, "200 OK", if on { "layer shadows uncached
" } else { "layer shadows cached
" }).await?;
                }
                _ => {
                    let on = crate::lcd::LAYER_UNCACHED.load(Ordering::Relaxed);
                    text(socket, "400 Bad Request", if on { "uncached=0|1 (now 1)
" } else { "uncached=0|1 (now 0)
" }).await?;
                }
            }
        }
        (b"POST", b"/api/overlay") => {
            use crate::overlay_test as ov;
            use core::sync::atomic::Ordering;
            let signed = |key: &[u8]| {
                let v = query_param(query, key)?;
                match v.split_first() {
                    Some((b'-', rest)) => parse_decimal(rest).map(|n| -(n.min(4096) as i32)),
                    _ => parse_decimal(v).map(|n| n.min(4096) as i32),
                }
            };
            let mode = match query_param(query, b"mode") {
                Some(b"off") => Some(ov::OFF),
                Some(b"still") => Some(ov::STILL),
                Some(b"move") => Some(ov::MOVE),
                Some(b"fade") => Some(ov::FADE),
                Some(b"full") => Some(ov::FULL),
                _ => None,
            };
            match mode {
                Some(mode) => {
                    if let Some(x) = signed(b"x") {
                        ov::X.store(x, Ordering::Relaxed);
                    }
                    if let Some(y) = signed(b"y") {
                        ov::Y.store(y, Ordering::Relaxed);
                    }
                    if let Some(a) = query_param(query, b"alpha").and_then(parse_decimal) {
                        ov::ALPHA.store(a.min(255) as u8, Ordering::Relaxed);
                    }
                    ov::MODE.store(mode, Ordering::Release);
                    text(socket, "200 OK", "ok\n").await?;
                }
                None => text(socket, "400 Bad Request", "mode=off|still|move|fade|full [&x=X&y=Y&alpha=0..255]\n").await?,
            }
        }
        (b"POST", b"/restart") => {
            text(socket, "200 OK", "Restarting\n").await?;
            return Ok(true);
        }
        (b"GET", _) => text(socket, "404 Not Found", "Not found: see GET /\n").await?,
        _ => text(socket, "405 Method Not Allowed", "See GET /\n").await?,
    }
    Ok(false)
}

// --- bridged queries ----------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum ApiError {
    /// A `tcp://` bridge client holds the protocol.
    Busy,
    /// No (or no complete) reply in time: e.g. `/api/perf` on a build
    /// without the `perf` feature.
    Timeout,
    /// The runtime answered with an error code.
    Device(u8),
}

async fn api_error(socket: &mut TcpSocket<'_>, e: ApiError) -> TcpResult<()> {
    let mut body = String::new();
    let status = match e {
        ApiError::Busy => {
            body.push_str("{\"error\":\"busy: a protocol bridge client is connected\"}");
            "409 Conflict"
        }
        ApiError::Timeout => {
            body.push_str("{\"error\":\"no reply from the runtime (firmware without this report?)\"}");
            "504 Gateway Timeout"
        }
        ApiError::Device(code) => {
            let _ = write!(body, "{{\"error\":\"device error\",\"code\":{}}}", code);
            "502 Bad Gateway"
        }
    };
    json(socket, status, &body).await
}

/// Sends the one-byte request `tag` to the runtime through the bridge and
/// returns the payload of its `resp_tag` reply. With `repeat_after`, asks
/// twice, that many ms apart, and returns the second reply (perf: the
/// first resets the window).
async fn bridged(tag: u8, resp_tag: u8, repeat_after: Option<u64>) -> Result<Vec<u8>, ApiError> {
    let session = bridge::Session::take(bridge::HTTP).ok_or(ApiError::Busy)?;
    let begin = Instant::now();
    while !session.ready() {
        if begin.elapsed() > QUERY_TIMEOUT {
            return Err(ApiError::Timeout);
        }
        Timer::after(Duration::from_millis(1)).await;
    }
    if let Some(ms) = repeat_after {
        ask(tag, resp_tag).await?;
        Timer::after(Duration::from_millis(ms)).await;
    }
    ask(tag, resp_tag).await
}

async fn ask(tag: u8, resp_tag: u8) -> Result<Vec<u8>, ApiError> {
    bridge::TX.discard();
    bridge::RX.push(tag);
    let deadline = Instant::now() + QUERY_TIMEOUT;
    // 0 = looking for the tag, 1 = length varint, 2 = payload.
    let mut stage = 0u8;
    let mut is_error = false;
    let (mut len, mut shift) = (0usize, 0u32);
    let mut payload = Vec::new();
    loop {
        while let Some(b) = bridge::TX.pop() {
            match stage {
                0 if b == resp_tag || b == TAG_ERROR_RESP => {
                    is_error = b == TAG_ERROR_RESP;
                    stage = 1;
                }
                0 => {}
                1 => {
                    len |= usize::from(b & 0x7F) << shift;
                    shift += 7;
                    if b & 0x80 == 0 {
                        if len == 0 || len > 4096 {
                            return Err(ApiError::Timeout);
                        }
                        stage = 2;
                    } else if shift > 21 {
                        return Err(ApiError::Timeout);
                    }
                }
                _ => {
                    payload.push(b);
                    if payload.len() == len {
                        return if is_error { Err(ApiError::Device(payload[0])) } else { Ok(payload) };
                    }
                }
            }
        }
        if Instant::now() > deadline {
            return Err(ApiError::Timeout);
        }
        Timer::after(Duration::from_millis(1)).await;
    }
}

fn u32_at(p: &[u8], at: usize) -> u32 {
    p.get(at..at + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Pool names: `protocol::mem_pool`, as `ferrite_cli.py` `MEM_POOLS`.
fn pool_name(id: u8) -> (&'static str, &'static str) {
    match id {
        mem_pool::HEAP => ("heap", "B"),
        mem_pool::CPU_STACK => ("cpu stack", "B"),
        mem_pool::OBJ_ARENA => ("object arena", "B"),
        mem_pool::OBJ_PTRS => ("object ptrs", ""),
        mem_pool::UART_RX => ("uart rx", "B"),
        mem_pool::WIDGETS => ("widgets", ""),
        mem_pool::STRINGS => ("strings", ""),
        mem_pool::EVAL_STACK => ("eval stack", ""),
        mem_pool::CALL_STACK => ("call stack", ""),
        mem_pool::CB_QUEUE => ("callback queue", ""),
        mem_pool::ANIMS => ("animations", ""),
        mem_pool::ARRAYS => ("arrays", ""),
        _ => ("?", ""),
    }
}

/// MemDetail payload (`protocol::send_memdetail`) → JSON.
fn mem_json(p: &[u8]) -> String {
    let count = p.get(1).copied().unwrap_or(0) as usize;
    let mut s = String::from("{\"pools\":[");
    for i in 0..count {
        let at = 2 + i * 13;
        if at + 13 > p.len() {
            break;
        }
        let (name, unit) = pool_name(p[at]);
        let _ = write!(
            s,
            "{}{{\"id\":{},\"name\":\"{}\",\"unit\":\"{}\",\"used\":{},\"capacity\":{},\"extra\":{}}}",
            if i > 0 { "," } else { "" },
            p[at],
            name,
            unit,
            u32_at(p, at + 1),
            u32_at(p, at + 5),
            u32_at(p, at + 9)
        );
    }
    s.push_str("]}");
    s
}

/// Phase names: `perf::phase` / `perf::count`, as `ferrite_cli.py`
/// `PERF_PHASES` / `PERF_COUNTS`.
fn phase_name(id: u8) -> &'static str {
    match id {
        1 => "vm",
        2 => "usart",
        3 => "input",
        4 => "animations",
        5 => "audio",
        6 => "render",
        7 => "callbacks",
        8 => "tasks/misc",
        9 => "frame_end",
        10 => "display wait",
        11 => "widget draws",
        12 => "pixel writes",
        13 => "present copy",
        14 => "timer reads",
        15 => "G2D/GPU drawing",
        16 => "sprite drawing",
        0x81 => "draw_widget calls",
        0x82 => "pixel primitive calls",
        0x83 => "presents",
        0x84 => "present KiB",
        0x85 => "timer reads",
        0x86 => "G2D draw ops",
        0x87 => "G2D draw kpx",
        0x88 => "sprites made",
        0x89 => "sprite blits",
        0x8A => "sprite first sightings",
        0x8B => "CPU blended kpx",
        0x8C => "subtrees shifted",
        0x8D => "shifted kpx",
        0x8E => "G2D picture copies",
        0x8F => "G2D move copies",
        0x41..=0x50 => "longest pass",
        _ => "?",
    }
}

/// Perf payload (`protocol::send_perf`) → JSON. Ids ≥ 0x80 are counts, the
/// rest microseconds; 10–16 are sub-phases (not part of the step sum).
fn perf_json(p: &[u8]) -> String {
    let count = p.get(1).copied().unwrap_or(0) as usize;
    let instr = p.get(18..26).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])));
    let mut s = String::new();
    let _ = write!(
        s,
        "{{\"hz\":{},\"window_us\":{},\"passes\":{},\"max_pass_us\":{},\"vm_instructions\":{},\"phases\":[",
        u32_at(p, 2),
        u32_at(p, 6),
        u32_at(p, 10),
        u32_at(p, 14),
        instr
    );
    let mut first = true;
    let mut counts = String::new();
    for i in 0..count {
        let at = 26 + i * 5;
        if at + 5 > p.len() {
            break;
        }
        let (id, v) = (p[at], u32_at(p, at + 1));
        if id >= 0x40 {
            let _ = write!(counts, "{}{{\"id\":{},\"name\":\"{}\",\"n\":{}}}", if counts.is_empty() { "" } else { "," }, id, phase_name(id), v);
        } else {
            let _ = write!(
                s,
                "{}{{\"id\":{},\"name\":\"{}\",\"us\":{},\"sub\":{}}}",
                if first { "" } else { "," },
                id,
                phase_name(id),
                v,
                (10..=16).contains(&id)
            );
            first = false;
        }
    }
    let _ = write!(s, "],\"counts\":[{}]}}", counts);
    s
}

// --- status ---------------------------------------------------------------------------------------

fn hex6(s: &mut String, b: &[u8; 6]) {
    for (i, x) in b.iter().enumerate() {
        let _ = write!(s, "{}{:02x}", if i > 0 { ":" } else { "" }, x);
    }
}

fn status_json() -> String {
    let l = super::link();
    let w = wifi::stats();
    let h = heap::stats();
    let mut s = String::new();
    let _ = write!(s, "{{\"state\":\"{}\",\"hostname\":\"{}\",\"mac\":\"", super::state_name(), super::config().hostname);
    hex6(&mut s, &l.mac);
    s.push_str("\",\"bssid\":\"");
    hex6(&mut s, &l.bssid);
    let [a, b, c, d] = l.address;
    let _ = write!(
        s,
        "\",\"ssid\":\"{}\",\"channel\":{},\"rssi\":{},\"address\":\"{}.{}.{}.{}/{}\",\"bridge_port\":{},\"bridge\":\"{}\"",
        super::config().ssid.replace('"', "'"),
        l.channel,
        l.rssi,
        a,
        b,
        c,
        d,
        l.prefix,
        super::BRIDGE_PORT,
        match bridge::owner() {
            bridge::TCP => "tcp",
            bridge::HTTP => "http",
            _ => "free",
        }
    );
    // The GPU compositor: its start message (with the self-test's failing corner), whether it
    // is switched on, and why renders were composed or declined.
    let _ = write!(
        s,
        ",\"compositor\":{{\"enabled\":{},\"status\":\"{}\"",
        crate::compose::ENABLED.load(core::sync::atomic::Ordering::Relaxed),
        crate::gpu::status().replace('"', "'")
    );
    for (name, n) in crate::compose::COUNT_NAMES.iter().zip(&crate::compose::COUNTS) {
        let _ = write!(s, ",\"{}\":{}", name, n.load(core::sync::atomic::Ordering::Relaxed));
    }
    s.push_str(",\"time\":{");
    for (i, (name, n)) in crate::compose::TIME_NAMES.iter().zip(&crate::compose::TIMES).enumerate() {
        let _ = write!(s, "{}\"{}\":{}", if i == 0 { "" } else { "," }, name, n.load(core::sync::atomic::Ordering::Relaxed));
    }
    s.push('}');
    s.push('}');
    // FIMD window 1 test: mode and the address it scans (0 = hidden).
    let (mode, scan) = crate::overlay_test::status();
    let _ = write!(s, ",\"overlay\":{{\"mode\":{},\"scan\":\"0x{:08x}\"}}", mode, scan);
    let now_ms = exynos_4412_rs::embassy::now() / 1000;
    let _ = write!(
        s,
        ",\"uptime_s\":{},\"wifi_up_s\":{},\"frames_in\":{},\"frames_out\":{},\"wake_edges\":{},\"backstops\":{},\"missed\":{}",
        now_ms / 1000,
        now_ms.saturating_sub(l.joined_ms) / 1000,
        w.frames_in,
        w.frames_out,
        w.edges,
        w.backstops,
        w.missed
    );
    let _ = write!(
        s,
        ",\"heap_used\":{},\"heap_size\":{},\"passes\":{},\"frames\":{},\"uart_rx\":{},\"uart_dropped\":{}",
        h.used,
        h.size,
        crate::platform::passes(),
        crate::lcd::frames(),
        crate::usart::RX_TOTAL.load(core::sync::atomic::Ordering::Relaxed),
        crate::usart::RX_DROPPED.load(core::sync::atomic::Ordering::Relaxed)
    );
    #[cfg(feature = "perf")]
    let _ = write!(s, ",\"cpu_mhz\":{}", crate::perf::hz() / 1_000_000);
    s.push('}');
    s
}

// --- FS image upload ------------------------------------------------------------------------------

async fn upload(
    socket: &mut TcpSocket<'_>,
    length: Option<usize>,
    want: Option<u32>,
    expect: bool,
    early: &[u8],
) -> TcpResult<()> {
    let Some(length) = length else {
        return text(socket, "411 Length Required", "Send a Content-Length\n").await;
    };
    if !(16..=crate::flash::SIZE).contains(&length) {
        return text(socket, "413 Content Too Large", "16 B .. 64 MiB\n").await;
    }
    if super::flash_status() != super::FLASH_IDLE {
        return text(socket, "409 Conflict", "Busy: another upload is being saved\n").await;
    }
    let mut body = Vec::new();
    if body.try_reserve_exact(length).is_err() {
        return text(socket, "507 Insufficient Storage", "Out of heap for the image\n").await;
    }
    body.resize(length, 0);
    if expect {
        write_all(socket, CONTINUE).await?;
    }
    let mut done = early.len().min(length);
    body[..done].copy_from_slice(&early[..done]);
    let begin = Instant::now();
    while done < length {
        let n = socket.read(&mut body[done..]).await?;
        if n == 0 {
            println!("ferrite: HTTP flash body ended after {} of {} bytes", done, length);
            return Ok(());
        }
        done += n;
    }
    let ms = begin.elapsed().as_millis().max(1);
    let got = !ferrite_core::fs::crc32_update(!0, &body);
    println!("ferrite: HTTP flash {} bytes in {} ms, {} KiB/s", length, ms, length as u64 * 1000 / 1024 / ms);
    if let Some(want) = want
        && want != got
    {
        return text(socket, "400 Bad Request", "CRC mismatch: X-CRC32 does not match the body\n").await;
    }
    if !super::submit_image(body) {
        return text(socket, "409 Conflict", "Busy: another upload is being saved\n").await;
    }
    // CPU0 checks and saves it (net::service).
    let begin = Instant::now();
    let status = loop {
        let s = super::flash_status();
        if s != super::FLASH_PENDING {
            break s;
        }
        if begin.elapsed() > SAVE_TIMEOUT {
            break super::FLASH_PENDING;
        }
        Timer::after(Duration::from_millis(10)).await;
    };
    let result = match status {
        super::FLASH_SAVED => text(socket, "200 OK", "CRC OK, saved FERRITE/FLASH.BIN; restarting\n").await,
        super::FLASH_BAD_IMAGE => {
            text(socket, "400 Bad Request", "Not a valid FS image (mount or checksum failed)\n").await
        }
        super::FLASH_TOO_LARGE => text(socket, "413 Content Too Large", "Image larger than the 64 MiB region\n").await,
        super::FLASH_NO_STORAGE => {
            text(socket, "503 Service Unavailable", "No FAT32 on CACHE: the image cannot be kept\n").await
        }
        super::FLASH_SAVE_FAILED => text(socket, "500 Internal Server Error", "Save FAILED (see the console)\n").await,
        _ => text(socket, "504 Gateway Timeout", "The runtime did not take the image\n").await,
    };
    let _ = with_timeout(Duration::from_secs(1), socket.flush()).await;
    super::answered();
    result
}

// --- dashboard --------------------------------------------------------------------------------------

const PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>ferrite-ui</title>
<style>
:root{--bg:#fff;--fg:#1b1b1f;--mute:#6b6b76;--line:#e3e3e8;--acc:#2f6fdb}
@media(prefers-color-scheme:dark){:root{--bg:#17171b;--fg:#ececf1;--mute:#9a9aa6;--line:#2c2c34;--acc:#6d9bff}}
body{margin:0;padding:16px;background:var(--bg);color:var(--fg);font:14px/1.45 system-ui,sans-serif;max-width:860px}
h1{font-size:20px;margin:0 0 4px}h2{font-size:15px;margin:22px 0 8px}
.mute{color:var(--mute)}table{border-collapse:collapse;width:100%}
td,th{padding:3px 8px;border-bottom:1px solid var(--line);text-align:right;font-variant-numeric:tabular-nums}
td:first-child,th:first-child{text-align:left}
button{font:inherit;padding:4px 12px;border:1px solid var(--line);background:none;color:var(--acc);border-radius:6px;cursor:pointer}
#st{display:grid;grid-template-columns:max-content 1fr;gap:2px 16px}
</style></head><body>
<h1>ferrite-ui</h1><div class="mute">Exynos 4412 &middot; protocol bridge: <code>ferrite_cli -p tcp://<span id="h">HOST</span>:7000 &hellip;</code></div>
<h2>Status</h2><div id="st"></div>
<h2>Memory <button onclick="mem()">Refresh</button></h2><table id="mem"></table>
<h2>Profile <button onclick="perf()">Measure 1 s</button></h2><div id="pinfo" class="mute"></div><table id="perf"></table>
<h2>FS image</h2>
<input type="file" id="f"> <button onclick="up()">Upload &amp; restart</button> <span id="ur" class="mute"></span>
<p><button onclick="if(confirm('Restart the board?'))fetch('/restart',{method:'POST'})">Restart</button></p>
<script>
const $=id=>document.getElementById(id);
const fmt=(v,u)=>u=='B'?(v>=1<<20?(v/1048576).toFixed(1)+' MiB':v>=1024?(v/1024).toFixed(1)+' KiB':v+' B'):String(v);
async function status(){try{const s=await (await fetch('/api/status')).json();$('h').textContent=location.hostname;
const rows=[['WiFi',s.state+' · '+s.ssid+' · ch '+s.channel+' · '+s.rssi+' dBm'],['Address',s.address+' ('+s.hostname+')'],
['MAC',s.mac],['Uptime',s.uptime_s+' s'],['Heap',fmt(s.heap_used,'B')+' / '+fmt(s.heap_size,'B')],
['Passes / frames',s.passes+' / '+s.frames],['Bridge',s.bridge],['Frames in/out',s.frames_in+' / '+s.frames_out]];
if(s.cpu_mhz)rows.push(['CPU0',s.cpu_mhz+' MHz']);
$('st').innerHTML=rows.map(r=>'<span class="mute">'+r[0]+'</span><span>'+r[1]+'</span>').join('')}catch(e){}}
async function mem(){const r=await fetch('/api/mem');const j=await r.json();if(!r.ok){$('mem').innerHTML='<tr><td>'+j.error;return}
$('mem').innerHTML='<tr><th>pool<th>used<th>capacity<th>%<th>extra'+j.pools.map(p=>'<tr><td>'+p.name+'<td>'+fmt(p.used,p.unit)+'<td>'+
(p.capacity?fmt(p.capacity,p.unit):'n/a')+'<td>'+(p.capacity?(100*p.used/p.capacity).toFixed(1):'')+'<td>'+fmt(p.extra,'B')).join('')}
async function perf(){$('pinfo').textContent='measuring…';const r=await fetch('/api/perf?ms=1000');const j=await r.json();
if(!r.ok){$('pinfo').textContent=j.error;$('perf').innerHTML='';return}
const w=j.window_us,n=Math.max(j.passes,1);
$('pinfo').textContent=(j.hz/1e6).toFixed(0)+' MHz · '+(w/1000).toFixed(1)+' ms window · '+j.passes+' passes ('+(j.passes/w*1e6).toFixed(0)+'/s, max '+j.max_pass_us+' µs) · VM '+(j.vm_instructions/w).toFixed(1)+' M instr/s';
$('perf').innerHTML='<tr><th>phase<th>ms<th>share<th>µs/pass'+j.phases.map(p=>'<tr><td>'+(p.sub?'&nbsp;&nbsp;':'')+p.name+'<td>'+(p.us/1000).toFixed(1)+'<td>'+(100*p.us/w).toFixed(1)+'%<td>'+(p.us/n).toFixed(1)).join('')+
j.counts.map(c=>'<tr><td class="mute">'+c.name+'<td>'+c.n+'<td><td>'+(c.n/n).toFixed(1)).join('')}
async function up(){const f=$('f').files[0];if(!f)return;$('ur').textContent='uploading '+f.size+' bytes…';
try{const r=await fetch('/flash',{method:'POST',body:f});$('ur').textContent=await r.text()}catch(e){$('ur').textContent='failed: '+e}}
status();setInterval(status,2000);mem();
</script></body></html>
"#;
