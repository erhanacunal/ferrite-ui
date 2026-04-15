/// USART0 serial driver — GD32F103 (APB2)
///
/// Pin ataması (Nextion donanımı):
///   PA9  = TX (AF push-pull, 50MHz)
///   PA10 = RX (floating input)
///
/// 115200 baud, 8N1, no flow control.
/// GPIO ve clock konfigürasyonu init_ports() tarafından yapılır.
///
/// RX interrupt: RBNE → ring buffer'a yazar, `rx_has_data()` flag set eder.
/// Main loop'ta `rx_read_byte()` ile okunur.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};

use super::UsartBackend;

const USART0_BASE: u32 = 0x4001_3800;
const USART_STAT: u32 = USART0_BASE + 0x00;
const USART_DATA: u32 = USART0_BASE + 0x04;
const USART_BAUD: u32 = USART0_BASE + 0x08;
const USART_CTL0: u32 = USART0_BASE + 0x0C;
const USART_CTL1: u32 = USART0_BASE + 0x10;
const USART_CTL2: u32 = USART0_BASE + 0x14;

const NVIC_ISER1: u32 = 0xE000_E104;

const STAT_TBE: u32 = 1 << 7;
const STAT_TC: u32 = 1 << 6;
const STAT_RBNE: u32 = 1 << 5;

const CTL0_UEN: u32 = 1 << 13;
const CTL0_TE: u32 = 1 << 3;
const CTL0_RE: u32 = 1 << 2;
const CTL0_RBNEIE: u32 = 1 << 5;

// --- RX Ring Buffer ---
// ISR yazar (head), main okur (tail). SPSC — lock gerekmez.

const RX_BUF_SIZE: usize = 128;
const RX_BUF_MASK: u8 = (RX_BUF_SIZE - 1) as u8;

static mut RX_BUF: [u8; RX_BUF_SIZE] = [0; RX_BUF_SIZE];
static mut RX_HEAD: u8 = 0;
static mut RX_TAIL: u8 = 0;
static RX_READY: AtomicBool = AtomicBool::new(false);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn USART0() {
    unsafe {
        let stat = read_volatile(USART_STAT as *const u32);

        if stat & STAT_RBNE != 0 {
            let byte = read_volatile(USART_DATA as *const u32) as u8;

            let head = read_volatile(&raw const RX_HEAD);
            write_volatile(&raw mut RX_BUF[head as usize], byte);

            let new_head = (head.wrapping_add(1)) & RX_BUF_MASK;
            write_volatile(&raw mut RX_HEAD, new_head);

            let tail = read_volatile(&raw const RX_TAIL);
            if new_head == tail {
                write_volatile(&raw mut RX_TAIL, (tail.wrapping_add(1)) & RX_BUF_MASK);
            }

            RX_READY.store(true, Ordering::Release);
        }
    }
}

pub struct Hw {
    _private: (),
}

impl Hw {
    pub fn init() -> Self {
        unsafe {
            write_volatile(USART_CTL0 as *mut u32, 0);
            write_volatile(USART_BAUD as *mut u32, 0x3AA);
            write_volatile(USART_CTL1 as *mut u32, 0);
            write_volatile(USART_CTL2 as *mut u32, 0);
            write_volatile(
                USART_CTL0 as *mut u32,
                CTL0_UEN | CTL0_TE | CTL0_RE | CTL0_RBNEIE,
            );

            let val = read_volatile(NVIC_ISER1 as *const u32);
            write_volatile(NVIC_ISER1 as *mut u32, val | (1 << 5));
        }

        Hw { _private: () }
    }
}

impl UsartBackend for Hw {
    fn write_byte(&self, byte: u8) {
        unsafe {
            while read_volatile(USART_STAT as *const u32) & STAT_TBE == 0 {}
            write_volatile(USART_DATA as *mut u32, byte as u32);
        }
    }

    fn flush(&self) {
        unsafe {
            while read_volatile(USART_STAT as *const u32) & STAT_TC == 0 {}
        }
    }
}

// --- RX public API (free fns) ---

pub fn rx_has_data() -> bool {
    RX_READY.load(Ordering::Acquire)
}

pub fn rx_read_byte() -> Option<u8> {
    unsafe {
        let tail = read_volatile(&raw const RX_TAIL);
        let head = read_volatile(&raw const RX_HEAD);

        if tail == head {
            RX_READY.store(false, Ordering::Release);
            return None;
        }

        let byte = read_volatile(&raw const RX_BUF[tail as usize]);
        let new_tail = (tail.wrapping_add(1)) & RX_BUF_MASK;
        write_volatile(&raw mut RX_TAIL, new_tail);

        if new_tail == read_volatile(&raw const RX_HEAD) {
            RX_READY.store(false, Ordering::Release);
        }

        Some(byte)
    }
}

pub fn rx_len() -> u8 {
    unsafe {
        let head = read_volatile(&raw const RX_HEAD);
        let tail = read_volatile(&raw const RX_TAIL);
        (head.wrapping_sub(tail)) & RX_BUF_MASK
    }
}

// --- Debug output (free fns, no Usart instance) ---

pub fn dbg(data: &[u8]) {
    unsafe {
        for &b in data {
            while read_volatile(USART_STAT as *const u32) & STAT_TBE == 0 {}
            write_volatile(USART_DATA as *mut u32, b as u32);
        }
    }
}

pub fn dbg_u16(val: u16) {
    let mut buf = [0u8; 5];
    let mut n = val;
    let mut pos = 5;
    if n == 0 {
        dbg(b"0");
        return;
    }
    while n > 0 {
        pos -= 1;
        buf[pos] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    dbg(&buf[pos..]);
}

pub fn rx_clear() {
    unsafe {
        cortex_m::interrupt::free(|_| {
            write_volatile(&raw mut RX_HEAD, 0);
            write_volatile(&raw mut RX_TAIL, 0);
            RX_READY.store(false, Ordering::Release);
        });
    }
}
