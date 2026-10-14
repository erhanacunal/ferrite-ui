#!/usr/bin/env python3
"""
Host-side tool for the N32903 NAND loader's UART menu.

Interrupts the loader's 3 s boot countdown (or reuses an already open
menu), then runs one command:

  python flash_tool.py COM5 writeapp firmware.bin [--boot]
  python flash_tool.py COM5 writeldr nandloader.bin
  python flash_tool.py COM5 boot [--monitor]

writeldr expects the loader *with* its IBR boot header (the nandloader.bin
produced by build.ps1 / prepend_header.py).

Wire protocol: see src/update.rs.  Requires pyserial (pip install pyserial).
"""

import argparse
import struct
import sys
import time
import zlib

import serial

ACK = 0x06
NAK = 0x15
MAGIC = b"NLDR"
CHUNK = 4096                  # must match CHUNK in src/update.rs
PROMPT = b"n32903> "
COUNTDOWN = b"Press any key to enter menu"
BOOT_CODE_MARKER = 0x57425AA5
BOOT_HEADER_LEN = 32

ACK_TIMEOUT = 5.0             # per protocol step
CHUNK_RETRIES = 8             # device gives up after 10
PROGRAM_TIMEOUT = 300.0       # erase + program + verify of the whole image


class LoaderError(Exception):
    pass


class Loader:
    def __init__(self, port, baud):
        self.ser = serial.Serial(port, baud, timeout=0.05)
        self.line_start = True

    def close(self):
        self.ser.close()

    def _show(self, data):
        """Echo device text to the console."""
        text = data.decode("ascii", errors="replace").replace("\r", "")
        if text:
            sys.stdout.write(text)
            sys.stdout.flush()
            self.line_start = text.endswith("\n")

    def read_until(self, patterns, timeout):
        """Read (and echo) until one of `patterns` appears; return it, or None."""
        if isinstance(patterns, bytes):
            patterns = [patterns]
        buf = b""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            data = self.ser.read(256)
            if data:
                self._show(data)
                buf = (buf + data)[-256:]
                for p in patterns:
                    if p in buf:
                        return p
        return None

    def wait_ctrl(self, timeout):
        """Echo text until the device sends ACK or NAK.  None on timeout."""
        deadline = time.monotonic() + timeout
        text = b""
        while time.monotonic() < deadline:
            c = self.ser.read(1)
            if not c:
                continue
            if c[0] in (ACK, NAK):
                self._show(text)
                return c[0]
            text += c
            if c == b"\n" or len(text) >= 80:
                self._show(text)
                text = b""
        self._show(text)
        return None

    # ------------------------------------------------------------------

    def enter_menu(self, wait):
        """Get to the n32903> prompt, catching the boot countdown if needed."""
        self.ser.reset_input_buffer()
        self.ser.write(b"\r")
        hit = self.read_until([PROMPT, COUNTDOWN], 0.5)
        if hit is None:
            print(f"Waiting up to {wait:.0f} s for the loader - reset or power-cycle the board now...")
            hit = self.read_until([PROMPT, COUNTDOWN], wait)
        if hit is None:
            raise LoaderError("loader countdown not seen")
        if hit == COUNTDOWN:
            self.ser.write(b" ")
            if not self.read_until(PROMPT, 3):
                raise LoaderError("no menu prompt after interrupting countdown")

    def command(self, cmd):
        self.ser.write(cmd.encode() + b"\r")

    def send_image(self, cmd, image):
        self.command(cmd)
        if self.wait_ctrl(ACK_TIMEOUT) != ACK:
            raise LoaderError(f"device not ready for {cmd}")

        header = MAGIC + struct.pack("<II", len(image), zlib.crc32(image))
        self.ser.write(header)
        if self.wait_ctrl(ACK_TIMEOUT) != ACK:
            raise LoaderError("device rejected the image header")

        start = time.monotonic()
        for offset in range(0, len(image), CHUNK):
            off = struct.pack("<I", offset)
            data = image[offset:offset + CHUNK]
            frame = off + data + struct.pack("<I", zlib.crc32(off + data))
            for attempt in range(CHUNK_RETRIES + 1):
                self.ser.write(frame)
                reply = self.wait_ctrl(ACK_TIMEOUT)
                if reply == ACK:
                    break
                if not self.line_start:
                    print()
                print(f"  chunk @0x{offset:X}: {'NAK' if reply == NAK else 'timeout'}, retrying")
            else:
                raise LoaderError(f"transfer failed at offset 0x{offset:X}")
            self._progress(offset + len(data), len(image), start)
        print()

        print("Programming NAND...")
        reply = self.wait_ctrl(PROGRAM_TIMEOUT)
        if reply != ACK:
            raise LoaderError("device reported failure" if reply == NAK else "no result from device")

    def _progress(self, done, total, start):
        elapsed = max(time.monotonic() - start, 1e-6)
        rate = done / elapsed
        eta = (total - done) / rate if rate else 0
        sys.stdout.write(
            f"\r  {done * 100 // total:3d}%  {done}/{total} bytes  "
            f"{rate / 1024:5.1f} KiB/s  ETA {eta:4.0f} s "
        )
        sys.stdout.flush()
        self.line_start = False

    def monitor(self):
        print("--- monitoring, Ctrl-C to quit ---")
        try:
            while True:
                data = self.ser.read(256)
                if data:
                    self._show(data)
        except KeyboardInterrupt:
            print()


def check_loader_image(image):
    if len(image) < BOOT_HEADER_LEN:
        raise LoaderError("file too small to be a loader image")
    marker, exe_addr, image_size = struct.unpack_from("<III", image)
    if marker != BOOT_CODE_MARKER:
        raise LoaderError("no IBR boot header - pass the nandloader.bin produced by prepend_header.py")
    if image_size + BOOT_HEADER_LEN != len(image):
        raise LoaderError(f"boot header ImageSize ({image_size}) does not match file size")
    print(f"Loader image: {len(image)} bytes, ExeAddr=0x{exe_addr:08X}")


def check_app_image(image):
    if len(image) >= 4 and struct.unpack_from("<I", image)[0] == BOOT_CODE_MARKER:
        raise LoaderError("this looks like a loader image (IBR boot header) - use writeldr")
    print(f"Application image: {len(image)} bytes")


def main():
    ap = argparse.ArgumentParser(description="N32903 NAND loader UART tool")
    ap.add_argument("port", help="serial port, e.g. COM5 or /dev/ttyUSB0")
    ap.add_argument("command", choices=["writeapp", "writeldr", "boot"])
    ap.add_argument("file", nargs="?", help="image file for writeapp / writeldr")
    ap.add_argument("-b", "--baud", type=int, default=115200)
    ap.add_argument("-w", "--wait", type=float, default=30,
                    help="seconds to wait for the board to reset (default 30)")
    ap.add_argument("--boot", action="store_true", help="boot the application after writing")
    ap.add_argument("-m", "--monitor", action="store_true", help="keep printing serial output at the end")
    args = ap.parse_args()

    if args.command in ("writeapp", "writeldr") and not args.file:
        ap.error(f"{args.command} needs an image file")

    image = None
    try:
        if args.file:
            with open(args.file, "rb") as f:
                image = f.read()
            if args.command == "writeldr":
                check_loader_image(image)
            else:
                check_app_image(image)

        dev = Loader(args.port, args.baud)
        try:
            dev.enter_menu(args.wait)
            if args.command == "boot":
                dev.command("boot")
            else:
                dev.send_image(args.command, image)
                if args.command == "writeldr":
                    print("Loader updated. Reset the board to run it.")
                if args.boot:
                    dev.read_until(PROMPT, 2)
                    dev.command("boot")
            if args.monitor or args.command == "boot" or args.boot:
                if args.monitor:
                    dev.monitor()
                else:
                    dev.read_until(b"Jumping to firmware", 10)
                    print()
        finally:
            dev.close()
    except (LoaderError, serial.SerialException, OSError) as e:
        print(f"\nERROR: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
