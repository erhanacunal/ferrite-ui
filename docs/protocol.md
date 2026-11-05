# USART Protocol

Ferrite devices use a small protobuf-style protocol over USART0 for host
commands, program upload, flash filesystem updates, diagnostics, and user
messages. The reference implementation is `src/protocol.rs`; the host tool is
`tools/ferrite_cli.py`.

## Serial Settings

- Baud: `115200`
- Data bits: `8`
- Parity: none
- Stop bits: `1`

## Framing

Every message starts with a protobuf-style tag:

```text
tag = (field_number << 3) | wire_type
```

Supported wire types:

| Wire Type | Name | Encoding |
| --- | --- | --- |
| `0` | Varint command | Tag byte only for host-to-device commands. Device varint responses include tag + unsigned varint value. |
| `2` | Payload | Tag + unsigned varint length + raw payload bytes. |

The firmware accepts only the fields listed below. Unknown fields or unsupported
wire types are ignored by the receiver state machine.

## Host To Device

| Field | Tag | Wire | Name | Payload |
| --- | --- | --- | --- | --- |
| `1` | `0x08` | Varint | `Ping` | None |
| `2` | `0x12` | Payload | `Execute` | Program bytecode, max `2048` bytes |
| `3` | `0x18` | Varint | `Restart` | None |
| `4` | `0x22` | Payload | `WriteFs` header | `total_size u32 LE` + `chunk_size u32 LE` [+ `flags u32 LE` + `header CRC-32 u32 LE`] |
| `5` | `0x2A` | Payload | `WriteChunk` | Chunk bytes; in CRC mode `seq u32 LE` + `CRC-32 u32 LE` + chunk bytes |
| `6` | `0x32` | Payload | `UserMessage` | Arbitrary bytes, max `64` bytes |
| `7` | `0x38` | Varint | `MemInfo` | None |
| `8` | `0x40` | Varint | `TouchCalibrate` | None |
| `9` | `0x48` | Varint | `StackInfo` | None |

### Execute

`Execute` sends a RAM-executed program image or raw bytecode to the device:

```text
0x12 <length varint> <program bytes>
```

The payload must be at most `MAX_PROGRAM_SIZE` (`2048`) bytes. Larger programs
should be written through the flash filesystem using `writefs`.

### WriteFs

`WriteFs` uploads a complete flash filesystem image to external flash at
`FS_BASE`.

Flow:

1. Host sends `WriteFs` header: `total_size u32 LE` + `chunk_size u32 LE`.
2. Device parses the header and replies with `Pong`.
3. Host sends one `WriteChunk` payload at a time.
4. Device writes each chunk into a 4KB sector buffer, flushing full sectors to
   flash, then replies with `Pong`.
5. After the final chunk, device flushes the partial sector if needed, replies
   with `Pong`, flushes USART, then resets.

The current CLI uses `4096` byte chunks to match the firmware sector buffer.

#### CRC chunks

Over a noisy line one flipped bit used to spoil the whole upload (error `9`
after the last chunk). A host can ask for checked, retried chunks:

1. Header is 16 bytes: `total_size`, `chunk_size`, `flags` (bit 0 = CRC
   chunks, bit 1 = sequence ACKs, only with bit 0), then the CRC-32
   (zlib/IEEE) of those first 12 bytes. A 16-byte header is always checked,
   whatever its flags say, so a damaged flags word can't turn the transfer
   into a plain one.
2. Firmware that supports it replies `FsCaps` with the flags it accepted
   (`3A 01 01`, or `3A 01 03` with sequence ACKs) instead of `Pong`. Older
   firmware reads only the first 8 bytes and replies `Pong`: the host then
   sends plain chunks. A header whose CRC fails gets error `10`.
3. Each chunk payload is `seq u32 LE` (0, 1, 2…) + `CRC-32(seq bytes + data)
   u32 LE` + data. The device keeps the chunk in RAM until the CRC matches,
   then writes it and replies `Pong`. A bad chunk gets error `10` and nothing
   is written; the host sends the same chunk again.
4. A chunk resent because its `Pong` was lost (`seq` = the one just written,
   with the same CRC field, so the same bytes) is acknowledged again without
   being written twice. A resend of that `seq` with other bytes is bad.
5. A chunk cut short (bytes lost) stalls the parser; after `RX_RESYNC_MS`
   (500 ms) the device drops the partial chunk, and the host resends it after
   its 0.6 s resync wait.
6. From the header until the image is complete (or the line has been quiet
   for `TRANSFER_IDLE_MS`, 10 s), only `WriteFs` and `WriteChunk` tags start
   a message; every other byte is skipped. A damaged chunk tag or length
   leaves chunk data to be parsed as messages, and image bytes are full of
   tags such as `Restart` (`0x18`) or `Ping` (`0x08`).

**Sequence ACKs (flags bit 1).** Pongs and errors don't say which chunk they
answer, and one damaged chunk can draw several replies (stray tags in its
data), so a host can fall one reply behind: it takes a written chunk for a
failed one or the reverse, and the upload ends short or shifted. With bit 1
every chunk, written or not, is answered by `FsAck` (below): `next_seq` (the
chunk the device expects next) and `last_crc` (the CRC field of the last chunk
written). The host reads it by content: `next_seq` = its seq + 1 with the CRC
of a version it sent for that seq = written (it moves on by that version's
length, so halving a failing chunk is always safe); `next_seq` = its seq =
send again; an older `next_seq` = a stale reply, skipped. The last chunk is
answered with `FsAck` too, then the device checks the image (error `9` if
bad) and resets.

`ferrite_cli.py writefs` asks for sequence ACKs and falls back to plain CRC
chunks (`FsCaps` `01`) or plain chunks (`Pong`). It halves a failing chunk
down to 256 B and tries that size up to 32 times, 0.2 s apart (8 tries at the
larger sizes). The designer's upload actions still send the plain 8-byte
header.

### UserMessage

`UserMessage` carries up to `64` arbitrary bytes to the running program. When a
complete message arrives, the main loop dispatches the program's
`on_user_message` callback with an array containing the bytes.

## Device To Host

| Field | Tag | Wire | Name | Payload |
| --- | --- | --- | --- | --- |
| `1` | `0x0A` | Payload | `Error` | One byte error code |
| `2` | `0x10` | Varint | `Pong` | None |
| `3` | `0x18` | Varint | `MemInfo` response | Free heap bytes as unsigned varint |
| `4` | `0x22` | Payload | `TouchCalibrate` response | 9-byte calibration record |
| `5` | `0x2A` | Payload | `StackInfo` response | `used u32 LE` + `free u32 LE` |
| `7` | `0x3A` | Payload | `FsCaps` | One flags byte (bit 0 = CRC chunks, bit 1 = sequence ACKs): ACK of a CRC `WriteFs` header |
| `8` | `0x42` | Payload | `FsAck` | 12 bytes: `next_seq u32 LE` + `last_crc u32 LE` + CRC-32 of those 8 bytes; the answer to every chunk with sequence ACKs |

`Pong` is used both as the ping response and as the ACK for `WriteFs` header and
chunk transfer (without sequence ACKs).

## Payload Layouts

### WriteFs Header

```text
offset  size  field
0       4     total_size, u32 little-endian
4       4     chunk_size, u32 little-endian
```

### StackInfo Response

```text
offset  size  field
0       4     used stack bytes, u32 little-endian
4       4     free stack bytes, u32 little-endian
```

### TouchCalibrate Response

```text
offset  size  field
0       1     flags: bit0=xy_swap, bit1=x_flip, bit2=y_flip
1       2     x_min, u16 little-endian
3       2     x_max, u16 little-endian
5       2     y_min, u16 little-endian
7       2     y_max, u16 little-endian
```

## Error Codes

| Code | Meaning |
| --- | --- |
| `1` | `page_main` not found |
| `2` | Main program not found |
| `3` | Image not found |
| `4` | Font not found |
| `5` | No filesystem in flash |
| `6` | Program execution error |
| `7` | Insufficient memory |
| `8` | Invalid system config |
| `9` | Flash image corrupted |
| `10` | `WriteFs` header/chunk CRC mismatch: resend it |

## Examples

Ping:

```text
host -> device: 08
device -> host: 10
```

Request memory info:

```text
host -> device: 38
device -> host: 18 <free-bytes varint>
```

Send a user message `01 02 03`:

```text
host -> device: 32 03 01 02 03
```

Start a filesystem upload for an image with `N` bytes:

```text
host -> device: 22 08 <N u32 LE> 00 10 00 00
device -> host: 10
host -> device: 2A <chunk length varint> <chunk bytes>
device -> host: 10
```
