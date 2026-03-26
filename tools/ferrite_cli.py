#!/usr/bin/env python3
"""ferrite-cli — Host-side tool for communicating with ferrite-ui devices.

Implements the USART protobuf protocol (see protocol.rs).
Commands: ping, restart, execute <file>, writefs <file>, send <data...>

Usage:
    python ferrite_cli.py -p COM3 ping
    python ferrite_cli.py -p COM3 restart
    python ferrite_cli.py -p COM3 execute program.bin
    python ferrite_cli.py -p COM3 writefs flash.bin
    python ferrite_cli.py -p COM3 send 0x01 0x02 0x03
    python ferrite_cli.py -p COM3 send "hello world"
"""

import argparse
import struct
import sys
import time
import serial

# --- Protocol constants ---

# Host → Device tags: (field << 3) | wire_type
TAG_PING    = (1 << 3) | 0   # 0x08, varint
TAG_EXECUTE = (2 << 3) | 2   # 0x12, payload
TAG_RESTART = (3 << 3) | 0   # 0x18, varint
TAG_WRITEFS    = (4 << 3) | 2   # 0x22, payload (header: total_size + chunk_size)
TAG_WRITECHUNK = (5 << 3) | 2   # 0x2A, payload (chunk data)
TAG_USERMSG    = (6 << 3) | 2   # 0x32, payload (user message data, max 64 bytes)

CHUNK_SIZE = 4096  # Must match device sector buffer size

# Device → Host tags
TAG_ERROR = (1 << 3) | 2     # 0x0A, payload
TAG_PONG  = (2 << 3) | 0     # 0x10, varint

BAUD_RATE = 115200
DEFAULT_TIMEOUT = 2.0  # seconds

ERROR_DESCRIPTIONS = {
    1: "page_main not found",
    2: "main program not found",
    3: "image not found",
    4: "font not found",
    5: "no file system in flash",
    6: "program execution error",
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
    """Open serial port with ferrite-ui settings (115200 8N1)."""
    return serial.Serial(
        port=port,
        baudrate=BAUD_RATE,
        bytesize=serial.EIGHTBITS,
        parity=serial.PARITY_NONE,
        stopbits=serial.STOPBITS_ONE,
        timeout=timeout,
    )


def read_response(ser: serial.Serial) -> tuple[str, int | None]:
    """Read a single device response. Returns (type, value).

    Returns:
        ("pong", None) for pong
        ("error", code) for error
        ("timeout", None) if no response
    """
    tag_byte = ser.read(1)
    if not tag_byte:
        return ("timeout", None)

    tag = tag_byte[0]

    if tag == TAG_PONG:
        return ("pong", None)

    if tag == TAG_ERROR:
        # Read length varint
        length_bytes = bytearray()
        while True:
            b = ser.read(1)
            if not b:
                return ("timeout", None)
            length_bytes.append(b[0])
            if b[0] & 0x80 == 0:
                break
        length, _ = decode_varint(bytes(length_bytes))
        # Read payload
        payload = ser.read(length)
        if len(payload) < length:
            return ("timeout", None)
        return ("error", payload[0])

    # Unknown tag — skip
    return ("unknown", tag)


# --- Commands ---


def cmd_ping(ser: serial.Serial) -> bool:
    """Send ping and wait for pong."""
    ser.write(build_varint_msg(TAG_PING))
    ser.flush()

    resp_type, resp_val = read_response(ser)
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


def cmd_execute(ser: serial.Serial, path: str) -> bool:
    """Send bytecode file for immediate execution."""
    try:
        with open(path, "rb") as f:
            payload = f.read()
    except FileNotFoundError:
        print(f"file not found: {path}", file=sys.stderr)
        return False

    if len(payload) > 1024:
        print(f"program too large: {len(payload)} bytes (max 1024, use writefs for larger)", file=sys.stderr)
        return False

    ser.write(build_payload_msg(TAG_EXECUTE, payload))
    ser.flush()
    print(f"execute: sent {len(payload)} bytes")
    return True


def cmd_writefs(ser: serial.Serial, path: str) -> bool:
    """Send flash filesystem image in chunks with flow control.

    Protocol:
      1. Send WriteFs header (total_size u32 LE + chunk_size u32 LE)
      2. Wait for pong ACK
      3. For each chunk: send WriteChunk, wait for pong ACK
      4. Device restarts after final ACK
    """
    try:
        with open(path, "rb") as f:
            data = f.read()
    except FileNotFoundError:
        print(f"file not found: {path}", file=sys.stderr)
        return False

    total_size = len(data)
    chunk_count = (total_size + CHUNK_SIZE - 1) // CHUNK_SIZE

    print(f"writefs: {total_size} bytes, {chunk_count} chunks ({CHUNK_SIZE}B each)")

    # 1. Send header: total_size(u32 LE) + chunk_size(u32 LE)
    header = struct.pack("<II", total_size, CHUNK_SIZE)
    ser.write(build_payload_msg(TAG_WRITEFS, header))
    ser.flush()

    # 2. Wait for ACK
    resp_type, resp_val = read_response(ser)
    if resp_type != "pong":
        if resp_type == "error":
            desc = ERROR_DESCRIPTIONS.get(resp_val, "unknown")
            print(f"error: code {resp_val} — {desc}", file=sys.stderr)
        else:
            print(f"no ACK for header (timeout)", file=sys.stderr)
        return False

    # 3. Send chunks
    for i in range(chunk_count):
        offset = i * CHUNK_SIZE
        chunk = data[offset:offset + CHUNK_SIZE]

        ser.write(build_payload_msg(TAG_WRITECHUNK, chunk))
        ser.flush()

        # Wait for ACK
        resp_type, resp_val = read_response(ser)
        if resp_type == "error":
            desc = ERROR_DESCRIPTIONS.get(resp_val, "unknown")
            print(f"\nerror: code {resp_val} — {desc}", file=sys.stderr)
            return False
        elif resp_type != "pong":
            print(f"\nno ACK for chunk {i + 1} (timeout)", file=sys.stderr)
            return False

        # Progress
        pct = (i + 1) * 100 // chunk_count
        print(f"\r  [{i + 1}/{chunk_count}] {pct}%", end="", flush=True)

    print(" done (device will restart)")
    return True


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


# --- Main ---


def main():
    parser = argparse.ArgumentParser(
        prog="ferrite-cli",
        description="Host-side tool for ferrite-ui devices",
    )
    parser.add_argument("-p", "--port", required=True, help="Serial port (e.g. COM3, /dev/ttyUSB0)")
    parser.add_argument("-t", "--timeout", type=float, default=DEFAULT_TIMEOUT, help="Response timeout in seconds (default: 2.0)")

    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("ping", help="Ping device (expects pong)")
    sub.add_parser("restart", help="Restart device")

    p_exec = sub.add_parser("execute", help="Send bytecode for execution")
    p_exec.add_argument("file", help="Bytecode binary file")

    p_fs = sub.add_parser("writefs", help="Write flash filesystem image")
    p_fs.add_argument("file", help="Flash filesystem image file")

    p_send = sub.add_parser("send", help="Send user message to device (field 6)")
    p_send.add_argument("data", nargs="+", help="Data: hex bytes (0xFF), decimal (255), or \"text\"")

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
        elif args.command == "execute":
            ok = cmd_execute(ser, args.file)
        elif args.command == "writefs":
            ok = cmd_writefs(ser, args.file)
        elif args.command == "send":
            ok = cmd_send(ser, args.data)
        else:
            ok = False

    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
