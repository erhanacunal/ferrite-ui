/// USART protocol — protobuf-style tag encoding
///
/// Encoding:
///   Varint type:  1 byte  — tag only (wire_type + field_no)
///   Payload type: N bytes — tag + length(varint) + payload
///
/// Device sends (TX):
///   Field 1, Payload = Error (payload: error code byte)
///   Field 2, Varint  = Pong
///
/// Device receives (RX):
///   Field 1, Varint  = Ping
///   Field 2, Payload = Execute (payload: program bytecode)
///   Field 3, Varint  = Restart
///   Field 4, Payload = WriteFs (payload: flash filesystem image)

use crate::flash::Flash;
use crate::usart::Usart;

/// Maximum program bytecode size received via USART
const MAX_PROGRAM_SIZE: usize = 1024;

// --- RX State Machine ---

#[derive(Clone, Copy)]
enum RxState {
    /// Waiting for a tag byte
    Idle,
    /// Reading payload length varint (for Payload-type messages)
    Length {
        field: u8,
        value: u32,
        shift: u8,
    },
    /// Reading payload bytes
    Payload {
        field: u8,
        remaining: u32,
    },
}

// --- FS Writer (streaming flash write) ---

struct FsWriter {
    addr: u32,
    page_buf: [u8; 256],
    page_pos: u16,
}

impl FsWriter {
    const fn new() -> Self {
        Self {
            addr: 0,
            page_buf: [0; 256],
            page_pos: 0,
        }
    }

    fn reset(&mut self) {
        self.addr = 0;
        self.page_pos = 0;
    }

    fn write_byte(&mut self, byte: u8, flash: &Flash) {
        self.page_buf[self.page_pos as usize] = byte;
        self.page_pos += 1;

        if self.page_pos == 256 {
            self.flush_page(flash);
        }
    }

    fn flush_page(&mut self, flash: &Flash) {
        let len = self.page_pos as usize;
        if len == 0 {
            return;
        }

        // Erase sector at 4KB boundary
        if self.addr & 0xFFF == 0 {
            flash.erase_sector(self.addr);
        }

        flash.write(self.addr, &self.page_buf[..len]);
        self.addr += len as u32;
        self.page_pos = 0;
    }

    fn flush(&mut self, flash: &Flash) {
        if self.page_pos > 0 {
            self.flush_page(flash);
        }
    }
}

// --- Events ---

#[derive(Clone, Copy, PartialEq)]
pub enum RxEvent {
    None,
    Ping,
    Restart,
    ProgramReady,
    FsWriteComplete,
}

// --- Protocol ---

pub struct Protocol {
    state: RxState,
    program_buf: [u8; MAX_PROGRAM_SIZE],
    program_len: u16,
    fs_writer: FsWriter,
}

impl Protocol {
    pub const fn new() -> Self {
        Self {
            state: RxState::Idle,
            program_buf: [0; MAX_PROGRAM_SIZE],
            program_len: 0,
            fs_writer: FsWriter::new(),
        }
    }

    /// Return program bytecode (valid after ProgramReady event).
    pub fn program_code(&self) -> &[u8] {
        &self.program_buf[..self.program_len as usize]
    }

    /// Process a single received byte. Returns an event if a complete message was received.
    pub fn feed(&mut self, byte: u8, flash: &Flash) -> RxEvent {
        match self.state {
            RxState::Idle => {
                // Tag byte: (field << 3) | wire_type
                let wt = byte & 0x07;
                let field = byte >> 3;

                match wt {
                    // Varint type: tag byte only, no additional data
                    0 => match field {
                        1 => RxEvent::Ping,
                        3 => RxEvent::Restart,
                        _ => RxEvent::None,
                    },
                    // Payload type: tag + length varint + payload
                    2 => {
                        match field {
                            2 => {
                                // Execute — read length, then bytecode
                                self.program_len = 0;
                                self.state = RxState::Length {
                                    field: 2,
                                    value: 0,
                                    shift: 0,
                                };
                            }
                            4 => {
                                // WriteFs — read length, then flash image
                                self.fs_writer.reset();
                                self.state = RxState::Length {
                                    field: 4,
                                    value: 0,
                                    shift: 0,
                                };
                            }
                            _ => {} // Unknown field — ignore
                        }
                        RxEvent::None
                    }
                    _ => RxEvent::None, // Unknown wire type — ignore
                }
            }

            RxState::Length {
                field,
                ref mut value,
                ref mut shift,
            } => {
                *value |= ((byte & 0x7F) as u32) << *shift;

                if byte & 0x80 == 0 {
                    // Length varint complete
                    let len = *value;
                    let f = field;
                    self.state = RxState::Idle;

                    if len == 0 {
                        // Zero-length payload — message complete immediately
                        match f {
                            2 => RxEvent::ProgramReady,
                            4 => RxEvent::FsWriteComplete,
                            _ => RxEvent::None,
                        }
                    } else {
                        self.state = RxState::Payload {
                            field: f,
                            remaining: len,
                        };
                        RxEvent::None
                    }
                } else {
                    *shift += 7;
                    if *shift >= 35 {
                        // Varint overflow — reset
                        self.state = RxState::Idle;
                    }
                    RxEvent::None
                }
            }

            RxState::Payload {
                field,
                ref mut remaining,
            } => {
                match field {
                    2 => {
                        // Program bytecode → buffer
                        if (self.program_len as usize) < MAX_PROGRAM_SIZE {
                            self.program_buf[self.program_len as usize] = byte;
                            self.program_len += 1;
                        }
                    }
                    4 => {
                        // FS image → stream to flash
                        self.fs_writer.write_byte(byte, flash);
                    }
                    _ => {}
                }

                *remaining -= 1;
                if *remaining == 0 {
                    self.state = RxState::Idle;
                    match field {
                        2 => RxEvent::ProgramReady,
                        4 => {
                            self.fs_writer.flush(flash);
                            RxEvent::FsWriteComplete
                        }
                        _ => RxEvent::None,
                    }
                } else {
                    RxEvent::None
                }
            }
        }
    }
}

// --- TX messages ---

/// Send error: Field 1, Payload type — tag + length(1) + error_code
pub fn send_error(usart: &Usart, code: u8) {
    // tag = (1 << 3) | 2 = 0x0A, length = 1, payload = code
    usart.write(&[0x0A, 0x01, code]);
}

/// Send pong: Field 2, Varint type — tag byte only
pub fn send_pong(usart: &Usart) {
    // tag = (2 << 3) | 0 = 0x10
    usart.write(&[0x10]);
}
