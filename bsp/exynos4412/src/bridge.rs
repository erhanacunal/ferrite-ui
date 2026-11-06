//! The USART protocol over the network: while a network session holds the
//! bridge, the runtime's protocol channel (`usart::HwUart`) reads [`RX`] and
//! writes [`TX`] instead of UART2, so every `ferrite_cli` command (writefs,
//! memdetail, perf, …) works over TCP unchanged.
//!
//! - CPU3 (`net/`) owns the network side: it pushes received bytes into
//!   [`RX`] and sends what it pops from [`TX`].
//! - CPU0 (the runtime) pops [`RX`] and pushes [`TX`].
//!
//! One session at a time ([`Session::take`]): the TCP bridge port, or an HTTP
//! request that asks the runtime something (`/api/mem`, `/api/perf`). UART2
//! bytes that arrive meanwhile wait in its own ring. Both rings are
//! single-producer / single-consumer across the two cores.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

/// A lock-free single-producer / single-consumer byte ring (`N` a power of two).
pub struct Ring<const N: usize> {
    buf: UnsafeCell<[u8; N]>,
    head: AtomicUsize,
    tail: AtomicUsize,
}

// SAFETY: one producer writes slots outside [tail, head) and publishes them
// with a Release store of `head`; one consumer reads [tail, head) after an
// Acquire load and frees them with a Release store of `tail`.
unsafe impl<const N: usize> Sync for Ring<N> {}

impl<const N: usize> Ring<N> {
    const MASK: usize = N - 1;

    pub const fn new() -> Self {
        assert!(N.is_power_of_two());
        Self { buf: UnsafeCell::new([0; N]), head: AtomicUsize::new(0), tail: AtomicUsize::new(0) }
    }

    /// Bytes waiting.
    pub fn len(&self) -> usize {
        self.head.load(Ordering::Acquire).wrapping_sub(self.tail.load(Ordering::Acquire)) & Self::MASK
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Producer: stores what fits of `data`, returns how much.
    pub fn push_slice(&self, data: &[u8]) -> usize {
        let mut head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let mut n = 0;
        for &b in data {
            let next = (head + 1) & Self::MASK;
            if next == tail {
                break;
            }
            // SAFETY: slot `head` is not in [tail, head) (see the Sync impl).
            unsafe { (*self.buf.get())[head] = b };
            head = next;
            n += 1;
        }
        self.head.store(head, Ordering::Release);
        n
    }

    pub fn push(&self, b: u8) -> bool {
        self.push_slice(&[b]) == 1
    }

    /// Consumer: takes up to `out.len()` bytes, returns how many.
    pub fn pop_slice(&self, out: &mut [u8]) -> usize {
        let mut tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let mut n = 0;
        while tail != head && n < out.len() {
            // SAFETY: [tail, head) was published by the producer.
            out[n] = unsafe { (*self.buf.get())[tail] };
            tail = (tail + 1) & Self::MASK;
            n += 1;
        }
        self.tail.store(tail, Ordering::Release);
        n
    }

    pub fn pop(&self) -> Option<u8> {
        let mut b = [0];
        (self.pop_slice(&mut b) == 1).then_some(b[0])
    }

    /// Consumer: drops everything waiting.
    pub fn discard(&self) {
        self.tail.store(self.head.load(Ordering::Acquire), Ordering::Release);
    }

    pub const fn capacity(&self) -> usize {
        N
    }
}

/// Network → runtime.
pub static RX: Ring<{ 64 * 1024 }> = Ring::new();
/// Runtime → network.
pub static TX: Ring<{ 16 * 1024 }> = Ring::new();

/// Who holds the bridge: [`NONE`] (protocol on UART2), [`TCP`], [`HTTP`].
static OWNER: AtomicU8 = AtomicU8::new(NONE);
pub const NONE: u8 = 0;
pub const TCP: u8 = 1;
pub const HTTP: u8 = 2;
/// Set by a new session: the runtime drops what an earlier session left in
/// [`RX`] (it is RX's consumer), then clears it. The session sends nothing
/// before that.
static RX_DISCARD: AtomicBool = AtomicBool::new(false);

/// Whether the protocol currently runs over the network.
#[inline]
pub fn active() -> bool {
    OWNER.load(Ordering::Acquire) != NONE
}

pub fn owner() -> u8 {
    OWNER.load(Ordering::Relaxed)
}

/// Runtime side (CPU0): the next byte for the protocol parser.
pub fn rx_pop() -> Option<u8> {
    if RX_DISCARD.load(Ordering::Acquire) {
        RX.discard();
        RX_DISCARD.store(false, Ordering::Release);
        return None;
    }
    RX.pop()
}

/// Runtime side (CPU0): a reply byte. Waits up to ~100 ms for room (CPU3
/// sends every millisecond), then drops it.
pub fn tx_push(b: u8, now_ms: impl Fn() -> u32) {
    if TX.push(b) {
        return;
    }
    let start = now_ms();
    while active() && now_ms().wrapping_sub(start) < 100 {
        if TX.push(b) {
            return;
        }
        core::hint::spin_loop();
    }
}

/// Runtime side (CPU0): waits up to `ms` until CPU3 has taken every reply
/// byte (`flush`, and before a restart).
pub fn tx_drain(ms: u32, now_ms: impl Fn() -> u32) {
    let start = now_ms();
    while active() && !TX.is_empty() && now_ms().wrapping_sub(start) < ms {
        core::hint::spin_loop();
    }
}

/// The bridge held by a network client, released on drop.
pub struct Session(());

impl Session {
    /// Claims the bridge for `who` ([`TCP`] / [`HTTP`]); `None` if another
    /// session holds it. Callers wait for [`Session::ready`] before sending.
    pub fn take(who: u8) -> Option<Self> {
        OWNER.compare_exchange(NONE, who, Ordering::AcqRel, Ordering::Relaxed).ok()?;
        // CPU3 is TX's consumer: stale replies go now; RX's leftovers go on
        // the runtime's next read.
        TX.discard();
        RX_DISCARD.store(true, Ordering::Release);
        Some(Self(()))
    }

    /// Whether the runtime has dropped the earlier session's leftovers, so
    /// [`RX`] may be written.
    pub fn ready(&self) -> bool {
        !RX_DISCARD.load(Ordering::Acquire)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        OWNER.store(NONE, Ordering::Release);
    }
}
