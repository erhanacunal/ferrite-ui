/// USART protocol — protobuf-style tag encoding
///
/// Encoding:
///   Varint type:  1 byte  — tag only (wire_type + field_no)
///   Payload type: N bytes — tag + length(varint) + payload
///
/// Device sends (TX):
///   Field 1, Payload = Error (payload: error code byte)
///   Field 2, Varint  = Pong (also used as ACK for WriteFs chunks)
///
/// Device receives (RX):
///   Field 1, Varint  = Ping
///   Field 2, Payload = Execute (payload: program bytecode)
///   Field 3, Varint  = Restart
///   Field 4, Payload = WriteFs header (8B: total_size u32 LE + chunk_size u32 LE)
///   Field 5, Payload = WriteChunk (chunk data bytes)
///   Field 6, Payload = UserMessage (arbitrary user data, max 64 bytes)

extern crate alloc;

use alloc::vec::Vec;
use crate::flash::Flash;
use crate::usart::Usart;

/// Maximum program bytecode size received via USART
const MAX_PROGRAM_SIZE: usize = 1024;

/// Maximum user message size (bytes)
const MAX_USER_MSG: usize = 64;

/// Sector buffer size for flash writes (4KB = 1 erase sector)
const FS_SECTOR_SIZE: usize = 4096;

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

// --- FS Writer (chunked flash write with heap-allocated sector buffer) ---

struct FsWriter {
    /// Current flash write address
    addr: u32,
    /// Sector buffer — heap-allocated on demand (4KB, freed on device restart)
    buf: Vec<u8>,
    /// Total bytes expected (from header)
    total_size: u32,
    /// Total bytes received so far
    received: u32,
    /// Header collection buffer (8 bytes: total_size + chunk_size)
    header_buf: [u8; 8],
    /// Bytes collected into header_buf
    header_pos: u8,
}

impl FsWriter {
    fn new() -> Self {
        Self {
            addr: 0,
            buf: Vec::new(),
            total_size: 0,
            received: 0,
            header_buf: [0; 8],
            header_pos: 0,
        }
    }

    /// Reset state for a new WriteFs header.
    fn reset_header(&mut self) {
        self.header_pos = 0;
    }

    /// Feed a header byte. Returns true when all 8 bytes are collected.
    fn feed_header(&mut self, byte: u8) -> bool {
        if (self.header_pos as usize) < 8 {
            self.header_buf[self.header_pos as usize] = byte;
            self.header_pos += 1;
        }
        self.header_pos >= 8
    }

    /// Parse the collected header and prepare for chunk reception.
    /// Allocates the 4KB sector buffer from heap.
    fn parse_header(&mut self) {
        let b = &self.header_buf;
        self.total_size =
            (b[0] as u32) | ((b[1] as u32) << 8) | ((b[2] as u32) << 16) | ((b[3] as u32) << 24);
        self.addr = 0;
        self.received = 0;
        // Allocate sector buffer on heap (4KB)
        self.buf.clear();
        self.buf.reserve(FS_SECTOR_SIZE);
    }

    /// Write a single data byte into the sector buffer.
    /// Flushes a full 4KB sector to flash when the buffer is full.
    fn write_byte(&mut self, byte: u8, flash: &Flash) {
        self.buf.push(byte);
        self.received += 1;

        if self.buf.len() == FS_SECTOR_SIZE {
            self.flush_sector(flash);
        }
    }

    /// Erase sector and write the full 4KB buffer to flash.
    fn flush_sector(&mut self, flash: &Flash) {
        flash.erase_sector(self.addr);
        let mut offset: usize = 0;
        while offset < FS_SECTOR_SIZE {
            flash.write(self.addr + offset as u32, &self.buf[offset..offset + 256]);
            offset += 256;
        }
        self.addr += FS_SECTOR_SIZE as u32;
        self.buf.clear();
    }

    /// Flush any remaining bytes in the buffer (partial sector at end).
    fn flush(&mut self, flash: &Flash) {
        let len = self.buf.len();
        if len == 0 {
            return;
        }

        flash.erase_sector(self.addr);
        let mut offset: usize = 0;
        while offset < len {
            let page_len = (len - offset).min(256);
            flash.write(self.addr + offset as u32, &self.buf[offset..offset + page_len]);
            offset += page_len;
        }
        self.buf.clear();
    }

    /// Returns true when all expected bytes have been received.
    fn is_complete(&self) -> bool {
        self.received >= self.total_size
    }
}

// --- Events ---

#[derive(Clone, Copy, PartialEq)]
pub enum RxEvent {
    None,
    Ping,
    Restart,
    ProgramReady,
    /// WriteFs header parsed — device is ready for chunks (send pong ACK)
    FsReady,
    /// Chunk received and written — ready for next chunk (send pong ACK)
    FsChunkDone,
    /// All chunks received, flash write complete (send pong ACK, then restart)
    FsWriteComplete,
    /// User message received (arbitrary data for VM callback)
    UserMessage,
}

// --- Protocol ---

pub struct Protocol {
    state: RxState,
    program_buf: [u8; MAX_PROGRAM_SIZE],
    program_len: u16,
    fs_writer: FsWriter,
    user_msg_buf: [u8; MAX_USER_MSG],
    user_msg_len: u8,
}

impl Protocol {
    pub fn new() -> Self {
        Self {
            state: RxState::Idle,
            program_buf: [0; MAX_PROGRAM_SIZE],
            program_len: 0,
            fs_writer: FsWriter::new(),
            user_msg_buf: [0; MAX_USER_MSG],
            user_msg_len: 0,
        }
    }

    /// Return program bytecode (valid after ProgramReady event).
    pub fn program_code(&self) -> &[u8] {
        &self.program_buf[..self.program_len as usize]
    }

    /// Return user message data (valid after UserMessage event).
    pub fn user_message(&self) -> &[u8] {
        &self.user_msg_buf[..self.user_msg_len as usize]
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
                                // WriteFs header — read length, then 8-byte header
                                self.fs_writer.reset_header();
                                self.state = RxState::Length {
                                    field: 4,
                                    value: 0,
                                    shift: 0,
                                };
                            }
                            5 => {
                                // WriteChunk — read length, then chunk data
                                self.state = RxState::Length {
                                    field: 5,
                                    value: 0,
                                    shift: 0,
                                };
                            }
                            6 => {
                                // UserMessage — read length, then data
                                self.user_msg_len = 0;
                                self.state = RxState::Length {
                                    field: 6,
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
                            4 => RxEvent::FsReady,
                            6 => RxEvent::UserMessage,
                            5 => {
                                if self.fs_writer.is_complete() {
                                    self.fs_writer.flush(flash);
                                    RxEvent::FsWriteComplete
                                } else {
                                    RxEvent::FsChunkDone
                                }
                            }
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
                        // WriteFs header bytes → collect
                        self.fs_writer.feed_header(byte);
                    }
                    5 => {
                        // WriteChunk data → sector buffer
                        self.fs_writer.write_byte(byte, flash);
                    }
                    6 => {
                        // User message data → buffer
                        if (self.user_msg_len as usize) < MAX_USER_MSG {
                            self.user_msg_buf[self.user_msg_len as usize] = byte;
                            self.user_msg_len += 1;
                        }
                    }
                    _ => {}
                }

                *remaining -= 1;
                if *remaining == 0 {
                    self.state = RxState::Idle;
                    match field {
                        2 => RxEvent::ProgramReady,
                        6 => RxEvent::UserMessage,
                        4 => {
                            // Header complete — parse and prepare for chunks
                            self.fs_writer.parse_header();
                            RxEvent::FsReady
                        }
                        5 => {
                            // Chunk complete — check if all data received
                            if self.fs_writer.is_complete() {
                                self.fs_writer.flush(flash);
                                RxEvent::FsWriteComplete
                            } else {
                                RxEvent::FsChunkDone
                            }
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
