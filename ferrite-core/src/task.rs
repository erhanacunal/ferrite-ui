//! Several programs at once (`multi_program`): one widget tree, one VM per
//! task.
//!
//! Task 0 is `main`; it (or any task) starts children with `spawnProgram`.
//! The VMs themselves live in the runtime (`runtime::Tasks`); this module holds
//! what the VM opcodes and the object pool need to see:
//!
//! - [`TaskTable`] (`Ctx::tasks`): slot states, the program each runs, its
//!   parent, the foreground task, and the requests the opcodes queue for the
//!   runtime (spawn = a `Starting` slot, kills, messages). The runtime acts on
//!   them once per main-loop pass, after the VM bursts.
//! - Object ownership: `ObjPool::owner` (the task that allocated an object;
//!   [`ObjPool::free_task`] frees them when it ends) and `ObjPool::handler`
//!   (the task a widget's on_click / on_tap / on_paint go to).
//!
//! Everything runs cooperatively on one core in the same main-loop pass, so
//! nothing here needs locking.
extern crate alloc;

use alloc::collections::VecDeque;

use crate::pool::{ObjPool, ObjPtr, T_ARRAY, T_STRING, T_WIDGET};
use crate::vm::ProgramRef;
use crate::widget::{KIND_GRAPH, WidgetId};

/// Task slots, `main` included. Ids are slot numbers (`0..MAX_TASKS`).
pub const MAX_TASKS: usize = 16;
/// `main`: started at boot, never killed by another task.
pub const TASK_MAIN: u8 = 0;
/// Owner of the runtime's own widgets (the task-fault dialog).
pub const TASK_SYSTEM: u8 = 0xFF;
/// Messages waiting for delivery; `sendMessage` returns 0 past this.
pub const MAIL_MAX: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotState {
    Free,
    /// `spawnProgram` reserved it; the runtime starts it after the burst.
    Starting,
    Running,
}

#[derive(Clone, Copy)]
pub struct SlotInfo {
    pub state: SlotState,
    pub program: ProgramRef,
    /// Task that spawned it: gets `on_task_exit` (main when it is gone).
    pub parent: u8,
}

impl SlotInfo {
    const FREE: Self = Self { state: SlotState::Free, program: ProgramRef::EMPTY, parent: TASK_MAIN };
}

/// A `sendMessage` on its way to `to`'s `on_task_message(from, a, b)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Msg {
    pub to: u8,
    pub from: u8,
    pub a: i32,
    pub b: i32,
}

pub struct TaskTable {
    pub slots: [SlotInfo; MAX_TASKS],
    /// Task that gets the global input events (touch down/move/up, swipe,
    /// long press).
    pub fg: u8,
    /// Kill requests (bit = slot), done by the runtime after the burst.
    pub kills: u32,
    pub mail: VecDeque<Msg>,
}

const _: () = assert!(MAX_TASKS <= 32 && MAX_TASKS < TASK_SYSTEM as usize);

impl TaskTable {
    pub fn new() -> Self {
        let mut t = Self { slots: [SlotInfo::FREE; MAX_TASKS], fg: TASK_MAIN, kills: 0, mail: VecDeque::new() };
        t.reset();
        t
    }

    /// Only `main`, running (boot, program switch, reload).
    pub fn reset(&mut self) {
        self.slots = [SlotInfo::FREE; MAX_TASKS];
        self.slots[TASK_MAIN as usize] =
            SlotInfo { state: SlotState::Running, program: ProgramRef::new(b"main", 0), parent: TASK_MAIN };
        self.fg = TASK_MAIN;
        self.kills = 0;
        self.mail.clear();
    }

    fn slot(&self, id: i32) -> Option<&SlotInfo> {
        usize::try_from(id).ok().and_then(|i| self.slots.get(i))
    }

    /// `id` names a task that is starting or running.
    pub fn alive(&self, id: i32) -> bool {
        self.slot(id).is_some_and(|s| s.state != SlotState::Free)
    }

    /// Reserve a slot for `program`; the runtime starts it after the burst.
    /// None when every slot is taken.
    pub fn spawn(&mut self, program: ProgramRef, parent: u8) -> Option<u8> {
        let i = self.slots.iter().position(|s| s.state == SlotState::Free)?;
        self.slots[i] = SlotInfo { state: SlotState::Starting, program, parent };
        Some(i as u8)
    }

    /// Ask the runtime to end task `id`. `main` can't be killed.
    pub fn request_kill(&mut self, id: i32) -> bool {
        if id == TASK_MAIN as i32 || !self.alive(id) {
            return false;
        }
        self.kills |= 1 << id;
        true
    }

    /// Queue a message for `to`. False if it is not alive or the queue is full.
    pub fn send(&mut self, msg: Msg) -> bool {
        if !self.alive(msg.to as i32) || self.mail.len() >= MAIL_MAX {
            return false;
        }
        if self.mail.try_reserve(1).is_err() {
            return false;
        }
        self.mail.push_back(msg);
        true
    }

    pub fn set_fg(&mut self, id: i32) -> bool {
        if !self.alive(id) {
            return false;
        }
        self.fg = id as u8;
        true
    }

    /// Forget task `id` (it ended): free its slot, hand its children to
    /// `main`, drop the mail addressed to it, give the foreground back to
    /// `main` if it had it. Returns the slot as it was.
    pub fn release(&mut self, id: u8) -> SlotInfo {
        let was = self.slots[id as usize];
        self.slots[id as usize] = SlotInfo::FREE;
        for s in self.slots.iter_mut() {
            if s.state != SlotState::Free && s.parent == id {
                s.parent = TASK_MAIN;
            }
        }
        self.mail.retain(|m| m.to != id);
        self.kills &= !(1 << id);
        if self.fg == id {
            self.fg = TASK_MAIN;
        }
        was
    }
}

impl Default for TaskTable {
    fn default() -> Self {
        Self::new()
    }
}

// --- Object ownership ---

impl ObjPool {
    /// Task that owns object `p` (`TASK_SYSTEM` for none).
    pub fn owner_of(&self, p: ObjPtr) -> u8 {
        self.owner.get(p as usize).copied().unwrap_or(TASK_SYSTEM)
    }

    /// Task that handles the events of widget `id`.
    pub fn handler_of(&self, id: WidgetId) -> u8 {
        self.handler.get(id.0 as usize).copied().unwrap_or(TASK_SYSTEM)
    }

    /// The running task now handles `id`'s events (it set a handler on it).
    pub(crate) fn take_handler(&mut self, id: WidgetId) {
        let t = self.cur_task;
        if let Some(h) = self.handler.get_mut(id.0 as usize) {
            *h = t;
        }
    }

    /// Free everything task `task` owns: its widgets (with their subtrees,
    /// whoever owns those), then its strings and arrays. A string or graph
    /// array that a surviving widget still shows goes to that widget's owner
    /// instead, so the widget keeps it. The caller repaints.
    pub fn free_task(&mut self, task: u8) {
        let n = self.ptr_count() as ObjPtr;
        for p in 0..n {
            let w = WidgetId(p);
            if self.owner_of(p) == task && w != self.root && self.is_live(w) {
                self.destroy_subtree(w);
            }
        }
        // What the remaining widgets show stays alive.
        for p in 0..n {
            let w = WidgetId(p);
            if self.type_of(p) != T_WIDGET || !self.is_live(w) {
                continue;
            }
            let keeper = self.owner_of(p);
            let text = self.text_id(w);
            if self.owner_of(text) == task && self.type_of(text) == T_STRING {
                self.owner[text as usize] = keeper;
            }
            if self.get(w).kind == KIND_GRAPH {
                let arr = self.get(w).value as ObjPtr;
                if self.type_of(arr) == T_ARRAY && self.owner_of(arr) == task {
                    self.owner[arr as usize] = keeper;
                }
            }
        }
        for p in 0..n {
            if self.owner_of(p) != task {
                continue;
            }
            match self.type_of(p) {
                T_STRING => self.free_obj(p, T_STRING),
                T_ARRAY => self.free_obj(p, T_ARRAY),
                _ => {}
            }
        }
    }

    /// Live objects task `task` owns, per type (`[_, widgets, strings, arrays]`).
    pub fn task_objects(&self, task: u8) -> [usize; 4] {
        let mut n = [0; 4];
        for p in 0..self.ptr_count() as ObjPtr {
            let ty = self.type_of(p);
            if ty != crate::pool::T_FREE && self.owner_of(p) == task {
                n[ty as usize] += 1;
            }
        }
        n
    }
}

/// The slots set in a kill mask, lowest first.
pub fn mask_slots(mask: u32) -> impl Iterator<Item = u8> {
    (0..MAX_TASKS as u8).filter(move |&i| mask & (1 << i) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::KIND_LABEL;

    #[test]
    fn table_spawn_kill_release() {
        let mut t = TaskTable::new();
        assert!(t.alive(0) && !t.alive(1) && !t.alive(-1) && !t.alive(99));
        let a = t.spawn(ProgramRef::new(b"clock", 7), 0).unwrap();
        let b = t.spawn(ProgramRef::new(b"chat", 0), a).unwrap();
        assert_eq!((a, b), (1, 2));
        assert_eq!(t.slots[a as usize].state, SlotState::Starting);
        assert!(!t.request_kill(0), "main can't be killed");
        assert!(t.request_kill(a as i32));
        assert!(t.send(Msg { to: a, from: 0, a: 1, b: 2 }));
        assert!(t.set_fg(a as i32));
        let was = t.release(a);
        assert_eq!(was.program.arg, 7);
        assert!(!t.alive(a as i32));
        assert_eq!(t.slots[b as usize].parent, TASK_MAIN, "orphans go to main");
        assert!(t.mail.is_empty() && t.kills == 0 && t.fg == TASK_MAIN);
        assert!(!t.send(Msg { to: a, from: 0, a: 0, b: 0 }), "dead task");
        // Slots run out.
        while t.spawn(ProgramRef::new(b"x", 0), 0).is_some() {}
        assert!(t.slots.iter().all(|s| s.state != SlotState::Free));
    }

    #[test]
    fn mail_is_capped() {
        let mut t = TaskTable::new();
        for i in 0..MAIL_MAX {
            assert!(t.send(Msg { to: 0, from: 0, a: i as i32, b: 0 }));
        }
        assert!(!t.send(Msg { to: 0, from: 0, a: 0, b: 0 }));
    }

    #[test]
    fn objects_belong_to_the_allocating_task() {
        let mut pool = ObjPool::new();
        let root = pool.alloc_widget().unwrap();
        pool.root = root;
        pool.cur_task = 2;
        let panel = pool.alloc_widget().unwrap();
        pool.add_child(root, panel);
        let label = pool.alloc_widget().unwrap();
        pool.add_child(panel, label);
        let s = pool.str_alloc(b"hi").unwrap();
        let arr = pool.arr_alloc(3).unwrap();
        assert_eq!((pool.owner_of(panel.0), pool.owner_of(s), pool.handler_of(label)), (2, 2, 2));
        // Task 1 puts a label inside task 2's panel and shows a task-2 string on its own one.
        pool.cur_task = 1;
        let guest = pool.alloc_widget().unwrap();
        pool.add_child(panel, guest);
        let own = pool.alloc_widget().unwrap();
        pool.add_child(root, own);
        pool.get_mut(own).kind = KIND_LABEL;
        pool.set_text_id(own, s);
        pool.take_handler(label);
        assert_eq!(pool.handler_of(label), 1);

        pool.free_task(2);
        assert!(!pool.is_live(panel) && !pool.is_live(label));
        assert!(!pool.is_live(guest), "inside a freed subtree");
        assert!(pool.is_live(own) && pool.is_live(root));
        assert_eq!(pool.str_get(s), b"hi", "still shown: kept");
        assert_eq!(pool.owner_of(s), 1, "by the widget's owner");
        assert_eq!(pool.type_of(arr), crate::pool::T_FREE);
        assert_eq!(pool.task_objects(2), [0; 4]);
    }

    #[test]
    fn sweep_and_literals_are_per_task() {
        let mut pool = ObjPool::new();
        pool.cur_task = 1;
        let a1 = pool.str_intern(b"OK").unwrap();
        let tmp1 = pool.str_alloc(b"tmp").unwrap();
        pool.cur_task = 2;
        let a2 = pool.str_intern(b"OK").unwrap();
        assert_ne!(a1, a2, "a literal is not shared across tasks");
        assert_eq!(pool.str_intern(b"OK"), Some(a2));
        pool.str_sweep();
        assert_eq!(pool.str_get(tmp1), b"tmp", "task 2's sweep leaves task 1's strings");
        assert_eq!(pool.type_of(a2), crate::pool::T_FREE);
        pool.cur_task = 1;
        pool.str_sweep();
        assert_eq!(pool.type_of(tmp1), crate::pool::T_FREE);
    }
}
