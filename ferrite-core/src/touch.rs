/// Backend abstraction for touch input.
/// Hardware backends provide raw press detection and sample reading;
/// the framework provides state-machine debouncing.

// === Calibration ===

#[derive(Clone, Copy)]
pub struct CalParams {
    pub xy_swap: bool,
    pub x_flip: bool,
    pub y_flip: bool,
    pub x_min: u16,
    pub x_max: u16,
    pub y_min: u16,
    pub y_max: u16,
}

impl CalParams {
    pub fn default() -> Self {
        Self {
            xy_swap: true,
            x_flip: true,
            y_flip: true,
            x_min: 0,
            x_max: 4095,
            y_min: 0,
            y_max: 4095,
        }
    }

    pub fn to_bytes(&self) -> [u8; 9] {
        let mut buf = [0u8; 9];
        buf[0] = (self.xy_swap as u8) | ((self.x_flip as u8) << 1) | ((self.y_flip as u8) << 2);
        buf[1..3].copy_from_slice(&self.x_min.to_le_bytes());
        buf[3..5].copy_from_slice(&self.x_max.to_le_bytes());
        buf[5..7].copy_from_slice(&self.y_min.to_le_bytes());
        buf[7..9].copy_from_slice(&self.y_max.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < 9 {
            return None;
        }
        Some(Self {
            xy_swap: buf[0] & 0x01 != 0,
            x_flip: buf[0] & 0x02 != 0,
            y_flip: buf[0] & 0x04 != 0,
            x_min: u16::from_le_bytes([buf[1], buf[2]]),
            x_max: u16::from_le_bytes([buf[3], buf[4]]),
            y_min: u16::from_le_bytes([buf[5], buf[6]]),
            y_max: u16::from_le_bytes([buf[7], buf[8]]),
        })
    }
}

// === Backend trait ===

pub trait TouchBackend {
    /// Fast path: true if the input device reports a press right now.
    fn is_active(&self) -> bool;

    /// Read a screen-space (x, y) sample if currently pressed.
    fn read_screen(&self, cal: &CalParams) -> Option<(u16, u16)>;
}

// === Touch state machine ===

const RELEASE_DEBOUNCE: u8 = 5;

#[derive(Clone, Copy, PartialEq)]
enum TouchState {
    Idle,
    Pressed,
    Held,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TouchEventKind {
    Press,
    Hold,
    Release,
}

#[derive(Clone, Copy)]
pub struct TouchEvent {
    pub kind: TouchEventKind,
    pub x: u16,
    pub y: u16,
}

pub struct TouchImpl<B: TouchBackend> {
    be: B,
    state: TouchState,
    fail_count: u8,
    last_x: u16,
    last_y: u16,
    pub cal: CalParams,
}

impl<B: TouchBackend> TouchImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self {
            be,
            state: TouchState::Idle,
            fail_count: 0,
            last_x: 0,
            last_y: 0,
            cal: CalParams::default(),
        }
    }

    pub fn poll(&mut self) -> Option<TouchEvent> {
        if self.state == TouchState::Idle && !self.be.is_active() {
            return None;
        }

        let sample = self.be.read_screen(&self.cal);

        match self.state {
            TouchState::Idle => {
                if let Some((x, y)) = sample {
                    self.last_x = x;
                    self.last_y = y;
                    self.fail_count = 0;
                    self.state = TouchState::Pressed;
                    Some(TouchEvent {
                        kind: TouchEventKind::Press,
                        x,
                        y,
                    })
                } else {
                    None
                }
            }
            TouchState::Pressed | TouchState::Held => {
                if let Some((x, y)) = sample {
                    self.last_x = x;
                    self.last_y = y;
                    self.fail_count = 0;
                    self.state = TouchState::Held;
                    Some(TouchEvent {
                        kind: TouchEventKind::Hold,
                        x,
                        y,
                    })
                } else {
                    self.fail_count += 1;
                    if self.fail_count >= RELEASE_DEBOUNCE {
                        self.state = TouchState::Idle;
                        self.fail_count = 0;
                        Some(TouchEvent {
                            kind: TouchEventKind::Release,
                            x: self.last_x,
                            y: self.last_y,
                        })
                    } else {
                        None
                    }
                }
            }
        }
    }
}

// Hit test — pure widget-tree logic (WidgetTree lives in this crate).
pub fn hit_test(tree: &mut crate::widget::WidgetTree, x: u16, y: u16) -> crate::widget::WidgetId {
    use crate::widget::FLAG_CLICKABLE;
    let dfs = tree.dfs_order();
    let mut result = crate::widget::WidgetId::NONE;

    for i in 0..dfs.len() {
        let id = dfs[i];
        let w = tree.get(id);
        if w.flags & FLAG_CLICKABLE == 0 || !tree.is_tree_visible(id) {
            continue;
        }
        let abs = tree.absolute_rect(id);
        if (x as i16) >= abs.x
            && (x as i16) < abs.right()
            && (y as i16) >= abs.y
            && (y as i16) < abs.bottom()
        {
            let sp = tree.scroll_parent(id);
            if sp.is_some() {
                let viewport = tree.scroll_viewport(sp);
                if !abs.intersects(&viewport) {
                    continue;
                }
            }
            result = id;
        }
    }
    result
}

#[cfg(feature = "mock")]
pub type Touch = TouchImpl<crate::mock::MockTouch>;
