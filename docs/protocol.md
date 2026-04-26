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
| `4` | `0x22` | Payload | `WriteFs` header | `total_size u32 LE` + `chunk_size u32 LE` |
| `5` | `0x2A` | Payload | `WriteChunk` | Flash filesystem chunk bytes |
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

`Pong` is used both as the ping response and as the ACK for `WriteFs` header and
chunk transfer.

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
