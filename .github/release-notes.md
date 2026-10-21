## Downloads

| File | What it is |
|------|------------|
| `ferrite-gd32-nextion.bin` | Firmware for GD32F103 Nextion boards |
| `ferrite-n32903.bin` | Firmware for N32903 Nextion boards (Ai HMI T8) |
| `fs-gd32-<example>.bin` / `fs-n32903-<example>.bin` | Ready-to-flash filesystem image of each example |
| `ferrite_cli.py` | USART tool: `writefs`, `meminfo`, `settime`, `calibrate`, … (needs `pyserial`) |
| `flash_tool.py` | N32903 NAND loader tool |
| `SHA256SUMS` | Checksums of all files |

## Flashing

**GD32 Nextion** (ST-Link on the SWD pins):

```
st-flash write ferrite-gd32-nextion.bin 0x08000000
python ferrite_cli.py -p COM3 writefs fs-gd32-widget_demo.bin
```

**N32903** (the NAND loader must already be on the board, see `N32903_Flashing.md`):

```
python flash_tool.py COM10 writeapp ferrite-n32903.bin --boot
python ferrite_cli.py -p COM10 writefs fs-n32903-widget_demo.bin
```

To build your own projects, see the README.
