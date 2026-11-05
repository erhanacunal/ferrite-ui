/// USART protocol — protobuf-style tag encoding
///
/// Encoding:
///   Varint type:  1 byte  — tag only (wire_type + field_no)
///   Payload type: N bytes — tag + length(varint) + payload
///
/// Device sends (TX):
///   Field 1, Payload = Error (payload: error code byte)
///   Field 2, Varint  = Pong (also used as ACK for WriteFs chunks)
///   Field 6, Payload = MemDetail (per-pool memory usage, see `send_memdetail`)
///   Field 7, Payload = FsCaps (1B flags, bit 0 = CRC chunks): ACK of a
///                      WriteFs header that asked for CRC chunks
///
/// Device receives (RX):
///   Field 1, Varint  = Ping
///   Field 2, Payload = Execute (payload: program bytecode)
///   Field 3, Varint  = Restart
///   Field 4, Payload = WriteFs header (8B: total_size u32 LE + chunk_size u32 LE,
///                      optionally + flags u32 LE + CRC-32 of the first 12 bytes)
///   Field 5, Payload = WriteChunk (chunk data bytes; in CRC mode
///                      seq u32 LE + CRC-32(seq + data) u32 LE + data)
///
/// CRC chunks (writefs over a noisy line): a 16-byte header with flags bit 0
/// set asks for them. Firmware that knows them answers FsCaps instead of a
/// pong; older firmware reads only the first 8 bytes, answers a pong, and the
/// host falls back to plain chunks. In CRC mode a chunk is kept in RAM until
/// its CRC checks out, then written; a bad header or chunk gets error
/// `ERR_TRANSFER_CRC` (10), nothing is written and the host resends it. `seq`
/// counts chunks from 0: a resend of the chunk just written (its pong was
/// lost) is acknowledged again without writing it twice. A chunk cut short
/// (bytes lost) is dropped by `resync`, and the host resends it after the
/// resync pause.
///   Field 6, Payload = UserMessage (arbitrary user data, max 64 bytes)
///   Field 7, Varint  = MemInfo (query free heap memory)
///   Field 8, Varint  = TouchCalibrate (start 3-point touch calibration)
///   Field 9, Varint  = StackInfo (query current stack usage)
///   Field 10, Payload = SetDateTime (7B: year(0-99) + month + day + weekday + hour + minute + second)
///   Field 11, Varint  = MemDetail (query per-pool memory usage)
extern crate alloc;

use crate::flash::{FlashBackend, FlashImpl};
use crate::usart::{UsartBackend, UsartImpl};
use alloc::vec::Vec;

use crate::fs::crc32_update;

/// Error code for a WriteFs header or chunk that failed its CRC (resend it).
pub const ERR_TRANSFER_CRC: u8 = 10;

/// zlib-compatible CRC-32.
fn crc32(data: &[u8]) -> u32 {
    !crc32_update(!0, data)
}

/// Maximum program bytecode size received via USART (2KB)
pub const MAX_PROGRAM_SIZE: usize = 2048;

/// Maximum user message size (bytes)
const MAX_USER_MSG: usize = 64;

/// Sector buffer size for flash writes (4KB = 1 erase sector)
const FS_SECTOR_SIZE: usize = 4096;

/// A message that stalls this long mid-frame (no byte arrived, RX ring empty)
/// is abandoned: the parser goes back to `Idle`. The protocol has no frame
/// markers, so without this a stray byte (noise on a floating RX line, an
/// aborted host transfer) that looks like a payload tag would leave the
/// parser swallowing every later byte as payload until reset.
pub const RX_RESYNC_MS: u32 = 500;

/// A WriteFs transfer stays active (see [`Protocol::in_transfer`]) until the
/// image is complete or the line has been quiet this long. Longer than the
/// host's ACK timeout + resync pause (5 s + 0.6 s in `ferrite_cli`), so a
/// resend after a lost chunk is still inside the transfer.
pub const TRANSFER_IDLE_MS: u32 = 10_000;

// --- RX State Machine ---

#[derive(Clone, Copy)]
enum RxState {
    /// Waiting for a tag byte
    Idle,
    /// Reading payload length varint (for Payload-type messages)
    Length { field: u8, value: u32, shift: u8 },
    /// Reading payload bytes
    Payload { field: u8, remaining: u32 },
}

// --- FS Writer (chunked flash write with heap-allocated sector buffer) ---

struct FsWriter {
    /// Base flash address the FS is written to (board-specific — see
    /// [`crate::platform::Platform::FS_BASE`]). `writefs` targets this.
    fs_base: u32,
    /// Current flash write address
    addr: u32,
    /// Sector buffer — heap-allocated on demand (4KB, freed on device restart)
    buf: Vec<u8>,
    /// Total bytes expected (from header)
    total_size: u32,
    /// Total bytes received so far
    received: u32,
    /// Header collection buffer (8 bytes: total_size + chunk_size, or 16 with
    /// flags + header CRC)
    header_buf: [u8; FS_HEADER_MAX],
    /// Bytes collected into header_buf
    header_pos: u8,
    /// CRC chunks negotiated by the last header (see the module docs).
    crc_mode: bool,
    /// CRC mode with sequence ACKs: every chunk is answered with an FsAck
    /// that says how far the device is, instead of a pong / error 10.
    seq_ack: bool,
    /// Next chunk sequence number expected (CRC mode).
    next_seq: u32,
    /// CRC of the last chunk taken: a resend counts as its duplicate only
    /// with the same CRC (the same bytes), not just the same seq.
    last_crc: u32,
    /// Chunk in progress (CRC mode): its 8 prefix bytes (seq + CRC), how many
    /// of them arrived, and where its data starts in `buf` / `received`.
    chunk_prefix: [u8; 8],
    chunk_prefix_pos: u8,
    chunk_start: usize,
    chunk_rx_start: u32,
    in_chunk: bool,
    /// The chunk ran past `MAX_CHUNK` bytes (a corrupted length): it is bad.
    chunk_overflow: bool,
    /// A header was taken and the image isn't complete yet.
    active: bool,
}

/// WriteFs header: 8 bytes (plain) or 16 (flags + CRC).
const FS_HEADER_MAX: usize = 16;
/// Header flags bit 0: CRC chunks requested.
pub const FS_FLAG_CRC: u32 = 1;
/// Header flags bit 1 (with bit 0): answer chunks with FsAck
/// ([`send_fs_ack`]). Pongs and errors don't say which chunk they answer, so
/// a host whose replies fell one behind (extra replies to a damaged chunk)
/// takes a taken chunk for a failed one, or the reverse.
pub const FS_FLAG_SEQ_ACK: u32 = 2;
/// Largest chunk kept in RAM in CRC mode; longer = corrupted length.
const MAX_CHUNK: usize = 64 * 1024;

/// What a finished WriteFs header or chunk turned out to be.
#[derive(Clone, Copy, PartialEq, Debug)]
enum ChunkResult {
    /// Header / chunk taken (or a duplicate of the chunk just written).
    Ok,
    /// CRC mismatch, bad length, unexpected seq: nothing written.
    Bad,
}

impl FsWriter {
    fn new(fs_base: u32) -> Self {
        Self {
            fs_base,
            addr: fs_base,
            buf: Vec::new(),
            total_size: 0,
            received: 0,
            header_buf: [0; FS_HEADER_MAX],
            header_pos: 0,
            crc_mode: false,
            seq_ack: false,
            next_seq: 0,
            last_crc: 0,
            chunk_prefix: [0; 8],
            chunk_prefix_pos: 0,
            chunk_start: 0,
            chunk_rx_start: 0,
            in_chunk: false,
            chunk_overflow: false,
            active: false,
        }
    }

    /// Reset state for a new WriteFs header.
    fn reset_header(&mut self) {
        self.header_pos = 0;
    }

    /// Feed a header byte. Bytes past the 16 known ones are ignored.
    fn feed_header(&mut self, byte: u8) {
        if (self.header_pos as usize) < FS_HEADER_MAX {
            self.header_buf[self.header_pos as usize] = byte;
            self.header_pos += 1;
        }
    }

    /// Parse the collected header and prepare for chunk reception.
    /// Buffer allocation is deferred — happens in FsReady handler after
    /// the running program is freed to reclaim heap.
    ///
    /// A 16-byte header must carry a matching CRC of its first 12 bytes, and
    /// one with the CRC flag must be 16 bytes long; otherwise it is `Bad`
    /// and the previous state is kept. The CRC is checked whatever the flags
    /// say: a damaged flags word would otherwise turn the transfer into a
    /// plain one, with no CRC on the chunks. 8-byte headers (older hosts)
    /// carry no CRC.
    fn parse_header(&mut self) -> ChunkResult {
        let b = &self.header_buf;
        let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let len = self.header_pos as usize;
        let flags = if len >= 12 { word(8) } else { 0 };
        let crc_mode = flags & FS_FLAG_CRC != 0;
        if (crc_mode && len < 16) || (len >= 16 && crc32(&b[..12]) != word(12)) {
            return ChunkResult::Bad;
        }
        // Inside a CRC transfer only a CRC header may restart it: a stray
        // field-4 tag in damaged chunk data would otherwise reset the write
        // position and switch to plain chunks.
        if self.active && self.crc_mode && !crc_mode {
            return ChunkResult::Bad;
        }
        self.active = true;
        self.total_size = word(0);
        self.crc_mode = crc_mode;
        self.seq_ack = crc_mode && flags & FS_FLAG_SEQ_ACK != 0;
        self.next_seq = 0;
        self.in_chunk = false;
        self.addr = self.fs_base;
        self.received = 0;
        self.buf.clear();
        ChunkResult::Ok
    }

    /// A WriteChunk payload begins.
    fn begin_chunk(&mut self) {
        self.chunk_prefix_pos = 0;
        self.chunk_start = self.buf.len();
        self.chunk_rx_start = self.received;
        self.chunk_overflow = false;
        self.in_chunk = true;
    }

    /// One WriteChunk payload byte.
    fn chunk_byte<B: FlashBackend>(&mut self, byte: u8, flash: &FlashImpl<B>) {
        if !self.crc_mode {
            self.write_byte(byte, flash);
            return;
        }
        if (self.chunk_prefix_pos as usize) < self.chunk_prefix.len() {
            self.chunk_prefix[self.chunk_prefix_pos as usize] = byte;
            self.chunk_prefix_pos += 1;
        } else if self.buf.len() - self.chunk_start < MAX_CHUNK {
            // Kept in RAM until the CRC is checked: no sector flush here.
            self.buf.push(byte);
            self.received += 1;
        } else {
            self.chunk_overflow = true;
        }
    }

    /// The WriteChunk payload is complete. CRC mode: check it, then write its
    /// whole sectors (and the tail once the image is complete), or drop it.
    fn end_chunk<B: FlashBackend>(&mut self, flash: &FlashImpl<B>) -> ChunkResult {
        if !self.crc_mode {
            return ChunkResult::Ok;
        }
        self.in_chunk = false;
        let p = &self.chunk_prefix;
        let seq = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
        let want = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
        let data = &self.buf[self.chunk_start..];
        let ok = self.chunk_prefix_pos == 8
            && !self.chunk_overflow
            && self.received <= self.total_size
            && !crc32_update(crc32_update(!0, &p[..4]), data) == want;
        if !ok || seq != self.next_seq {
            self.rollback_chunk();
            // The chunk just written, sent again because its pong was lost.
            // Same bytes only: a host that lost track (stale replies) may
            // resend that seq shorter; ACKing it would shift the image.
            let dup = ok && seq.wrapping_add(1) == self.next_seq && want == self.last_crc;
            return if dup { ChunkResult::Ok } else { ChunkResult::Bad };
        }
        self.next_seq += 1;
        self.last_crc = want;
        while self.buf.len() >= FS_SECTOR_SIZE {
            self.flush_sector(flash);
        }
        ChunkResult::Ok
    }

    /// Drop the bytes of the chunk in progress (CRC mode).
    fn rollback_chunk(&mut self) {
        self.buf.truncate(self.chunk_start);
        self.received = self.chunk_rx_start;
        self.in_chunk = false;
    }

    /// A chunk cut short by `resync` (bytes lost on the line) is dropped, so
    /// the host can resend it. Plain mode has no way back.
    fn abort_chunk(&mut self) {
        if self.crc_mode && self.in_chunk {
            self.rollback_chunk();
        }
    }

    /// Pre-allocate sector buffer. Call after freeing program memory.
    pub fn alloc_sector_buf(&mut self) {
        self.buf.reserve(FS_SECTOR_SIZE);
    }

    /// Write a single data byte into the sector buffer.
    /// Flushes a full 4KB sector to flash when the buffer is full.
    fn write_byte<B: FlashBackend>(&mut self, byte: u8, flash: &FlashImpl<B>) {
        self.buf.push(byte);
        self.received += 1;

        if self.buf.len() == FS_SECTOR_SIZE {
            self.flush_sector(flash);
        }
    }

    /// Erase sector and write the full 4KB buffer to flash.
    fn flush_sector<B: FlashBackend>(&mut self, flash: &FlashImpl<B>) {
        flash.erase_sector(self.addr);
        let mut offset: usize = 0;
        while offset < FS_SECTOR_SIZE {
            flash.write(self.addr + offset as u32, &self.buf[offset..offset + 256]);
            offset += 256;
        }
        self.addr += FS_SECTOR_SIZE as u32;
        // CRC mode can hold more than a sector (a chunk not aligned to one).
        self.buf.drain(..FS_SECTOR_SIZE);
    }

    /// Flush any remaining bytes in the buffer (partial sector at end).
    fn flush<B: FlashBackend>(&mut self, flash: &FlashImpl<B>) {
        let len = self.buf.len();
        if len == 0 {
            return;
        }

        flash.erase_sector(self.addr);
        let mut offset: usize = 0;
        while offset < len {
            let page_len = (len - offset).min(256);
            flash.write(
                self.addr + offset as u32,
                &self.buf[offset..offset + page_len],
            );
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

#[derive(Clone, Copy, PartialEq, Debug)]
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
    /// CRC mode: a header or chunk failed its CRC and was dropped (send
    /// error `ERR_TRANSFER_CRC`; the host resends it)
    FsTransferBad,
    /// User message received (arbitrary data for VM callback)
    UserMessage,
    /// Memory info request — respond with free heap bytes
    MemInfo,
    /// Start 3-point touch calibration sequence
    TouchCalibrate,
    /// Stack usage query — respond with current stack used/free
    StackInfo,
    /// Set RTC date and time — respond with pong
    SetDateTime,
    /// Program too large — exceeds MAX_PROGRAM_SIZE (2KB)
    ProgramTooLarge,
    /// Per-pool memory usage query — respond with `send_memdetail`
    MemDetail,
}

// --- Protocol ---

pub struct Protocol {
    state: RxState,
    /// Heap-allocated program buffer — allocated on demand when Execute message arrives.
    /// Capacity=0 means the program was too large (being skipped).
    program_buf: Vec<u8>,
    fs_writer: FsWriter,
    user_msg_buf: [u8; MAX_USER_MSG],
    user_msg_len: u8,
    datetime_buf: [u8; 7],
    datetime_pos: u8,
}

impl Protocol {
    /// `fs_base` is the flash address `writefs` writes the resource image to
    /// (see [`crate::platform::Platform::FS_BASE`]).
    pub fn new(fs_base: u32) -> Self {
        Self {
            state: RxState::Idle,
            program_buf: Vec::new(),
            fs_writer: FsWriter::new(fs_base),
            user_msg_buf: [0; MAX_USER_MSG],
            user_msg_len: 0,
            datetime_buf: [0; 7],
            datetime_pos: 0,
        }
    }

    /// True between messages (waiting for a tag byte).
    pub fn is_idle(&self) -> bool {
        matches!(self.state, RxState::Idle)
    }

    /// Abandon a half-received message and wait for the next tag byte.
    /// A partial program buffer is freed. A partial WriteFs transfer stays
    /// abandoned: the host restarts it with a new header.
    pub fn resync(&mut self) {
        self.state = RxState::Idle;
        self.program_buf = Vec::new();
        // CRC mode: a chunk cut short is dropped, the host resends it.
        self.fs_writer.abort_chunk();
    }

    /// Inside a WriteFs transfer: a header was taken and the image isn't
    /// complete. Between messages the parser then starts only a header or a
    /// chunk and skips every other byte: chunk data read as messages (a
    /// damaged chunk tag or length) must not run commands, and image bytes
    /// are full of Restart tags (0x18). Not even ping: every 0x08 in the
    /// data would send a pong, which the host takes for a chunk ACK.
    pub fn in_transfer(&self) -> bool {
        self.fs_writer.active
    }

    /// Leave an abandoned transfer (the runtime calls this after
    /// [`TRANSFER_IDLE_MS`] of silence), so other commands work again.
    pub fn end_transfer(&mut self) {
        self.fs_writer.active = false;
    }

    /// The last WriteFs header negotiated CRC chunks: ACK it with
    /// [`send_fs_caps`] instead of a pong.
    pub fn fs_crc_mode(&self) -> bool {
        self.fs_writer.crc_mode
    }

    /// The last WriteFs header asked for sequence ACKs: answer chunks with
    /// [`send_fs_ack`].
    pub fn fs_seq_ack(&self) -> bool {
        self.fs_writer.seq_ack
    }

    /// Flags byte of the FsCaps reply to the last header.
    pub fn fs_caps(&self) -> u8 {
        let mut f = 0;
        if self.fs_writer.crc_mode {
            f |= FS_FLAG_CRC;
        }
        if self.fs_writer.seq_ack {
            f |= FS_FLAG_SEQ_ACK;
        }
        f as u8
    }

    /// FsAck contents: the next chunk seq expected, and the CRC of the last
    /// chunk taken (0 before the first).
    pub fn fs_ack_state(&self) -> (u32, u32) {
        (self.fs_writer.next_seq, self.fs_writer.last_crc)
    }

    /// A WriteFs header (field 4) payload is complete.
    fn fs_header_done(&mut self) -> RxEvent {
        match self.fs_writer.parse_header() {
            ChunkResult::Ok => RxEvent::FsReady,
            ChunkResult::Bad => RxEvent::FsTransferBad,
        }
    }

    /// A WriteChunk (field 5) payload is complete.
    fn fs_chunk_done<B: FlashBackend>(&mut self, flash: &FlashImpl<B>) -> RxEvent {
        if self.fs_writer.end_chunk(flash) == ChunkResult::Bad {
            return RxEvent::FsTransferBad;
        }
        if self.fs_writer.is_complete() {
            self.fs_writer.flush(flash);
            self.fs_writer.active = false;
            RxEvent::FsWriteComplete
        } else {
            RxEvent::FsChunkDone
        }
    }

    /// Hand over the received program (valid after ProgramReady event),
    /// leaving the buffer empty. The VM keeps it, so loading makes no copy.
    pub fn take_program(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.program_buf)
    }

    /// Pre-allocate 4KB sector buffer for flash writes.
    /// Call after freeing program memory to ensure heap space.
    pub fn alloc_sector_buf(&mut self) {
        self.fs_writer.alloc_sector_buf();
    }

    /// Return user message data (valid after UserMessage event).
    pub fn user_message(&self) -> &[u8] {
        &self.user_msg_buf[..self.user_msg_len as usize]
    }

    /// Return parsed DateTime (valid after SetDateTime event).
    pub fn datetime(&self) -> crate::rtc::DateTime {
        crate::rtc::DateTime {
            year: self.datetime_buf[0],
            month: self.datetime_buf[1],
            day: self.datetime_buf[2],
            weekday: self.datetime_buf[3],
            hour: self.datetime_buf[4],
            minute: self.datetime_buf[5],
            second: self.datetime_buf[6],
        }
    }

    /// Process a single received byte. Returns an event if a complete message was received.
    #[inline(never)]
    pub fn feed<B: FlashBackend>(&mut self, byte: u8, flash: &FlashImpl<B>) -> RxEvent {
        match self.state {
            RxState::Idle => {
                // Mid-transfer: header (0x22) and chunk (0x2A) only.
                if self.fs_writer.active && !matches!(byte, 0x22 | 0x2A) {
                    return RxEvent::None;
                }
                // Tag byte: (field << 3) | wire_type
                let wt = byte & 0x07;
                let field = byte >> 3;

                match wt {
                    // Varint type: tag byte only, no additional data
                    0 => match field {
                        1 => RxEvent::Ping,
                        3 => RxEvent::Restart,
                        7 => RxEvent::MemInfo,
                        8 => RxEvent::TouchCalibrate,
                        9 => RxEvent::StackInfo,
                        11 => RxEvent::MemDetail,
                        _ => RxEvent::None,
                    },
                    // Payload type: tag + length varint + payload
                    2 => {
                        match field {
                            2 => {
                                // Execute — read length, then bytecode
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
                            10 => {
                                // SetDateTime — read length, then 7 bytes
                                self.datetime_pos = 0;
                                self.state = RxState::Length {
                                    field: 10,
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

                    // For Execute (field 2): allocate buffer or mark skip
                    if f == 5 {
                        self.fs_writer.begin_chunk();
                    }
                    if f == 2 {
                        if len as usize > MAX_PROGRAM_SIZE {
                            self.program_buf = Vec::new(); // capacity=0 → skip mode
                        } else {
                            self.program_buf = Vec::with_capacity(len as usize);
                        }
                    }

                    if len == 0 {
                        // Zero-length payload — message complete immediately
                        match f {
                            2 => {
                                if self.program_buf.capacity() == 0 {
                                    RxEvent::ProgramTooLarge
                                } else {
                                    RxEvent::ProgramReady
                                }
                            }
                            4 => self.fs_header_done(),
                            6 => RxEvent::UserMessage,
                            5 => self.fs_chunk_done(flash),
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
                        // Program bytecode → heap buffer (skip if capacity=0)
                        if self.program_buf.capacity() > 0 {
                            self.program_buf.push(byte);
                        }
                    }
                    4 => {
                        // WriteFs header bytes → collect
                        self.fs_writer.feed_header(byte);
                    }
                    5 => {
                        // WriteChunk data → sector buffer (CRC mode: held
                        // until the chunk's CRC is checked)
                        self.fs_writer.chunk_byte(byte, flash);
                    }
                    6 => {
                        // User message data → buffer
                        if (self.user_msg_len as usize) < MAX_USER_MSG {
                            self.user_msg_buf[self.user_msg_len as usize] = byte;
                            self.user_msg_len += 1;
                        }
                    }
                    10 => {
                        // DateTime bytes: year, month, day, weekday, hour, minute, second
                        if (self.datetime_pos as usize) < 7 {
                            self.datetime_buf[self.datetime_pos as usize] = byte;
                            self.datetime_pos += 1;
                        }
                    }
                    _ => {}
                }

                *remaining -= 1;
                if *remaining == 0 {
                    self.state = RxState::Idle;
                    match field {
                        2 => {
                            if self.program_buf.capacity() == 0 {
                                RxEvent::ProgramTooLarge
                            } else {
                                RxEvent::ProgramReady
                            }
                        }
                        6 => RxEvent::UserMessage,
                        // Header complete — parse and prepare for chunks
                        4 => self.fs_header_done(),
                        // Chunk complete — check it, then whether all data arrived
                        5 => self.fs_chunk_done(flash),
                        10 => RxEvent::SetDateTime,
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
pub fn send_error<U: UsartBackend>(usart: &UsartImpl<U>, code: u8) {
    // tag = (1 << 3) | 2 = 0x0A, length = 1, payload = code
    usart.write(&[0x0A, 0x01, code]);
}

/// Send VM error: Field 1, Payload type — tag + length + error_code +
/// vm fault code (`vm::fault::*`) + faulting PC: u16 LE (length 4) while it
/// fits, else u32 LE (length 6, wide images). Hosts that only read
/// `payload[0]` keep working.
pub fn send_vm_error<U: UsartBackend>(usart: &UsartImpl<U>, code: u8, fault: u8, pc: u32) {
    let [b0, b1, b2, b3] = pc.to_le_bytes();
    if pc > 0xFFFF {
        usart.write(&[0x0A, 0x06, code, fault, b0, b1, b2, b3]);
    } else {
        usart.write(&[0x0A, 0x04, code, fault, b0, b1]);
    }
}

/// Send pong: Field 2, Varint type — tag byte only
/// ACK of a WriteFs header that negotiated CRC chunks: field 7 payload, one
/// flags byte (bit 0 = CRC chunks).
pub fn send_fs_caps<U: UsartBackend>(usart: &UsartImpl<U>, flags: u8) {
    // tag = (7 << 3) | 2 = 0x3A, length = 1
    usart.write(&[0x3A, 0x01, flags]);
}

/// FsAck (field 8, payload): the answer to every chunk in sequence-ACK mode,
/// taken or not. 12 bytes LE: `next_seq` u32 (the chunk the device expects
/// next) + `last_crc` u32 (CRC field of the last chunk taken) + CRC-32 of
/// those 8 bytes. The host reads it by content: `next_seq` = its seq + 1
/// with the CRC of a version it sent = taken; `next_seq` = its seq = resend;
/// older = a stale reply, skipped.
pub fn send_fs_ack<U: UsartBackend>(usart: &UsartImpl<U>, next_seq: u32, last_crc: u32) {
    let mut m = [0u8; 14];
    m[0] = 0x42; // tag = (8 << 3) | 2
    m[1] = 12;
    m[2..6].copy_from_slice(&next_seq.to_le_bytes());
    m[6..10].copy_from_slice(&last_crc.to_le_bytes());
    let c = crc32(&m[2..10]);
    m[10..14].copy_from_slice(&c.to_le_bytes());
    usart.write(&m);
}

pub fn send_pong<U: UsartBackend>(usart: &UsartImpl<U>) {
    // tag = (2 << 3) | 0 = 0x10
    usart.write(&[0x10]);
}

/// Send memory info: Field 3, Varint type — tag + varint(free_bytes)
pub fn send_meminfo<U: UsartBackend>(usart: &UsartImpl<U>, free_bytes: u32) {
    // tag = (3 << 3) | 0 = 0x18
    usart.write_byte(0x18);
    // Encode free_bytes as varint
    let mut val = free_bytes;
    loop {
        let mut b = (val & 0x7F) as u8;
        val >>= 7;
        if val != 0 {
            b |= 0x80;
        }
        usart.write_byte(b);
        if val == 0 {
            break;
        }
    }
}

/// MemDetail pool ids (`MemEntry::id`). Mirrored in `tools/ferrite_cli.py`
/// (`MEM_POOLS`) — keep in sync.
pub mod mem_pool {
    /// used / total heap bytes; extra = largest free block.
    pub const HEAP: u8 = 1;
    /// used / (used + free) CPU stack bytes; extra = peak (0 = not measured).
    pub const CPU_STACK: u8 = 2;
    /// Object pool arena bytes below the bump pointer / `arena_bytes`;
    /// extra = bytes in freed blocks that compaction would reclaim.
    pub const OBJ_ARENA: u8 = 3;
    /// Live objects (all types) / `max_objects`; extra = pool heap bytes.
    pub const OBJ_PTRS: u8 = 4;
    /// UART RX bytes waiting / ring size (0 / 0 = backend doesn't report).
    pub const UART_RX: u8 = 5;
    /// Live widgets / `max_objects`; extra = their arena bytes.
    pub const WIDGETS: u8 = 6;
    /// Live strings / `max_objects`; extra = their arena bytes.
    pub const STRINGS: u8 = 7;
    /// Eval stack depth / `eval_stack`; extra = heap bytes.
    pub const EVAL_STACK: u8 = 8;
    /// Call stack depth / `call_stack`; extra = heap bytes.
    pub const CALL_STACK: u8 = 9;
    /// Queued callbacks / `cb_queue`; extra = heap bytes.
    pub const CB_QUEUE: u8 = 10;
    /// Active tweens / animation slot cap; extra = heap bytes.
    pub const ANIMS: u8 = 11;
    /// Live arrays / `max_objects`; extra = their arena bytes.
    pub const ARRAYS: u8 = 12;
}

/// One MemDetail row: pool `id` (`mem_pool`), `used` and `capacity` in the
/// pool's unit, plus a pool-specific `extra` (see `mem_pool`).
#[derive(Clone, Copy)]
pub struct MemEntry {
    pub id: u8,
    pub used: u32,
    pub capacity: u32,
    pub extra: u32,
}

/// MemDetail payload version.
const MEMDETAIL_VERSION: u8 = 1;

/// Send per-pool memory usage: Field 6, Payload type — tag + length varint +
/// version(u8) + count(u8) + count × [id u8, used u32, capacity u32, extra u32]
/// (all LE, 13 bytes each).
pub fn send_memdetail<U: UsartBackend>(usart: &UsartImpl<U>, entries: &[MemEntry]) {
    // tag = (6 << 3) | 2 = 0x32
    usart.write_byte(0x32);
    let mut len = 2 + entries.len() as u32 * 13;
    loop {
        let mut b = (len & 0x7F) as u8;
        len >>= 7;
        if len != 0 {
            b |= 0x80;
        }
        usart.write_byte(b);
        if len == 0 {
            break;
        }
    }
    usart.write_byte(MEMDETAIL_VERSION);
    usart.write_byte(entries.len() as u8);
    for e in entries {
        usart.write_byte(e.id);
        usart.write(&e.used.to_le_bytes());
        usart.write(&e.capacity.to_le_bytes());
        usart.write(&e.extra.to_le_bytes());
    }
}

/// Send stack info: Field 5, Payload type — tag + length(8) + used(u32 LE) + free(u32 LE)
/// Payload: used u32 + free u32 (current SP) + peak u32 (high-water mark, 0 =
/// not measured) = 12 bytes.
pub fn send_stackinfo<U: UsartBackend>(usart: &UsartImpl<U>, used: u32, free: u32, peak: u32) {
    // tag = (5 << 3) | 2 = 0x2A, length = 12
    usart.write_byte(0x2A);
    usart.write_byte(12);
    usart.write(&used.to_le_bytes());
    usart.write(&free.to_le_bytes());
    usart.write(&peak.to_le_bytes());
}

/// Send touch calibration result: Field 4, Payload type
/// Payload: flags(1) + x_min(2) + x_max(2) + y_min(2) + y_max(2) = 9 bytes
/// flags: bit0=xy_swap, bit1=x_flip, bit2=y_flip
pub fn send_touch_cal<U: UsartBackend>(usart: &UsartImpl<U>, cal: &crate::touch::CalParams) {
    // tag = (4 << 3) | 2 = 0x22, length = 9
    usart.write_byte(0x22);
    usart.write_byte(9);
    usart.write(&cal.to_bytes());
}

#[cfg(test)]
mod memdetail_tests {
    use super::*;
    use core::cell::RefCell;

    /// Records every byte written.
    struct Rec(RefCell<Vec<u8>>);
    impl UsartBackend for Rec {
        fn write_byte(&self, byte: u8) {
            self.0.borrow_mut().push(byte);
        }
        fn flush(&self) {}
    }

    #[test]
    fn memdetail_request_is_field_11_varint() {
        let mut p = Protocol::new(0x1000);
        let flash = crate::flash::FlashImpl::with_backend(crate::mock::MockFlash);
        assert!(matches!(p.feed(0x58, &flash), RxEvent::MemDetail));
    }

    #[test]
    fn memdetail_response_layout() {
        // Same bytes are decoded by tools/ferrite_cli.py (parse_memdetail).
        let usart = UsartImpl::with_backend(Rec(RefCell::new(Vec::new())));
        send_memdetail(&usart, &[
            MemEntry { id: mem_pool::HEAP, used: 100, capacity: 200, extra: 50 },
            MemEntry { id: mem_pool::OBJ_PTRS, used: 3, capacity: 47, extra: 0 },
        ]);
        let want: &[u8] = &[
            0x32, 28, 1, 2, //
            1, 100, 0, 0, 0, 200, 0, 0, 0, 50, 0, 0, 0, //
            4, 3, 0, 0, 0, 47, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(*usart.backend().0.borrow(), want);
    }

    #[test]
    fn memdetail_length_is_a_multibyte_varint_past_127() {
        let usart = UsartImpl::with_backend(Rec(RefCell::new(Vec::new())));
        let e = MemEntry { id: 1, used: 0, capacity: 0, extra: 0 };
        send_memdetail(&usart, &[e; 11]); // 2 + 11*13 = 145
        let out = usart.backend().0.borrow().clone();
        assert_eq!(&out[..3], &[0x32, 0x80 | (145 & 0x7F), 145 >> 7]);
        assert_eq!(out.len(), 3 + 145);
    }

    #[test]
    fn stray_payload_tag_swallows_commands_until_resync() {
        let mut p = Protocol::new(0x1000);
        let flash = crate::flash::FlashImpl::with_backend(crate::mock::MockFlash);
        // Noise: a UserMessage tag (0x32) + a long length (0xFF 0x7F) …
        for b in [0x32, 0xFF, 0x7F] {
            p.feed(b, &flash);
        }
        assert!(!p.is_idle());
        // … and a later ping is eaten as payload.
        assert!(matches!(p.feed(0x08, &flash), RxEvent::None));
        p.resync();
        assert!(p.is_idle());
        assert!(matches!(p.feed(0x08, &flash), RxEvent::Ping));
    }
}

#[cfg(test)]
mod writefs_crc_tests {
    use super::*;
    use core::cell::RefCell;

    const BASE: u32 = 0x1000;

    /// NOR-like RAM flash starting at 0.
    struct RamFlash(RefCell<Vec<u8>>);
    impl FlashBackend for RamFlash {
        fn read(&self, addr: u32, buf: &mut [u8]) {
            let m = self.0.borrow();
            buf.copy_from_slice(&m[addr as usize..addr as usize + buf.len()]);
        }
        fn write(&self, addr: u32, data: &[u8]) {
            let mut m = self.0.borrow_mut();
            for (d, &b) in m[addr as usize..].iter_mut().zip(data) {
                *d &= b;
            }
        }
        fn erase_sector(&self, addr: u32) {
            let a = addr as usize & !4095;
            self.0.borrow_mut()[a..a + 4096].fill(0xFF);
        }
        fn read_id(&self) -> (u8, u8, u8) {
            (0, 0, 0)
        }
    }

    fn flash() -> FlashImpl<RamFlash> {
        FlashImpl::with_backend(RamFlash(RefCell::new(vec_ff(64 * 1024))))
    }

    fn vec_ff(n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        v.resize(n, 0xFF);
        v
    }

    fn msg(tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut m = Vec::from([tag]);
        let mut len = payload.len();
        while len > 0x7F {
            m.push((len as u8 & 0x7F) | 0x80);
            len >>= 7;
        }
        m.push(len as u8);
        m.extend_from_slice(payload);
        m
    }

    fn header(total: u32, crc: bool) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(&total.to_le_bytes());
        h.extend_from_slice(&4096u32.to_le_bytes());
        if crc {
            h.extend_from_slice(&FS_FLAG_CRC.to_le_bytes());
            let c = crc32(&h);
            h.extend_from_slice(&c.to_le_bytes());
        }
        msg(0x22, &h)
    }

    fn chunk(seq: u32, data: &[u8]) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&seq.to_le_bytes());
        let c = !crc32_update(crc32_update(!0, &seq.to_le_bytes()), data);
        p.extend_from_slice(&c.to_le_bytes());
        p.extend_from_slice(data);
        msg(0x2A, &p)
    }

    /// Feeds `bytes`; returns the non-`None` events.
    fn feed(p: &mut Protocol, f: &FlashImpl<RamFlash>, bytes: &[u8]) -> Vec<RxEvent> {
        bytes.iter().map(|&b| p.feed(b, f)).filter(|e| *e != RxEvent::None).collect()
    }

    fn image(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    fn stored(f: &FlashImpl<RamFlash>, n: usize) -> Vec<u8> {
        let mut out = vec_ff(n);
        f.read(BASE, &mut out);
        out
    }

    #[test]
    fn crc_header_negotiates_and_chunks_land() {
        let (mut p, f, img) = (Protocol::new(BASE), flash(), image(4096 + 700));
        assert_eq!(feed(&mut p, &f, &header(img.len() as u32, true)), [RxEvent::FsReady]);
        assert!(p.fs_crc_mode());
        assert_eq!(feed(&mut p, &f, &chunk(0, &img[..4096])), [RxEvent::FsChunkDone]);
        assert_eq!(feed(&mut p, &f, &chunk(1, &img[4096..])), [RxEvent::FsWriteComplete]);
        assert_eq!(stored(&f, img.len()), img);
    }

    #[test]
    fn plain_header_keeps_the_old_protocol() {
        let (mut p, f, img) = (Protocol::new(BASE), flash(), image(5000));
        assert_eq!(feed(&mut p, &f, &header(img.len() as u32, false)), [RxEvent::FsReady]);
        assert!(!p.fs_crc_mode());
        assert_eq!(feed(&mut p, &f, &msg(0x2A, &img[..4096])), [RxEvent::FsChunkDone]);
        assert_eq!(feed(&mut p, &f, &msg(0x2A, &img[4096..])), [RxEvent::FsWriteComplete]);
        assert_eq!(stored(&f, img.len()), img);
    }

    #[test]
    fn corrupted_chunk_is_dropped_and_resend_succeeds() {
        let (mut p, f, img) = (Protocol::new(BASE), flash(), image(4096 + 300));
        feed(&mut p, &f, &header(img.len() as u32, true));
        let mut bad = chunk(0, &img[..4096]);
        bad[100] ^= 0x10; // one bit flipped on the line
        assert_eq!(feed(&mut p, &f, &bad), [RxEvent::FsTransferBad]);
        assert_eq!(feed(&mut p, &f, &chunk(0, &img[..4096])), [RxEvent::FsChunkDone]);
        assert_eq!(feed(&mut p, &f, &chunk(1, &img[4096..])), [RxEvent::FsWriteComplete]);
        assert_eq!(stored(&f, img.len()), img);
    }

    #[test]
    fn resent_chunk_after_lost_pong_is_not_written_twice() {
        let (mut p, f, img) = (Protocol::new(BASE), flash(), image(4096 * 2 + 10));
        feed(&mut p, &f, &header(img.len() as u32, true));
        assert_eq!(feed(&mut p, &f, &chunk(0, &img[..4096])), [RxEvent::FsChunkDone]);
        // The pong was lost: the host sends chunk 0 again.
        assert_eq!(feed(&mut p, &f, &chunk(0, &img[..4096])), [RxEvent::FsChunkDone]);
        assert_eq!(feed(&mut p, &f, &chunk(1, &img[4096..8192])), [RxEvent::FsChunkDone]);
        assert_eq!(feed(&mut p, &f, &chunk(2, &img[8192..])), [RxEvent::FsWriteComplete]);
        assert_eq!(stored(&f, img.len()), img);
    }

    #[test]
    fn chunk_cut_short_is_dropped_by_resync() {
        let (mut p, f, img) = (Protocol::new(BASE), flash(), image(3000));
        feed(&mut p, &f, &header(img.len() as u32, true));
        let good = chunk(0, &img);
        // Ten bytes lost: the payload never completes, the parser stalls.
        let mut short = good.clone();
        short.drain(500..510);
        assert_eq!(feed(&mut p, &f, &short), []);
        p.resync();
        assert_eq!(feed(&mut p, &f, &good), [RxEvent::FsWriteComplete]);
        assert_eq!(stored(&f, img.len()), img);
    }

    #[test]
    fn corrupted_crc_header_is_rejected() {
        let (mut p, f) = (Protocol::new(BASE), flash());
        let mut h = header(5000, true);
        h[3] ^= 0x01; // total_size damaged
        assert_eq!(feed(&mut p, &f, &h), [RxEvent::FsTransferBad]);
        assert_eq!(feed(&mut p, &f, &header(5000, true)), [RxEvent::FsReady]);
    }

    #[test]
    fn crc_flag_lost_on_the_line_does_not_downgrade_to_plain() {
        let (mut p, f) = (Protocol::new(BASE), flash());
        let mut h = header(5000, true);
        // tag + length, then total_size and chunk_size: flags start at 10.
        h[10] &= !0x01;
        assert_eq!(feed(&mut p, &f, &h), [RxEvent::FsTransferBad]);
        assert_eq!(feed(&mut p, &f, &header(5000, true)), [RxEvent::FsReady]);
        assert!(p.fs_crc_mode());
    }

    #[test]
    fn damaged_chunk_tag_does_not_run_its_data_as_commands() {
        let (mut p, f) = (Protocol::new(BASE), flash());
        // Image bytes full of Restart (0x18) and MemInfo (0x38) tags.
        let img: Vec<u8> = (0..4096 + 100).map(|i| [0x18, 0x00, 0x38, 0x41][i % 4]).collect();
        feed(&mut p, &f, &header(img.len() as u32, true));
        assert!(p.in_transfer());
        let mut bad = chunk(0, &img[..4096]);
        bad[0] = 0x28; // 0x2A with bit 1 lost: field 5 varint, then data
        assert_eq!(feed(&mut p, &f, &bad), []);
        p.resync();
        assert_eq!(feed(&mut p, &f, &chunk(0, &img[..4096])), [RxEvent::FsChunkDone]);
        // A stray ping tag in data must not answer: its pong would look
        // like a chunk ACK.
        assert_eq!(feed(&mut p, &f, &[0x08]), []);
        assert_eq!(feed(&mut p, &f, &chunk(1, &img[4096..])), [RxEvent::FsWriteComplete]);
        assert!(!p.in_transfer());
        assert_eq!(stored(&f, img.len()), img);
    }

    #[test]
    fn shorter_resend_of_the_last_seq_is_not_taken_as_its_duplicate() {
        let (mut p, f, img) = (Protocol::new(BASE), flash(), image(2048));
        feed(&mut p, &f, &header(img.len() as u32, true));
        assert_eq!(feed(&mut p, &f, &chunk(0, &img[..1024])), [RxEvent::FsChunkDone]);
        // A host one reply behind thinks seq 0 failed and halves it.
        assert_eq!(feed(&mut p, &f, &chunk(0, &img[..512])), [RxEvent::FsTransferBad]);
        // The real duplicate is still ACKed, and the transfer goes on.
        assert_eq!(feed(&mut p, &f, &chunk(0, &img[..1024])), [RxEvent::FsChunkDone]);
        assert_eq!(feed(&mut p, &f, &chunk(1, &img[1024..])), [RxEvent::FsWriteComplete]);
        assert_eq!(stored(&f, img.len()), img);
    }

    #[test]
    fn stray_plain_header_cannot_restart_a_crc_transfer() {
        let (mut p, f) = (Protocol::new(BASE), flash());
        feed(&mut p, &f, &header(9000, true));
        assert_eq!(feed(&mut p, &f, &msg(0x22, &[1, 2, 3, 4, 5, 6, 7, 8])), [RxEvent::FsTransferBad]);
        assert!(p.fs_crc_mode());
        // A real new CRC header still restarts it.
        assert_eq!(feed(&mut p, &f, &header(5000, true)), [RxEvent::FsReady]);
    }

    #[test]
    fn commands_work_again_after_an_abandoned_transfer() {
        let (mut p, f) = (Protocol::new(BASE), flash());
        feed(&mut p, &f, &header(9000, true));
        assert_eq!(feed(&mut p, &f, &[0x18]), []);
        p.end_transfer();
        assert_eq!(feed(&mut p, &f, &[0x18]), [RxEvent::Restart]);
    }

    #[test]
    fn caps_reply_bytes() {
        struct Rec(RefCell<Vec<u8>>);
        impl UsartBackend for Rec {
            fn write_byte(&self, b: u8) {
                self.0.borrow_mut().push(b);
            }
            fn flush(&self) {}
        }
        let u = UsartImpl::with_backend(Rec(RefCell::new(Vec::new())));
        send_fs_caps(&u, 0x03);
        // Decoded by tools/ferrite_cli.py (TAG_FSCAPS_RESP, TAG_FSACK_RESP).
        assert_eq!(&*u.backend().0.borrow(), &[0x3A, 0x01, 0x03]);
        u.backend().0.borrow_mut().clear();
        send_fs_ack(&u, 0x0102_0304, 0xAABB_CCDD);
        let b = u.backend().0.borrow();
        assert_eq!(&b[..10], &[0x42, 12, 4, 3, 2, 1, 0xDD, 0xCC, 0xBB, 0xAA]);
        assert_eq!(&b[10..], &crc32(&b[2..10]).to_le_bytes());
    }

    #[test]
    fn seq_ack_header_negotiates_and_ack_state_tracks_chunks() {
        let (mut p, f, img) = (Protocol::new(BASE), flash(), image(3000));
        let mut h = Vec::new();
        h.extend_from_slice(&(img.len() as u32).to_le_bytes());
        h.extend_from_slice(&4096u32.to_le_bytes());
        h.extend_from_slice(&(FS_FLAG_CRC | FS_FLAG_SEQ_ACK).to_le_bytes());
        let c = crc32(&h);
        h.extend_from_slice(&c.to_le_bytes());
        assert_eq!(feed(&mut p, &f, &msg(0x22, &h)), [RxEvent::FsReady]);
        assert!(p.fs_seq_ack());
        assert_eq!(p.fs_caps(), 0x03);
        assert_eq!(p.fs_ack_state(), (0, 0));
        let c0 = chunk(0, &img[..1000]);
        assert_eq!(feed(&mut p, &f, &c0), [RxEvent::FsChunkDone]);
        // tag, length (2 bytes: 1008 > 127), seq, then the CRC field.
        let crc0 = u32::from_le_bytes(c0[7..11].try_into().unwrap());
        assert_eq!(p.fs_ack_state(), (1, crc0));
        // Plain CRC headers keep pongs.
        assert_eq!(feed(&mut p, &f, &header(100, true)), [RxEvent::FsReady]);
        assert!(!p.fs_seq_ack());
        assert_eq!(p.fs_caps(), 0x01);
    }
}
