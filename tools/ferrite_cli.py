#!/usr/bin/env python3
"""ferrite-cli — Host-side tool for communicating with ferrite-ui devices.

Implements the USART protobuf protocol (see protocol.rs).
Commands: ping, restart, meminfo, memdetail, stackinfo, execute <file>, writefs <file>, send <data...>, calibrate, settime, monitor

Usage:
    python ferrite_cli.py -p COM3 ping
    python ferrite_cli.py -p COM3 meminfo
    python ferrite_cli.py -p COM3 memdetail                (per-pool memory usage)
    python ferrite_cli.py -p COM3 restart
    python ferrite_cli.py -p COM3 execute program.bin
    python ferrite_cli.py -p COM3 -l execute program.fxe   (execute + listen)
    python ferrite_cli.py -p COM3 writefs flash.bin
    python ferrite_cli.py -p COM3 -l writefs flash.bin     (writefs, then watch the reboot)
    python ferrite_cli.py -p COM3 monitor                  (print what the device sends, Ctrl+C ends)
    python ferrite_cli.py -p COM3 monitor --hex            (raw bytes as hex, protocol replies too)
    python ferrite_cli.py -p COM3 send 0x01 0x02 0x03
    python ferrite_cli.py -p COM3 send "hello world"
    python ferrite_cli.py -p COM3 settime                  (use current system time)
    python ferrite_cli.py -p COM3 settime "2025-05-17 14:30:00"
"""

import argparse
import struct
import sys
import time
import zlib
import serial

# --- Protocol constants ---

# Host → Device tags: (field << 3) | wire_type
TAG_PING    = (1 << 3) | 0   # 0x08, varint
TAG_EXECUTE = (2 << 3) | 2   # 0x12, payload
TAG_RESTART = (3 << 3) | 0   # 0x18, varint
TAG_WRITEFS    = (4 << 3) | 2   # 0x22, payload (header: total_size + chunk_size)
TAG_WRITECHUNK = (5 << 3) | 2   # 0x2A, payload (chunk data)
TAG_USERMSG    = (6 << 3) | 2   # 0x32, payload (user message data, max 64 bytes)
TAG_MEMINFO    = (7 << 3) | 0   # 0x38, varint
TAG_TOUCHCAL   = (8 << 3) | 0   # 0x40, varint (start touch calibration)
TAG_STACKINFO  = (9 << 3) | 0   # 0x48, varint (query stack usage)
TAG_SETDATETIME = (10 << 3) | 2  # 0x52, payload (7B: year/month/day/weekday/hour/min/sec)
TAG_MEMDETAIL  = (11 << 3) | 0   # 0x58, varint (query per-pool memory usage)

CHUNK_SIZE = 4096  # Must match device sector buffer size

# Device → Host tags
TAG_ERROR    = (1 << 3) | 2   # 0x0A, payload
TAG_PONG     = (2 << 3) | 0   # 0x10, varint
TAG_MEMINFO_RESP = (3 << 3) | 0  # 0x18, varint (free heap bytes)
TAG_TOUCHCAL_RESP = (4 << 3) | 2  # 0x22, payload (calibration result, 9 bytes)
TAG_STACKINFO_RESP = (5 << 3) | 2  # 0x2A, payload (used + free [+ peak], 8 or 12 bytes)
TAG_MEMDETAIL_RESP = (6 << 3) | 2  # 0x32, payload (version + count + 13-byte entries)
TAG_FSCAPS_RESP = (7 << 3) | 2  # 0x3A, payload (1 byte flags): ACK of a CRC writefs header
TAG_FSACK_RESP = (8 << 3) | 2  # 0x42, payload (12 B): next_seq + last_crc + CRC-32 (seq-ACK writefs)

# WriteFs header flags (u32 after total_size + chunk_size) — FS_FLAG_* in protocol.rs
FS_FLAG_CRC = 0x01  # chunks carry seq + CRC-32 and are retried
FS_FLAG_SEQ_ACK = 0x02  # with CRC: every chunk answered by FsAck (next_seq + last_crc)
# Error code for a header/chunk that failed its CRC — ERR_TRANSFER_CRC in protocol.rs
ERR_TRANSFER_CRC = 10
# Attempts per chunk in CRC mode before writefs gives up.
CHUNK_TRIES = 8
# At MIN_CRC_CHUNK: attempts, and a pause between them. Corruption comes in
# bursts on some lines (exynos4412 UART2); the device has no retry limit.
MIN_CHUNK_TRIES = 32
MIN_CHUNK_PAUSE = 0.2
# Smallest chunk the CRC-mode retry halves down to.
MIN_CRC_CHUNK = 256
# Probes asking whether the device took an unanswered chunk.
PROBE_TRIES = 6

# MemDetail pool ids -> (name, unit, meaning of "extra"). Mirrors
# protocol::mem_pool in ferrite-core/src/protocol.rs — keep in sync.
#   unit "B" = bytes, "" = count. extra "heap" = heap bytes the pool holds.
MEM_POOLS = {
    1: ("heap", "B", "largest free"),
    2: ("cpu stack", "B", "peak"),
    3: ("object arena", "B", "reclaimable"),
    4: ("object ptrs", "", "heap"),
    5: ("uart rx", "B", "heap"),
    6: ("widgets", "", "in arena"),
    7: ("strings", "", "in arena"),
    8: ("eval stack", "", "heap"),
    9: ("call stack", "", "heap"),
    10: ("callback queue", "", "heap"),
    11: ("animations", "", "heap"),
    12: ("arrays", "", "in arena"),
}

BAUD_RATE = 115200
DEFAULT_TIMEOUT = 5.0  # seconds

ERROR_DESCRIPTIONS = {
    1: "page_main not found",
    2: "main program not found",
    3: "image not found",
    4: "font not found",
    5: "no file system in flash",
    6: "program execution error",
    7: "insufficient memory",
    8: "invalid system config",
    9: "flash image corrupted",
    10: "transfer CRC mismatch (resent automatically by writefs)",
}

# VM fault codes (error 6 detail) — keep in sync with `vm::fault` in vm.rs
VM_FAULT_NAMES = {
    1: "code out of bounds",
    2: "bad opcode",
    3: "stack overflow",
    4: "stack underflow",
    5: "call stack overflow",
    6: "ret without call",
    7: "pool full: widget",
    8: "pool full: string",
    9: "invalid array",
    10: "array index out of range",
    11: "invalid file handle",
    12: "syscall failed",
    13: "bad operand",
    14: "invalid gpio pin/mode",
    15: "bad widget page",
    16: "eeprom error",
    17: "pool full: array",
}


def encode_varint(value: int) -> bytes:
    """Encode an unsigned integer as a protobuf varint."""
    out = bytearray()
    while value > 0x7F:
        out.append((value & 0x7F) | 0x80)
        value >>= 7
    out.append(value & 0x7F)
    return bytes(out)


def decode_varint(data: bytes, offset: int = 0) -> tuple[int, int]:
    """Decode a varint from data at offset. Returns (value, bytes_consumed)."""
    value = 0
    shift = 0
    consumed = 0
    while offset < len(data):
        b = data[offset]
        value |= (b & 0x7F) << shift
        offset += 1
        consumed += 1
        if b & 0x80 == 0:
            break
        shift += 7
        if shift >= 35:
            raise ValueError("varint overflow")
    return value, consumed


def build_varint_msg(tag: int) -> bytes:
    """Build a varint-type message (tag byte only, no payload)."""
    return bytes([tag])


def build_payload_msg(tag: int, payload: bytes) -> bytes:
    """Build a payload-type message: tag + varint(length) + payload."""
    return bytes([tag]) + encode_varint(len(payload)) + payload


def open_port(port: str, timeout: float = DEFAULT_TIMEOUT) -> serial.Serial:
    """Open serial port with ferrite-ui settings (115200 8N1).

    DTR and RTS stay deasserted. pyserial raises both on open by default,
    and the ESP32-S3 USB-Serial-JTAG (epaper) reads those lines as the
    esptool reset/boot wiring: opening or closing the port then reset the
    chip. Boards on a plain UART ignore the lines.
    """
    ser = serial.Serial()
    ser.port = port
    ser.baudrate = BAUD_RATE
    ser.bytesize = serial.EIGHTBITS
    ser.parity = serial.PARITY_NONE
    ser.stopbits = serial.STOPBITS_ONE
    ser.timeout = timeout
    ser.dtr = False
    ser.rts = False
    ser.open()
    return ser


# Payload lengths a real reply can have. Boot logs and debug prints are
# text, and text contains the payload tags ('"' 0x22, '*' 0x2A, '2' 0x32,
# newline 0x0A): a tag whose length doesn't fit is noise, not a reply.
ERROR_LENGTHS = (1, 4, 6)            # code [+ fault + PC u16 | u32]
STACKINFO_LENGTHS = (8, 12)          # used + free [+ peak]
TOUCHCAL_LENGTH = 9
MEMDETAIL_VERSION = 1
MEMDETAIL_ENTRY = 13                 # id u8 + used/capacity/extra u32


def _memdetail_length_ok(length: int) -> bool:
    return length >= 2 and (length - 2) % MEMDETAIL_ENTRY == 0 and length <= 2 + 255 * MEMDETAIL_ENTRY


def drain_input(ser: serial.Serial, quiet: float = 0.2, limit: float = 3.0):
    """Drop what the device sent before a request (a boot log after a reset,
    debug prints), so it isn't read as the reply. Free when nothing is
    waiting; otherwise reads until the line has been quiet for `quiet` s."""
    if not ser.in_waiting:
        return
    deadline = time.time() + limit
    last = time.time()
    while time.time() < deadline:
        n = ser.in_waiting
        if n:
            ser.read(n)
            last = time.time()
        elif time.time() - last >= quiet:
            return
        else:
            time.sleep(0.01)


def _read_varint(ser: serial.Serial) -> int | None:
    """One varint, or None on timeout."""
    raw = bytearray()
    while True:
        b = ser.read(1)
        if not b:
            return None
        raw.append(b[0])
        if b[0] & 0x80 == 0:
            break
    return decode_varint(bytes(raw))[0]


def read_response(ser: serial.Serial) -> tuple[str, int | None]:
    """Read a single device response. Returns (type, value).

    Skips any non-protocol bytes (e.g. debug prints) until a valid tag is
    found; a payload tag with an impossible length is skipped too.

    Returns:
        ("pong", None) for pong
        ("error", code) for error
        ("timeout", None) if no response
    """
    orig_timeout = ser.timeout
    deadline = time.time() + orig_timeout
    while time.time() < deadline:
        remaining = deadline - time.time()
        if remaining <= 0:
            break
        ser.timeout = remaining
        tag_byte = ser.read(1)
        if not tag_byte:
            break
        # print(f"DEBUG: received byte 0x{tag_byte[0]:02X} ('{chr(tag_byte[0])}')", file=sys.stderr)

        tag = tag_byte[0]

        if tag == TAG_PONG:
            ser.timeout = orig_timeout
            return ("pong", None)

        if tag == TAG_MEMINFO_RESP:
            # Varint type — free heap bytes
            free_bytes = _read_varint(ser)
            ser.timeout = orig_timeout
            if free_bytes is None:
                return ("timeout", None)
            return ("meminfo", free_bytes)

        if tag == TAG_FSCAPS_RESP:
            # Only sent right after a writefs header; one flags byte.
            length = _read_varint(ser)
            if length != 1:
                continue
            payload = ser.read(1)
            ser.timeout = orig_timeout
            if not payload:
                return ("timeout", None)
            return ("fscaps", payload[0])

        if tag == TAG_FSACK_RESP:
            # Sequence-ACK writefs: next_seq u32 + last_crc u32 + CRC-32.
            length = _read_varint(ser)
            if length != 12:
                continue
            payload = ser.read(12)
            if len(payload) < 12:
                ser.timeout = orig_timeout
                return ("timeout", None)
            if zlib.crc32(payload[:8]) != int.from_bytes(payload[8:], "little"):
                continue
            ser.timeout = orig_timeout
            return ("fsack", (int.from_bytes(payload[0:4], "little"),
                              int.from_bytes(payload[4:8], "little")))

        if tag in (TAG_STACKINFO_RESP, TAG_MEMDETAIL_RESP, TAG_ERROR, TAG_TOUCHCAL_RESP):
            length = _read_varint(ser)
            if length is None:
                ser.timeout = orig_timeout
                return ("timeout", None)
            if not {
                TAG_STACKINFO_RESP: length in STACKINFO_LENGTHS,
                TAG_MEMDETAIL_RESP: _memdetail_length_ok(length),
                TAG_ERROR: length in ERROR_LENGTHS,
                TAG_TOUCHCAL_RESP: length == TOUCHCAL_LENGTH,
            }[tag]:
                continue  # text that happened to contain a tag byte
            payload = ser.read(length)
            if len(payload) < length:
                ser.timeout = orig_timeout
                return ("timeout", None)

            if tag == TAG_STACKINFO_RESP:
                # used u32 LE + free u32 LE [+ peak u32 LE: high-water mark,
                # 0 = not measured; older firmware omits it]
                ser.timeout = orig_timeout
                used = int.from_bytes(payload[0:4], "little")
                free = int.from_bytes(payload[4:8], "little")
                peak = int.from_bytes(payload[8:12], "little") if length >= 12 else 0
                return ("stackinfo", (used, free, peak))

            if tag == TAG_MEMDETAIL_RESP:
                if payload[0] != MEMDETAIL_VERSION or 2 + payload[1] * MEMDETAIL_ENTRY != length:
                    continue
                ser.timeout = orig_timeout
                return ("memdetail", parse_memdetail(payload))

            ser.timeout = orig_timeout
            if tag == TAG_ERROR:
                if len(payload) >= 4:
                    fault = payload[1]
                    # PC is u16 LE, or u32 LE (6-byte payload) past 64 KB.
                    pc = int.from_bytes(payload[2:6] if len(payload) >= 6 else payload[2:4],
                                        "little")
                    name = VM_FAULT_NAMES.get(fault, "unknown")
                    print(f"VM fault {fault} ({name}) at PC 0x{pc:04X}", flush=True)
                return ("error", payload[0])
            return ("touchcal", payload)

        # Unknown byte (debug output etc.) — skip and keep reading

    ser.timeout = orig_timeout
    return ("timeout", None)


# --- Commands ---


# The device drops a message that stalls mid-frame for 500 ms
# (protocol::RX_RESYNC_MS). After a timeout, waiting a bit longer than that
# lets a device whose parser was stuck on stray bytes recover before a resend.
RESYNC_WAIT = 0.6  # seconds


def resync(ser: serial.Serial):
    """Wait out the device's RX resync timeout and drop stale input."""
    time.sleep(RESYNC_WAIT)
    ser.reset_input_buffer()


def request(ser: serial.Serial, msg: bytes) -> tuple[str, int | None]:
    """Send `msg` and read the response. On a timeout, resync and resend once."""
    for attempt in range(2):
        drain_input(ser)
        ser.write(msg)
        ser.flush()
        resp = read_response(ser)
        if resp[0] != "timeout" or attempt:
            return resp
        print("no response, resyncing and retrying...", file=sys.stderr)
        resync(ser)
    return resp


def cmd_ping(ser: serial.Serial) -> bool:
    """Send ping and wait for pong."""
    resp_type, resp_val = request(ser, build_varint_msg(TAG_PING))
    if resp_type == "pong":
        print("pong")
        return True
    elif resp_type == "error":
        desc = ERROR_DESCRIPTIONS.get(resp_val, "unknown")
        print(f"error: code {resp_val} — {desc}", file=sys.stderr)
        return False
    else:
        print(f"no response (timeout)", file=sys.stderr)
        return False


def cmd_restart(ser: serial.Serial) -> bool:
    """Send restart command. Device will reset immediately (no response expected)."""
    ser.write(build_varint_msg(TAG_RESTART))
    ser.flush()
    print("restart sent")
    return True


def cmd_meminfo(ser: serial.Serial) -> bool:
    """Query free heap memory from device."""
    resp_type, resp_val = request(ser, build_varint_msg(TAG_MEMINFO))
    if resp_type == "meminfo":
        print(f"free: {resp_val} bytes ({resp_val / 1024:.1f} KB)")
        return True
    elif resp_type == "error":
        desc = ERROR_DESCRIPTIONS.get(resp_val, "unknown")
        print(f"error: code {resp_val} — {desc}", file=sys.stderr)
        return False
    else:
        print("no response (timeout)", file=sys.stderr)
        return False


def parse_memdetail(payload: bytes) -> list[tuple[int, int, int, int]]:
    """MemDetail payload -> [(pool id, used, capacity, extra), ...]."""
    if len(payload) < 2:
        return []
    count = payload[1]
    entries = []
    for i in range(count):
        off = 2 + i * 13
        if off + 13 > len(payload):
            break
        pid = payload[off]
        used, cap, extra = struct.unpack_from("<III", payload, off + 1)
        entries.append((pid, used, cap, extra))
    return entries


def _fmt_amount(value: int, unit: str) -> str:
    if unit != "B":
        return str(value)
    if value >= 1024:
        return f"{value / 1024:.1f} KB"
    return f"{value} B"


def format_memdetail(entries) -> list[str]:
    """Render MemDetail entries as a text table (one string per line)."""
    lines = [f"{'pool':<16}{'used':>10}{'capacity':>12}{'use':>6}   extra",
             "-" * 58]
    pool_heap = 0
    for pid, used, cap, extra in entries:
        name, unit, extra_kind = MEM_POOLS.get(pid, (f"pool {pid}", "", "extra"))
        if cap == 0:
            # 0 capacity = this board does not report the pool.
            lines.append(f"{name:<16}{'n/a':>10}{'n/a':>12}")
            continue
        pct = f"{100 * used / cap:.0f}%"
        if extra_kind == "heap":
            pool_heap += extra
            extra_txt = f"{_fmt_amount(extra, 'B')} heap"
        elif extra_kind in ("reclaimable", "in arena"):
            extra_txt = f"{_fmt_amount(extra, 'B')} {extra_kind}"
        elif extra_kind == "largest free":
            # 0 = not tracked (esp-alloc on epaper); a full heap still
            # shows up as 100% use.
            extra_txt = f"largest free block {_fmt_amount(extra, 'B')}" if extra else ""
        elif extra_kind == "peak":
            extra_txt = f"peak {_fmt_amount(extra, 'B')}" if extra else ""
        else:
            extra_txt = ""
        lines.append(f"{name:<16}{_fmt_amount(used, unit):>10}"
                     f"{_fmt_amount(cap, unit):>12}{pct:>6}   {extra_txt}".rstrip())
    lines.append("-" * 58)
    lines.append(f"heap held by the pools above: {_fmt_amount(pool_heap, 'B')}")
    return lines


def cmd_memdetail(ser: serial.Serial) -> bool:
    """Query per-pool memory usage (heap, strings, UART, widgets, VM)."""
    resp_type, resp_val = request(ser, build_varint_msg(TAG_MEMDETAIL))
    if resp_type == "memdetail":
        for line in format_memdetail(resp_val):
            print(line)
        return True
    elif resp_type == "error":
        desc = ERROR_DESCRIPTIONS.get(resp_val, "unknown")
        print(f"error: code {resp_val} — {desc}", file=sys.stderr)
        return False
    else:
        print("no response (timeout) — firmware may predate memdetail", file=sys.stderr)
        return False


def cmd_stackinfo(ser: serial.Serial) -> bool:
    """Query current stack usage from device."""
    resp_type, resp_val = request(ser, build_varint_msg(TAG_STACKINFO))
    if resp_type == "stackinfo":
        used, free, peak = resp_val
        total = used + free
        print(f"stack used: {used} bytes ({used / 1024:.1f} KB)")
        print(f"stack free: {free} bytes ({free / 1024:.1f} KB)")
        print(f"stack total: {total} bytes ({total / 1024:.1f} KB)")
        if peak:
            print(f"stack peak: {peak} bytes ({peak / 1024:.1f} KB, "
                  f"{100 * peak / total:.0f}% of total, {total - peak} bytes never touched)")
        return True
    elif resp_type == "error":
        desc = ERROR_DESCRIPTIONS.get(resp_val, "unknown")
        print(f"error: code {resp_val} — {desc}", file=sys.stderr)
        return False
    else:
        print("no response (timeout)", file=sys.stderr)
        return False


def cmd_execute(ser: serial.Serial, path: str) -> bool:
    """Send bytecode file for immediate execution."""
    try:
        with open(path, "rb") as f:
            payload = f.read()
    except FileNotFoundError:
        print(f"file not found: {path}", file=sys.stderr)
        return False

    if len(payload) > 2048:
        print(f"program too large: {len(payload)} bytes (max 2048, use writefs for larger)", file=sys.stderr)
        return False

    drain_input(ser)
    ser.write(build_payload_msg(TAG_EXECUTE, payload))
    ser.flush()
    print(f"execute: sent {len(payload)} bytes")
    return True


def cmd_writefs(ser: serial.Serial, path: str) -> bool:
    """Send a flash filesystem image (see `writefs_data`)."""
    try:
        with open(path, "rb") as f:
            data = f.read()
    except FileNotFoundError:
        print(f"file not found: {path}", file=sys.stderr)
        return False
    return writefs_data(ser, data)


def writefs_data(ser: serial.Serial, data: bytes, log=None) -> bool:
    """Send a flash filesystem image in chunks with flow control.

    Protocol (protocol.rs, "CRC chunks"):
      1. WriteFs header: total_size u32 + chunk_size u32 + flags u32 (CRC
         chunks) + CRC-32 of those 12 bytes.
      2. Firmware with CRC chunks answers FsCaps; older firmware reads only
         the first 8 bytes and answers a pong: plain chunks then.
      3. CRC mode: each chunk = seq u32 + CRC-32(seq + data) u32 + data. A
         chunk answered with error 10, or not answered (bytes lost), is sent
         again, up to CHUNK_TRIES times; nothing bad reaches the flash.
      4. After the last ACK the device checks the whole image and restarts.

    `log(text)` gets progress lines (default: print). A plain-mode timeout
    restarts the transfer from the header once, after a resync pause.
    """
    log = log or (lambda text: print(text, flush=True))
    result = _writefs_once(ser, data, log)
    if result == "timeout":
        log("retrying writefs from the header...")
        resync(ser)
        result = _writefs_once(ser, data, log)
    return result == "ok"


def _fs_header(total_size: int) -> bytes:
    head = struct.pack("<III", total_size, CHUNK_SIZE, FS_FLAG_CRC | FS_FLAG_SEQ_ACK)
    return head + struct.pack("<I", zlib.crc32(head))


def _crc_chunk(seq: int, chunk: bytes) -> bytes:
    seq_le = struct.pack("<I", seq)
    return seq_le + struct.pack("<I", zlib.crc32(chunk, zlib.crc32(seq_le))) + chunk


def _send_header(ser: serial.Serial, total_size: int, log) -> str:
    """Send the header until it is taken: "crc", "plain", "timeout" or "error"."""
    for attempt in range(3):
        drain_input(ser)
        ser.write(build_payload_msg(TAG_WRITEFS, _fs_header(total_size)))
        ser.flush()
        resp_type, resp_val = read_response(ser)
        if resp_type == "fscaps":
            if resp_val & FS_FLAG_SEQ_ACK:
                return "seq"
            return "crc" if resp_val & FS_FLAG_CRC else "plain"
        if resp_type == "pong":
            return "plain"
        if resp_type == "error" and resp_val == ERR_TRANSFER_CRC:
            log("header damaged on the line, resending")
            continue
        if resp_type == "error":
            desc = ERROR_DESCRIPTIONS.get(resp_val, "unknown")
            log(f"error: code {resp_val} — {desc}")
            return "error"
        return "timeout"
    return "timeout"


def _writefs_once(ser: serial.Serial, data: bytes, log) -> str:
    """One writefs transfer: "ok", "timeout" or "error"."""
    total_size = len(data)
    chunk_count = (total_size + CHUNK_SIZE - 1) // CHUNK_SIZE
    log(f"writefs: {total_size} bytes, {chunk_count} chunks ({CHUNK_SIZE}B each)")

    mode = _send_header(ser, total_size, log)
    if mode == "timeout":
        log("no ACK for header (timeout)")
    if mode in ("timeout", "error"):
        return mode
    if mode == "seq":
        log("CRC chunks with sequence ACKs")
        return _send_seq_chunks(ser, data, log)
    crc = mode == "crc"
    log("CRC chunks with retry" if crc else "device firmware has no CRC chunks: plain transfer")

    if crc:
        return _send_crc_chunks(ser, data, log)

    for i in range(chunk_count):
        chunk = data[i * CHUNK_SIZE:(i + 1) * CHUNK_SIZE]
        ser.write(build_payload_msg(TAG_WRITECHUNK, chunk))
        ser.flush()
        resp_type, resp_val = read_response(ser)
        if resp_type == "error":
            desc = ERROR_DESCRIPTIONS.get(resp_val, "unknown")
            log(f"\nerror: code {resp_val} — {desc}")
            return "error"
        if resp_type != "pong":
            log(f"\nno ACK for chunk {i + 1} (timeout)")
            return "timeout"
        pct = (i + 1) * 100 // chunk_count
        print(f"\r  [{i + 1}/{chunk_count}] {pct}%", end="", flush=True)

    log(" done (device will restart)")
    return "ok"


def _probe_taken(ser: serial.Serial, seq: int):
    """Whether the device has written chunk `seq`: True / False, None if it
    doesn't answer. Sends an empty chunk numbered seq - 1: the device ACKs it
    as a duplicate only while seq - 1 is the last chunk it took (so `seq` is
    not taken); otherwise it answers error 10. Corruption can turn an ACK
    into an error but never the reverse, so one ACK settles it and errors
    must repeat."""
    probe = build_payload_msg(TAG_WRITECHUNK, _crc_chunk((seq - 1) & 0xFFFFFFFF, b""))
    errors = 0
    for _ in range(PROBE_TRIES):
        ser.write(probe)
        ser.flush()
        resp_type, resp_val = read_response(ser)
        if resp_type == "pong":
            return False
        if resp_type == "error" and resp_val == ERR_TRANSFER_CRC:
            errors += 1
            if errors >= 3:
                return True
        else:
            resync(ser)
    return None


def _await_fsack(ser: serial.Serial, seq: int, versions: dict) -> tuple:
    """Reads replies to chunk `seq` until one settles it: ("taken", length),
    ("bad", None), ("timeout", None) or ("error", code). An FsAck is read by
    content, so stale replies (extra answers to a damaged chunk) can't be
    taken for this one: next_seq = seq + 1 with the CRC of a version sent
    for this seq = taken (with that version's length); next_seq = seq = not
    taken; older = stale, skipped. `versions` maps CRC field -> length."""
    timeout = ser.timeout
    deadline = time.time() + timeout
    try:
        while True:
            remaining = deadline - time.time()
            if remaining <= 0:
                return ("timeout", None)
            ser.timeout = remaining
            resp_type, resp_val = read_response(ser)
            if resp_type == "timeout":
                return ("timeout", None)
            if resp_type == "error" and resp_val != ERR_TRANSFER_CRC:
                return ("error", resp_val)
            if resp_type != "fsack":
                continue  # pong / error 10: not sent in this mode, noise
            next_seq, last_crc = resp_val
            if next_seq == seq + 1 and last_crc in versions:
                return ("taken", versions[last_crc])
            if next_seq == seq:
                return ("bad", None)
            if next_seq > seq + 1 or next_seq == seq + 1:
                # Ahead of us, or a CRC we never sent for this seq: the
                # host and the device disagree. Can't happen unless the
                # reply was forged by corruption that passed its CRC.
                return ("error", ERR_TRANSFER_CRC)
            # next_seq < seq: a stale reply, keep reading.
    finally:
        ser.timeout = timeout


def _send_seq_chunks(ser: serial.Serial, data: bytes, log) -> str:
    """Sequence-ACK chunks (FS_FLAG_SEQ_ACK). Every reply says how far the
    device is, so a failing chunk can be halved at any time: if the device
    did take an earlier, longer version of this seq, its FsAck names that
    version's CRC and the host moves on by that length."""
    total_size = len(data)
    size = CHUNK_SIZE
    seq = pos = retries = fails = 0
    versions = {}  # CRC field -> length, for the versions of this seq sent
    while pos < total_size:
        chunk = data[pos:pos + size]
        body = _crc_chunk(seq, chunk)
        versions[int.from_bytes(body[4:8], "little")] = len(chunk)
        ser.write(build_payload_msg(TAG_WRITECHUNK, body))
        ser.flush()
        outcome, value = _await_fsack(ser, seq, versions)
        if outcome == "taken":
            pos += value
            seq += 1
            fails = 0
            versions = {}
            print(f"\r  {pos}/{total_size} bytes ({pos * 100 // total_size}%), chunk {size} B   ",
                  end="", flush=True)
            continue
        if outcome == "error":
            desc = ERROR_DESCRIPTIONS.get(value, "unknown")
            log(f"\nerror: code {value} — {desc}")
            return "error"
        retries += 1
        fails += 1
        why = "CRC mismatch" if outcome == "bad" else "no ACK"
        if outcome == "timeout":
            resync(ser)
        if size > MIN_CRC_CHUNK and fails >= 2:
            size = max(MIN_CRC_CHUNK, size // 2)
            fails = 0
            log(f"\n  seq {seq}: {why}, chunk size -> {size} B")
            continue
        tries = MIN_CHUNK_TRIES if size <= MIN_CRC_CHUNK else CHUNK_TRIES
        if fails >= tries:
            log(f"\nseq {seq} (offset {pos}): gave up after {tries} attempts at {size} B")
            return "error"
        if size <= MIN_CRC_CHUNK:
            time.sleep(MIN_CHUNK_PAUSE)
        log(f"\n  seq {seq}: {why}, resending ({fails}/{tries})")

    note = f", {retries} resend(s), last chunk size {size} B" if retries else ""
    log(f" done{note} (device will restart)")
    return "ok"


def _send_crc_chunks(ser: serial.Serial, data: bytes, log) -> str:
    """CRC-mode chunks. The device accepts any chunk length here, so a chunk
    that keeps failing is halved (down to MIN_CRC_CHUNK): on a line that
    corrupts one byte in a few KB, small chunks still get through. After
    CHUNK_TRIES failures at the smallest size it gives up."""
    total_size = len(data)
    size = CHUNK_SIZE
    seq = 0
    pos = 0
    retries = 0
    fails = 0  # consecutive failures at the current size
    # This seq went unanswered once: the device may have taken it (lost
    # pong), so it must be resent with the same bytes until it is ACKed.
    maybe_taken = False
    while pos < total_size:
        chunk = data[pos:pos + size]
        ser.write(build_payload_msg(TAG_WRITECHUNK, _crc_chunk(seq, chunk)))
        ser.flush()
        resp_type, resp_val = read_response(ser)
        if resp_type == "pong":
            pos += len(chunk)
            seq += 1
            fails = 0
            maybe_taken = False
            print(f"\r  {pos}/{total_size} bytes ({pos * 100 // total_size}%), chunk {size} B   ",
                  end="", flush=True)
            continue
        if resp_type == "error" and resp_val != ERR_TRANSFER_CRC:
            desc = ERROR_DESCRIPTIONS.get(resp_val, "unknown")
            log(f"\nerror: code {resp_val} — {desc}")
            return "error"
        retries += 1
        fails += 1
        if resp_type == "error":
            # A chunk cut short by a damaged length leaves its data to be
            # parsed as messages, and stray chunk/header tags in it answer
            # with more errors. Drop those, or every later reply is read
            # one behind (a taken chunk looks failed, then gets resent).
            time.sleep(0.1)
            drain_input(ser, quiet=0.1)
        why = "CRC mismatch" if resp_type == "error" else "no ACK"
        if resp_type != "error":
            # Bytes lost: the device drops the partial chunk after its
            # resync timeout, then takes the resend.
            resync(ser)
            maybe_taken = True
        if maybe_taken and resp_type == "error" and fails >= 2:
            # Resends keep failing: ask whether the device already has it.
            taken = _probe_taken(ser, seq)
            last = pos + len(chunk) >= total_size
            if taken or (taken is None and last):
                # It has it (or, last chunk unanswered: it restarted).
                log(f"\n  seq {seq}: already taken, ACK was lost")
                pos += len(chunk)
                seq += 1
                fails = 0
                maybe_taken = False
                continue
            if taken is False:
                maybe_taken = False
        # Shrink only while the device surely hasn't taken this seq: after a
        # missing ACK it may have written it (lost pong) and then ACKs only
        # a duplicate of the same bytes — a smaller resend would desync.
        if resp_type == "error" and not maybe_taken and size > MIN_CRC_CHUNK and fails >= 2:
            size = max(MIN_CRC_CHUNK, size // 2)
            fails = 0
            log(f"\n  seq {seq}: {why}, chunk size -> {size} B")
            continue
        tries = MIN_CHUNK_TRIES if size <= MIN_CRC_CHUNK else CHUNK_TRIES
        if fails >= tries:
            log(f"\nseq {seq} (offset {pos}): gave up after {tries} attempts at {size} B")
            return "error"
        if size <= MIN_CRC_CHUNK:
            time.sleep(MIN_CHUNK_PAUSE)
        log(f"\n  seq {seq}: {why}, resending ({fails}/{tries})")

    note = f", {retries} resend(s), last chunk size {size} B" if retries else ""
    log(f" done{note} (device will restart)")
    return "ok"


def cmd_send(ser: serial.Serial, data_args: list[str]) -> bool:
    """Send a user message to the device (field 6, max 64 bytes).

    Data can be specified as:
      - Hex bytes: 0x0A 0xFF 0x00
      - Decimal bytes: 10 255 0
      - Text string: "hello world"
    """
    if not data_args:
        print("no data to send", file=sys.stderr)
        return False

    # Check if it's a quoted string
    joined = ' '.join(data_args)
    if joined.startswith('"') and joined.endswith('"'):
        payload = joined[1:-1].encode('utf-8')
    else:
        # Parse as byte values (hex or decimal)
        payload = bytearray()
        for arg in data_args:
            try:
                val = int(arg, 0)  # auto-detect hex/dec/oct/bin
                if val < 0 or val > 255:
                    print(f"byte value out of range: {arg}", file=sys.stderr)
                    return False
                payload.append(val)
            except ValueError:
                # Try as raw text
                payload.extend(arg.encode('utf-8'))
        payload = bytes(payload)

    if len(payload) > 64:
        print(f"message too large: {len(payload)} bytes (max 64)", file=sys.stderr)
        return False

    ser.write(build_payload_msg(TAG_USERMSG, payload))
    ser.flush()

    hex_str = ' '.join(f'{b:02X}' for b in payload)
    print(f"send: {len(payload)} bytes [{hex_str}]")
    return True


def listen_serial(ser: serial.Serial, hex_dump: bool = False, timestamps: bool = True):
    """Print what the device sends until Ctrl+C.

    Text lines are printed as they end; other bytes inline as \\xNN (or every
    byte as hex with `hex_dump`). Each line gets the time since start. The
    port is kept across device restarts; if it disappears (USB adapter or
    USB-serial device re-enumerating) it is reopened when it comes back.
    """
    print("--- Monitoring %s (Ctrl+C to stop) ---" % ser.port, flush=True)
    ser.timeout = 0.1  # short timeout for responsive Ctrl+C
    t0 = time.time()
    buf = bytearray()

    def stamp():
        return f"[{time.time() - t0:8.3f}] " if timestamps else ""

    def emit():
        line = bytes(buf).decode("utf-8", errors="replace").rstrip("\r")
        print(stamp() + line, flush=True)
        buf.clear()

    try:
        while True:
            try:
                data = ser.read(256)
            except serial.SerialException:
                if buf:
                    emit()
                print(stamp() + "--- port lost, waiting for it ---", flush=True)
                ser = _reopen(ser)
                print(stamp() + "--- port back ---", flush=True)
                continue
            if not data:
                continue
            if hex_dump:
                print(stamp() + " ".join(f"{b:02X}" for b in data), flush=True)
                continue
            for b in data:
                if b == 0x0A:
                    emit()
                elif b >= 0x20 or b in (0x0D, 0x09):
                    buf.append(b)
                else:
                    buf.extend(f"\\x{b:02X}".encode())
    except KeyboardInterrupt:
        if buf:
            emit()
        print("\n--- Stopped ---")


def _reopen(ser: serial.Serial) -> serial.Serial:
    """Close `ser` and open the same port again once it exists (Ctrl+C aborts)."""
    port, timeout = ser.port, ser.timeout
    try:
        ser.close()
    except Exception:
        pass
    while True:
        time.sleep(0.5)
        try:
            new = open_port(port, timeout)
            return new
        except serial.SerialException:
            continue


def cmd_calibrate(ser: serial.Serial) -> bool:
    """Start 3-point touch calibration.

    Sends the calibrate command and waits for the device to complete the
    calibration sequence (user must touch 3 crosshairs on screen).
    Receives calibration result payload.
    """
    print("Starting touch calibration...")
    print("Touch the 3 crosshairs on screen in order (top-left, top-right, bottom-left)")
    drain_input(ser)
    ser.write(build_varint_msg(TAG_TOUCHCAL))
    ser.flush()

    # Use a long timeout — user needs time to touch 3 points
    orig_timeout = ser.timeout
    ser.timeout = 60.0

    resp_type, resp_val = read_response(ser)
    ser.timeout = orig_timeout

    if resp_type == "touchcal" and resp_val and len(resp_val) >= 9:
        flags = resp_val[0]
        xy_swap = bool(flags & 0x01)
        x_flip  = bool(flags & 0x02)
        y_flip  = bool(flags & 0x04)
        x_min = int.from_bytes(resp_val[1:3], 'little')
        x_max = int.from_bytes(resp_val[3:5], 'little')
        y_min = int.from_bytes(resp_val[5:7], 'little')
        y_max = int.from_bytes(resp_val[7:9], 'little')
        print(f"Calibration complete:")
        print(f"  xy_swap = {xy_swap}")
        print(f"  x_flip  = {x_flip}")
        print(f"  y_flip  = {y_flip}")
        print(f"  x_min   = {x_min}")
        print(f"  x_max   = {x_max}")
        print(f"  y_min   = {y_min}")
        print(f"  y_max   = {y_max}")
        return True
    elif resp_type == "error":
        desc = ERROR_DESCRIPTIONS.get(resp_val, "unknown")
        print(f"error: code {resp_val} — {desc}", file=sys.stderr)
        return False
    else:
        print(f"no calibration response (timeout or unexpected: {resp_type})", file=sys.stderr)
        return False


def cmd_settime(ser: serial.Serial, dt_str: str | None) -> bool:
    """Set device RTC. Payload: year(0-99) month day weekday hour minute second."""
    from datetime import datetime as dt_cls

    if dt_str is None:
        now = dt_cls.now()
    else:
        try:
            now = dt_cls.strptime(dt_str, "%Y-%m-%d %H:%M:%S")
        except ValueError:
            print(f"invalid format (expected YYYY-MM-DD HH:MM:SS): {dt_str}", file=sys.stderr)
            return False

    year = now.year - 2000          # AT8563T stores 0-99 offset from 2000
    weekday = (now.weekday() + 1) % 7  # Python 0=Mon → RTC 0=Sun

    payload = bytes([year, now.month, now.day, weekday, now.hour, now.minute, now.second])
    resp_type, resp_val = request(ser, build_payload_msg(TAG_SETDATETIME, payload))
    if resp_type == "pong":
        day_names = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
        print(f"time set: {now.strftime('%Y-%m-%d %H:%M:%S')} ({day_names[weekday]})")
        return True
    elif resp_type == "error":
        desc = ERROR_DESCRIPTIONS.get(resp_val, "unknown")
        print(f"error: code {resp_val} — {desc}", file=sys.stderr)
        return False
    else:
        print("no response (timeout)", file=sys.stderr)
        return False


# --- Main ---


def main():
    parser = argparse.ArgumentParser(
        prog="ferrite-cli",
        description="Host-side tool for ferrite-ui devices",
    )
    parser.add_argument("-p", "--port", required=True, help="Serial port (e.g. COM3, /dev/ttyUSB0)")
    parser.add_argument("-t", "--timeout", type=float, default=DEFAULT_TIMEOUT, help="Response timeout in seconds (default: 2.0)")
    parser.add_argument("-l", "--listen", action="store_true",
                        help="After command, keep printing what the device sends, e.g. the boot "
                             "log after writefs restarts it (Ctrl+C to stop)")

    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("ping", help="Ping device (expects pong)")
    sub.add_parser("restart", help="Restart device")
    sub.add_parser("meminfo", help="Query free heap memory")
    sub.add_parser("memdetail", help="Query per-pool memory usage (heap, strings, UART, widgets, VM)")

    p_exec = sub.add_parser("execute", help="Send bytecode for execution")
    p_exec.add_argument("file", help="Bytecode binary file")

    p_fs = sub.add_parser("writefs", help="Write flash filesystem image")
    p_fs.add_argument("file", help="Flash filesystem image file")

    p_send = sub.add_parser("send", help="Send user message to device (field 6)")
    p_send.add_argument("data", nargs="+", help="Data: hex bytes (0xFF), decimal (255), or \"text\"")

    sub.add_parser("calibrate", help="Start 3-point touch calibration")
    sub.add_parser("stackinfo", help="Query current stack usage")

    p_mon = sub.add_parser("monitor", help="Print what the device sends (boot log, restarts); Ctrl+C ends")
    p_mon.add_argument("--hex", action="store_true", help="show every byte as hex")
    p_mon.add_argument("--no-time", action="store_true", help="no timestamps")

    p_settime = sub.add_parser("settime", help="Set device RTC date and time")
    p_settime.add_argument("datetime", nargs="?", default=None,
                           metavar="YYYY-MM-DD HH:MM:SS",
                           help="Date/time string (default: current system time)")

    args = parser.parse_args()

    try:
        ser = open_port(args.port, args.timeout)
    except serial.SerialException as e:
        print(f"serial error: {e}", file=sys.stderr)
        sys.exit(1)

    with ser:
        if args.command == "ping":
            ok = cmd_ping(ser)
        elif args.command == "restart":
            ok = cmd_restart(ser)
        elif args.command == "meminfo":
            ok = cmd_meminfo(ser)
        elif args.command == "memdetail":
            ok = cmd_memdetail(ser)
        elif args.command == "execute":
            ok = cmd_execute(ser, args.file)
        elif args.command == "writefs":
            ok = cmd_writefs(ser, args.file)
        elif args.command == "send":
            ok = cmd_send(ser, args.data)
        elif args.command == "calibrate":
            ok = cmd_calibrate(ser)
        elif args.command == "stackinfo":
            ok = cmd_stackinfo(ser)
        elif args.command == "settime":
            ok = cmd_settime(ser, args.datetime)
        elif args.command == "monitor":
            listen_serial(ser, hex_dump=args.hex, timestamps=not args.no_time)
            ok = True
        else:
            ok = False

        if args.listen and args.command != "monitor":
            listen_serial(ser)

    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
