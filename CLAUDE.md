# CLAUDE.md — ferrite-ui

Nextion NX8048K070 ekranı için Rust ile yazılmış bare-metal HMI framework.

## Donanım

- **CPU:** GD32F103RBT6 (Cortex-M3, 108MHz, 128KB Flash, 20KB RAM)
- **Ekran:** NX8048K070 (800x480)
- **FPGA:** Display controller — CPU'dan bağımsız LCD'yi tazeliyor
  - 2MB frame buffer FPGA'da, CPU'da frame buffer yok
  - Double buffer: CMD 4/5 ile swap, tearing yok
- **Touch:** XPT2046 (SPI)
- **Flash:** W25Q256JVFQ 32MB (SPI) — UI bytecode, font, görseller burada
- **RTC:** AT8563T (I2C)

## FPGA Protokolü 

- `GPIOB[15:0]` = 16-bit data bus
- `PA15`: 1=data, 0=command (LCD_CMD_DATA)
- `PA12`: clock (BC=falling edge, BOP=rising edge)
- `spin(3)` gecikme zorunlu — 108MHz CPU FPGA'yı geçiyor

| CMD  | İşlev               | Data          |
|------|---------------------|---------------|
| 0x02 | Y başlangıç (y1)    | uint16 piksel |
| 0x03 | X başlangıç (x1)    | uint16 piksel |
| 0x06 | Y bitiş (y2)        | uint16 piksel |
| 0x07 | X bitiş (x2)        | uint16 piksel |
| 0x0F | Piksel write başlat | sonraki data'lar piksel rengi |
| 0x04 | Front buffer swap   | lcd4Value = lcd5Value |
| 0x05 | Back buffer seç     | 0 veya 1      |

### Double Buffer Akışı

```
begin_frame() → CMD5 (back buffer toggle)
  → set_address() + piksel data (back buffer'a yaz)
end_frame()   → CMD4 (front ← back, FPGA swap)
```

`lcd4Value` = FPGA'nın gösterdiği (front), `lcd5Value` = yazılan (back).
`lcd4 == lcd5` iken `begin_frame` çağrılmaz — zaten fresh buffer var.

## Mimari Kararlar

### Rust no_std
- `#![no_std]` + `#![no_main]`
- Rust 2024 edition (`edition = "2024"` — Rust 1.85 ile stable)
- `cortex-m-rt` crate — startup, interrupt table, `#[entry]` macro
- `panic-halt` — panic → sonsuz döngü
- Newlib yok, hidden runtime yok, RAM tamamen kullanıcıya ait
- Target: `thumbv7m-none-eabi`

### Widget Sistemi
- **HTML-benzeri iç içe geçme** — widget içinde widget, ağaç yapısı
- **Arena allocator** — heap yok, `MAX_WIDGETS = 64`, statik bellek
- **Ağaç yapısı:** left-child right-sibling (parent + first_child + next_sibling)
- **WidgetId:** `u8` index, `0xFF` = NONE sentinel
- **Kutu modeli (CSS border-box benzeri):**
  - `margin` → dış boşluk (size'a dahil değil)
  - `border` → kenar çizgisi (size'a dahil)
  - `padding` → iç boşluk (size'a dahil)
  - `location` → parent content area'ya göreceli offset
  - `size` → border box boyutu
- **Flags:** `VISIBLE` (0x01), `ENABLED` (0x02), `CLICKABLE` (0x04), `DIRTY` (0x08), `PRESSED` (0x10), `CHECKED` (0x20)
- **Widget tipleri:** `KIND_BASE` (0, container), `KIND_LABEL` (1, metin), `KIND_BUTTON` (2, tıklanabilir container), `KIND_PROGRESS` (3, ilerleme çubuğu), `KIND_SLIDER` (4, kaydırıcı), `KIND_CHECKBOX` (5, onay kutusu), `KIND_RADIO` (6, radyo düğmesi)
- **Label:** text_color, font_id, text_align (LEFT/CENTER/RIGHT), text (text_pool'dan)
- **Button:** press_color (basılıyken arka plan), child widget kabul eder
- **Text pool:** WidgetTree'de 256 byte append-only buffer, label metinleri burada
- **Renk:** RGB565 (`background_color`, `border_color`, `text_color`, `press_color`)
- **Painter's algorithm** — z-order: DFS pre-order (düşük index = altta)
- **Clip region** (ReactOS Region API'sinden ilham)
  - Statik rect pool: `MAX_CLIP_RECTS = 32`
  - Her `subtract` işlemi max 4 yeni rect üretir (üst/alt/sol/sağ şerit)
  - Pool doluysa fallback: dirty rect'in tamamını çiz (overdraw, tearing yok)
- Double buffer şu an devre dışı — LCD direkt front buffer'a yazıyor
- **Dirty redraw akışı:**
  1. `mark_dirty(id)` → widget + tüm alt ağacı dirty işaretler
  2. `render_dirty()` → DFS order hesaplar
  3. Her dirty widget için occluder'ları toplar (DFS'te sonraki, soyundan olmayan)
  4. ClipRegion'dan occluder rect'leri subtract eder
  5. Kalan görünür rect'ler üzerinden widget çizilir
  6. Alt widget'lar aynı occluder listesiyle recursive çizilir
- **İki render modu:**
  - `render_all()` — tam ekran, DFS pre-order, clip yok (ilk açılış)
  - `render_dirty()` — iteratif (recursive değil), DFS cache kullanır, clip'li (partial update)
  - **DFS cache:** `WidgetTree.dfs_cache` — tree değişmedikçe yeniden hesaplanmaz (alloc/add_child/clear invalidate eder)

### Render
- `fill_rect` → `set_address` + burst piksel yazma (FPGA'da donanım hızı)
- Frame buffer CPU RAM'inde yok — doğrudan FPGA'ya yazılıyor
- Partial update: sadece dirty widget'lar redraw edilir

### Bytecode Interpreter (VM)
- **Protobuf tag encoding:** `tag = (opcode << 3) | wire_type`
- **Wire types:** 0=varint, 1=i16 fixed (2B LE), 2=LEN (varint len + payload), 5=no-arg
- **ZigZag varint:** signed integer encoding (protobuf uyumlu)
- **37 opcode:** stack ops (PUSH/POP/DUP/SWAP), aritmetik (ADD/SUB/MUL/DIV/MOD/NEG), karşılaştırma (EQ/NE/LT/LE/GT/GE), mantık (AND/OR/NOT), kontrol (JMP/JZ/JNZ/CALL/RET/YIELD/HALT), widget (W_TARGET/W_SET/W_GET/W_DIRTY/W_RENDER/W_ALLOC/W_PARENT), flash (F_READ/F_WRITE)
- **Opcode 0–15:** 1-byte tag (sık kullanılan), **16+:** 2-byte tag (nadir)
- **W_ALLTAR opcode (0x1C):** combined alloc + store + target (saves 5 bytes per widget)
- **Vm struct:** eval stack (16-deep), vars (sparse Vec, max 256), call stack (8-deep)
- **Property R/W:** scalar (W_SET wt=0, tek değer stack'ten) ve compound (W_SET wt=2, LEN payload ile çoklu zigzag varint)
- **Builder:** RAM'de bytecode oluşturma, forward jump patching, `&mut [u8]` buffer'a yazar
- **Çalıştırma:** `vm.run(&code[..len], &mut tree, &mut lcd, &flash)` — F_READ/F_WRITE flash üzerinden çalışır
- **Kontrol akışı:** if/while/for — JZ/JNZ/JMP kombinasyonları ile

### Harici Flash (W25Q256, 32MB)
- **Pin ataması:** PA4=CS, PA5=CLK, PA6=MISO, PA7=MOSI (bit-bang SPI)
- **4-byte address mode:** init'te 0xB7 ile aktif (32MB tam erişim)
- **API:** `read(addr, buf)`, `write(addr, data)`, `erase_sector(addr)`, `read_id()`
- `write()` sayfa sınırlarını otomatik böler (256B page program)
- `erase_sector()` ve `page_program()` busy wait ile bekler

### Flash Dosya Sistemi (Fs)
- **Basit TOC yapısı** — resource'lara isimle erişim
- **Layout:**
  - `0x000000 - 0x000FFF`: Reserved (4KB = 1 sector, erase guard)
  - `0x001000 - 0x00100F`: Header (16B: magic "FERR" + version + screen W/H + resource count + checksum)
  - `0x001010 - 0x001FFF`: Resource Table (max 127 entry × 32B)
  - `0x002000+`: Resource data (packed)
- **Entry format (32B):** name[16] + kind(1) + pad(3) + offset(4) + size(4) + reserved(4)
- **Resource tipleri:** Font=0, Image=1, Program=2, Page=3
- **API:** `mount()`, `find(name)`, `read_resource()`, `count_by_kind()`, `find_nth_by_kind()`, `verify_checksum()`
- **RAM maliyeti:** 12 byte (sadece header cache — tablo flash'ta kalır)

### Recovery Mode
- **Boot'ta sol üst köşeye 3 saniye basılı tutma** → recovery mode
- Kırmızı progress bar dolarak gösterir, bırakırsan iptal
- Recovery modda: program yüklenmez, sadece USART aktif
- `writefs` ile yeni program flash'lanabilir
- **PENIRQ (PC14):** GPIO polling ile dokunma algılama (EXTI interrupt KULLANILMAZ — SPI çakışması)
- **RAM maliyeti:** ~18 byte (8 × WidgetId + count + active)

### Font Renderer (Adafruit GFX uyumlu)
- **Format:** Adafruit GFX bitmap font — iki ayrı flash resource:
  - Header: font meta (first/last/yAdvance) + glyph tablosu (7B/glyph)
  - Data: 1-bit packed bitmap (MSB first, satır padding'i yok)
- **GfxGlyph (7B):** bitmapOffset(u16) + width + height + xAdvance + xOffset(i8) + yOffset(i8)
- **Yükleme:** `Font::load(fs, flash, header_name, data_name)` — header tamamı RAM'e, bitmap flash'ta kalır
- **Çizim modları:**
  - Opaque: `begin_pixels` + stream (hızlı, fg+bg)
  - Transparent: sadece fg pikselleri `fill_rect(1,1)` ile (yavaş, arka plan korunur)
- **API:** `draw_char()`, `draw_str()`, `char_width()`, `text_width()`, `line_height()`
- **RAM maliyeti:** ~900 byte (128 glyph × 7B + meta) — font başına
- **Max glyph:** 128 (MAX_GLYPHS), bitmap okuma 128B chunk'larla

## Proje Adı

**ferrite-ui** — ticari marka sorunu yok.
Nextion, ITEAD'ın tescilli markası — bu proje tamamen bağımsız, clean-room implementation.

## Dosya Yapısı

```
ferrite-ui/
├── .cargo/config.toml  — thumbv7m-none-eabi target + linker flags
├── Cargo.toml          — cortex-m, cortex-m-rt (device feature), panic-halt
├── memory.x            — GD32F103RBT6 linker script (128K Flash, 20K RAM)
├── device.x            — Interrupt vector tanımları (USART0)
├── build.rs            — device.x → linker search path
└── src/
    ├── main.rs         — entry point, startup sequence, USART command loop
    ├── gpio.rs         — GPIOA/B init, 16-bit data bus, clock pulse
    ├── lcd.rs          — FPGA protokolü, fill_rect, begin_pixels/write_pixel
    ├── types.rs        — Rect, Offset, Size, Edges, Color (RGB565)
    ├── widget.rs       — Widget struct (7 kind), WidgetId, WidgetTree (DFS cache)
    ├── clip.rs         — ClipRegion (32 rect pool, subtract algoritması)
    ├── flash.rs        — W25Q256 SPI flash driver (donanım SPI0, 4-byte addr)
    ├── font.rs         — Adafruit GFX bitmap font renderer (flash + embedded)
    ├── embedded_font.rs — Gömülü FreeMono9pt7b font verisi (ROM'da)
    ├── fs.rs           — Flash dosya sistemi (TOC, isimle resource erişimi)
    ├── image.rs        — Ferrite Image (FI) format decoder (raw/rle/indexed+rle)
    ├── render.rs       — render_all + render_dirty (iteratif, painter's algorithm)
    ├── touch.rs        — XPT2046 SPI bit-bang, hit test, debounce, PENIRQ GPIO, recovery
    ├── sdcard.rs       — SD card SPI driver (SPI0 shared, Mode 0)
    ├── fat.rs          — FAT16/32 filesystem reader
    ├── vm.rs           — Bytecode interpreter (57+ opcode, sparse vars, W_ALLTAR)
    ├── backlight.rs    — LCD arka ışık PWM (TIMER0_CH0, PA8)
    ├── usart.rs        — USART0 serial + RX interrupt ring buffer
    ├── irq.rs          — GD32F103 interrupt vector table (__INTERRUPTS)
    └── protocol.rs     — USART protobuf protokolü (ping/pong, execute, restart, fs write)
```

## Bellek Kullanımı

- Widget arena: 64 × ~48 byte = ~3.0KB
- Text pool: 256 byte (label metinleri, append-only)
- Clip region: 32 × 8 byte = 256 byte
- VM: ~150 byte (stack + vars + call stack + array pool)
- Fs header: 12 byte
- Font (per font): ~900 byte (128 glyph × 7B + meta)
- Toplam statik (1 font): ~4.6KB (20KB RAM'in %23'ü)
- Binary: ~12.2KB Flash (128KB'nin %9.6'sı)

## Mevcut Durum

- [x] FPGA protokolü çözüldü (Ghidra reverse engineering)
- [x] LCD sürücüsü çalışıyor (kare çizdirme test edildi)
- [x] Double buffer mekanizması anlaşıldı
- [x] Rust no_std iskelet kurulumu
- [x] GPIO sürücüsü Rust'a port
- [x] Clip region implementasyonu
- [x] Widget sistemi (temel: iç içe widget, border, margin, padding, dirty redraw)
- [x] XPT2046 touch driver (SPI bit-bang, Z-pressure, median filtre, debounce)
- [x] Bytecode interpreter (37 opcode, protobuf tag, varint/zigzag, property R/W, Builder)
- [x] Flash driver (W25Q256 SPI bit-bang, 4-byte addr, read/write/erase, VM entegrasyonu)
- [x] Flash dosya sistemi (TOC, isimle resource erişimi, mount/find/read)
- [x] Font render (Adafruit GFX uyumlu, header RAM'de, bitmap flash'tan okunur)
- [x] Widget tipleri (Label, Button, Progress, Slider, Checkbox, Radio)
- [x] Image format (FI: raw/rle/indexed+rle, streaming decode, Python converter)
- [x] Backlight PWM (TIMER0_CH0, PA8, 10kHz, 0-100%)
- [x] USART0 RX interrupt + 128B ring buffer
- [x] Interrupt vector table (device.x + irq.rs)
- [x] Gömülü font (FreeMono9pt7b, ROM'da, flash gerekmez)
- [x] USART protobuf protokolü (ping/pong, execute, restart, fs write, meminfo, stackinfo)
- [x] Startup sequence (backlight → ekran → recovery check → font → fs → vm)
- [x] Hata protokolü (ekran + USART, 7 hata kodu)
- [x] Touch event → VM callback (on_click, on_tap, on_paint, on_touch_down/up/move)
- [x] Iteratif render (recursive yerine flat DFS, DFS cache)
- [x] PENIRQ GPIO polling (idle'da SPI atla)
- [x] SPI timing sabitleri (SPI_HALF_CLK=54, ~1MHz)
- [x] Recovery mode (boot'ta sol üst köşe 3sn basılı → USART-only mod)
- [x] @func_name syntax (compiler callback referansları)
- [x] W_ALLTAR opcode (alloc+target+store combined)
- [x] Sparse variable map (Vec<VmVar>, max 256, 32 limit kaldırıldı)
- [ ] SD card boot (SPI0 bus paylaşım sorunu çözülmeli)
