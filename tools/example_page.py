#!/usr/bin/env python3
"""ferrite-ui example: main.rs'deki demo widget agacinin bytecode karsiligi.

Root (800x480, siyah)
  +- Panel (500x280, mavi, beyaz border 3px) @ (150, 100)
      +- Btn1 (100x60, kirmizi, clickable) @ (20, 20)
      +- Btn2 (100x60, yesil, clickable) @ (140, 20)

Calistirir: python example_page.py
Cikti:     example_page.bin + disassembly
"""

from ferrite_cc import Compiler, COLOR_BLACK, COLOR_WHITE, COLOR_RED, COLOR_GREEN, COLOR_BLUE

cc = Compiler()

# --- Root: tam ekran siyah ---
cc.alloc("root")             # ID 0
cc.target("root")
cc.set_prop("size", 800, 480)
cc.set_prop("bg_color", COLOR_BLACK)

# --- Panel: ortada mavi kutu, beyaz border ---
cc.alloc("panel")            # ID 1
cc.target("panel")
cc.set_prop("location", 150, 100)
cc.set_prop("size", 500, 280)
cc.set_prop("bg_color", COLOR_BLUE)
cc.set_prop("border", 3, 3, 3, 3)
cc.set_prop("border_color", COLOR_WHITE)
cc.parent("root")

# --- Btn1: kirmizi, clickable ---
cc.alloc("btn1")             # ID 2
cc.target("btn1")
cc.set_prop("location", 20, 20)
cc.set_prop("size", 100, 60)
cc.set_prop("bg_color", COLOR_RED)
cc.set_prop("clickable", 1)
cc.parent("panel")

# --- Btn2: yesil, clickable ---
cc.alloc("btn2")             # ID 3
cc.target("btn2")
cc.set_prop("location", 140, 20)
cc.set_prop("size", 100, 60)
cc.set_prop("bg_color", COLOR_GREEN)
cc.set_prop("clickable", 1)
cc.parent("panel")

# --- Render + halt ---
cc.dirty()
cc.render()
cc.halt()

# --- Output ---
nbytes = cc.save("example_page.bin", page_bg=COLOR_BLACK)
print(f"Output: example_page.bin ({nbytes} bytes, page format)")
print()
print("Disassembly:")
print(cc.disassemble())
print()
print("Hex dump:")
print(cc.hexdump())
