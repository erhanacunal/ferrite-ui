//! Cores that are stopped and started again: CPU1 (UART RX, `usart.rs`) and CPU3 (WiFi,
//! `net/`) are stopped for standby and brought back at the wake (HAL `smp::stop_this_cpu` /
//! `smp::start_cpu`, `platform::enter_standby`).
//!
//! A stopped core's executor must have no task left: a task that never ended keeps its pool
//! slot, and spawning it again after the restart fails. So every task on such a core runs
//! inside a [`Group`] ([`Group::run`]), which ends it when CPU0 asks ([`Group::stop`]); the
//! core's executor (HAL `embassy::run_until`) returns once all of them have
//! ([`Group::done`]), and the core stops itself.

use core::cell::RefCell;
use core::future::{Future, poll_fn};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::task::Poll;

use embassy_sync::blocking_mutex::CriticalSectionMutex;
use embassy_sync::waitqueue::MultiWakerRegistration;

/// More than the tasks of any group (CPU3: four plus the HTTP servers).
const WAKERS: usize = 12;

/// The tasks of one stoppable core.
pub struct Group {
    stop: AtomicBool,
    /// Tasks spawned and not ended yet.
    live: AtomicU32,
    wakers: CriticalSectionMutex<RefCell<MultiWakerRegistration<WAKERS>>>,
}

impl Group {
    pub const fn new() -> Self {
        Self { stop: AtomicBool::new(false), live: AtomicU32::new(0), wakers: CriticalSectionMutex::new(RefCell::new(MultiWakerRegistration::new())) }
    }

    /// CPU0, before it starts the core (again).
    pub fn arm(&self) {
        self.stop.store(false, Ordering::Release);
    }

    /// On the core, before it spawns its `tasks` tasks, each through [`run`](Self::run). A
    /// [`stop`](Self::stop) that came before this still ends them.
    pub fn begin(&self, tasks: u32) {
        self.live.store(tasks, Ordering::Release);
    }

    /// Runs `task` until it ends or the group is stopped.
    pub async fn run<T>(&self, task: impl Future<Output = T>) {
        embassy_futures::select::select(task, self.stopped()).await;
        self.live.fetch_sub(1, Ordering::AcqRel);
    }

    async fn stopped(&self) {
        poll_fn(|cx| {
            self.wakers.lock(|w| {
                if self.stop.load(Ordering::Acquire) {
                    Poll::Ready(())
                } else {
                    w.borrow_mut().register(cx.waker());
                    Poll::Pending
                }
            })
        })
        .await
    }

    /// CPU0: ends every task of the group (each at its next poll).
    pub fn stop(&self) {
        self.wakers.lock(|w| {
            self.stop.store(true, Ordering::Release);
            w.borrow_mut().wake();
        });
    }

    /// The core's executor may return: stopped, and every task has ended.
    pub fn done(&self) -> bool {
        self.stop.load(Ordering::Acquire) && self.live.load(Ordering::Acquire) == 0
    }
}
