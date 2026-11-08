//! The Mali-400, shared by the 3D model widgets (`model3d.rs`) and the GPU compositor
//! (`compose.rs`).
//!
//! Started on first use (`Gpu::start_offscreen`, no screen of its own): its MMUs map the pool,
//! the LCD shadow (the compositor's target) and the whole heap (model buffers, sprites, the
//! flash shadow's pictures), so frames can draw into any of them and sample textures where they
//! are. Each user points the GPU at its own target before a frame (`Gpu::set_target`).
//!
//! **UI programs.** The compositor's two shader pairs (`shaders/ui_flat`, `shaders/ui_round`,
//! compiled into `compose/shaders.rs` by the HAL's `tools/shaderc`) are loaded once, then a
//! self-test frame ([`self_test`]) checks what the compositor relies on and the documentation
//! leaves open: which way round the descriptor's red/blue bit is for ARGB words and RGB565,
//! premultiplied blending, texel coordinates exact across a 1280-texel line (the fragment
//! shader is half-float), RGB565 lines whose stride is not a multiple of 64 bytes. A failure
//! leaves the compositor off, with a console line saying why.

use core::alloc::Layout;

use exynos_4412_rs::drivers::mali::{Filter, TexelFormat};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::gpu::{Area, Blend, Gpu, Shaders, Target};
use exynos_4412_rs::{heap, mmu};

use crate::compose::shaders::{UI_FLAT, UI_ROUND};
use crate::lcd;

/// What the compositor draws with: see the module's documentation.
#[derive(Clone, Copy)]
pub struct Ui {
    pub flat: Shaders,
    pub round: Shaders,
    /// Little-endian 0xAARRGGBB words (sprites, model buffers).
    pub argb: TexelFormat,
    /// RGB565, red in the top bits (pictures in the flash shadow).
    pub rgb565: TexelFormat,
    /// RGB565 textures whose stride is not a multiple of 64 bytes sample right.
    pub any_565_stride: bool,
}

pub struct Shared {
    pub gpu: Gpu,
    /// None: the compositor is off (no shaders compiled in, or the self-test failed).
    pub ui: Option<Ui>,
}

static mut SHARED: Option<Shared> = None;

/// What the GPU / compositor start said last (its console line without the prefix), for HTTP
/// `/api/status` (read by CPU3).
static STATUS: embassy_sync::blocking_mutex::CriticalSectionMutex<core::cell::RefCell<alloc::string::String>> =
    embassy_sync::blocking_mutex::CriticalSectionMutex::new(core::cell::RefCell::new(alloc::string::String::new()));

/// Where the self-test's corner check failed (for `status`).
static CORNER_DETAIL: embassy_sync::blocking_mutex::CriticalSectionMutex<core::cell::RefCell<alloc::string::String>> =
    embassy_sync::blocking_mutex::CriticalSectionMutex::new(core::cell::RefCell::new(alloc::string::String::new()));

/// Prints `ferrite: <line>` and keeps it as [`status`].
pub fn note(line: core::fmt::Arguments) {
    let text = alloc::fmt::format(line);
    exynos_4412_rs::println!("ferrite: {}", text);
    STATUS.lock(|s| *s.borrow_mut() = text);
}

/// The last GPU / compositor start message ("" before the first widget render).
pub fn status() -> alloc::string::String {
    let mut s = STATUS.lock(|s| s.borrow().clone());
    CORNER_DETAIL.lock(|d| {
        if !d.borrow().is_empty() {
            s.push_str(" -- ");
            s.push_str(&d.borrow());
        }
    });
    s
}
/// The GPU would not start: not tried again.
static mut FAILED: bool = false;

/// The GPU, started the first time; None if it cannot be (no G2D drawing, so no uncached
/// shadow, or the start failed).
pub fn get() -> Option<&'static mut Shared> {
    // SAFETY: CPU0 only (the renderer and the VM's syscalls, one main loop), never reentrantly.
    let (shared, failed) = unsafe { (&mut *(&raw mut SHARED), &mut *(&raw mut FAILED)) };
    if shared.is_none() && !*failed {
        *failed = true;
        *shared = start();
        *failed = shared.is_none();
    }
    shared.as_mut()
}

fn start() -> Option<Shared> {
    let (address, stride, width, height) = lcd::gpu_target()?;
    let target = Target { address, stride, width, height };
    // SAFETY: the shadow is drawn only through its uncached alias while G2D drawing is on, and
    // the compositor writes it with the GPU only between `lcd::gpu_begin` and `lcd::gpu_drew`.
    let mut gpu = match unsafe { Gpu::start_offscreen(Mct::new(), target) } {
        Ok(gpu) => gpu,
        Err(e) => {
            note(format_args!("GPU start failed: {:?}", e));
            return None;
        }
    };
    let (heap_start, heap_len) = heap::region();
    // SAFETY: the heap is plain memory; the GPU reads in it only textures the callers name,
    // and writes only the targets they set.
    if let Err(e) = unsafe { gpu.map(heap_start, heap_len) } {
        note(format_args!("GPU cannot map the heap: {:?}", e));
        return None;
    }
    let hz = raise_clock();
    note(format_args!("GPU started (offscreen, heap mapped, {} MHz)", hz / 1_000_000));
    let ui = ui(&mut gpu);
    Some(Shared { gpu, ui })
}

/// The GPU clock the compositor runs at: Linux's operating points (`exynos4412.dtsi`, the GPU's
/// OPP table: 160 MHz 875 mV, 267 MHz 900 mV, 350 MHz 950 mV, 440 MHz 1025 mV) allow 440 MHz at
/// 1.025 V; from the 800 MHz MPLL the divider gives 400 MHz. The HAL starts it at
/// `mali::SAFE_HZ` (160 MHz).
const FAST_HZ: u32 = 400_000_000;
const FAST_MICROVOLTS: u32 = 1_025_000;

/// Raises `vdd_g3d` (BUCK4, the DVS register the pins select) to [`FAST_MICROVOLTS`], waits for
/// the ramp, then the clock to [`FAST_HZ`]. The GPU is idle (just started). Returns the clock;
/// the safe one if the PMIC refused.
fn raise_clock() -> u32 {
    use exynos_4412_rs::drivers::i2c::{Bus, I2c};
    use exynos_4412_rs::drivers::max77686::{self, Max77686};
    let pmic = Max77686::new(I2c::new(Bus::I2c0));
    if let Err(e) = pmic.set_dvs_microvolts(4, max77686::selected_dvs(), FAST_MICROVOLTS) {
        exynos_4412_rs::println!("ferrite: GPU supply not raised ({:?}): staying at 160 MHz", e);
        return exynos_4412_rs::drivers::mali::SAFE_HZ;
    }
    // 875 -> 1025 mV at 13.75 mV/us or faster: about 11 us; wait 200.
    let mct = Mct::new();
    let begin = mct.count();
    while mct.count() - begin < 200 * 24 {
        core::hint::spin_loop();
    }
    exynos_4412_rs::drivers::cmu::Cmu::new().set_g3d_clock(FAST_HZ)
}

/// The UI programs and the self-test; None (with the reason printed) if the compositor is off.
fn ui(gpu: &mut Gpu) -> Option<Ui> {
    if !cfg!(feature = "gpu_compose") {
        return None;
    }
    if UI_FLAT.vertex.is_empty() || UI_ROUND.vertex.is_empty() {
        note(format_args!("GPU compositor off: compose/shaders.rs has no compiled shaders"));
        return None;
    }
    let loaded = gpu.shaders(&UI_FLAT).and_then(|flat| Ok((flat, gpu.shaders(&UI_ROUND)?)));
    let (flat, round) = match loaded {
        Ok(loaded) => loaded,
        Err(e) => {
            note(format_args!("GPU compositor off: shaders: {:?}", e));
            return None;
        }
    };
    let ui = Ui { flat, round, argb: TexelFormat::BGRA_BYTES, rgb565: TexelFormat::RGB_565, any_565_stride: false };
    match self_test(gpu, ui) {
        Ok(ui) => {
            note(format_args!(
                "GPU compositor on (ARGB swap {}, 565 swap {}, any 565 stride {})",
                ui.argb.swap_rb,
                ui.rgb565.swap_rb,
                ui.any_565_stride
            ));
            Some(ui)
        }
        Err(why) => {
            note(format_args!("GPU compositor off: self-test: {}", why));
            None
        }
    }
}

/// A 64-byte aligned heap buffer of `words`, zeroed and out of the data cache.
struct Buffer {
    ptr: *mut u32,
    layout: Layout,
}

impl Buffer {
    fn new(words: usize) -> Option<Self> {
        let layout = Layout::from_size_align(words * 4, 64).ok()?;
        // SAFETY: a non-zero size.
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) } as *mut u32;
        if ptr.is_null() {
            return None;
        }
        mmu::clean_dcache_range(ptr as usize, words * 4);
        Some(Self { ptr, layout })
    }

    fn addr(&self) -> usize {
        self.ptr as usize
    }

    fn words(&mut self) -> &mut [u32] {
        // SAFETY: the allocation's own words.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.layout.size() / 4) }
    }

    /// After the CPU wrote it: out to DRAM for the GPU.
    fn clean(&self) {
        mmu::clean_dcache_range(self.addr(), self.layout.size());
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout.
        unsafe { alloc::alloc::dealloc(self.ptr as *mut u8, self.layout) };
    }
}

/// Texels of the precision test's line: as wide as the screen.
const LINE: usize = 1280;
/// The test target: 16 x 16.
const SIDE: usize = 16;

/// Draws the test pattern into a 16 x 16 buffer and reads it back. Rows (two each):
/// 0 an ARGB texel (red), 2 an RGB565 texel (red), 4 texels 1264..1279 of a 1280-texel line,
/// 6 half-transparent red blended over blue, 8 line 1 of an RGB565 texture of stride 48 bytes
/// (green; line 0 is red). Returns `ui` with the swaps and the stride flag found.
/// A rounded corner of the self-test: the piece at x0..x0+8, rows 10..16, of a circle centred
/// at (cx, cy) with radius r and a border bw wide, crossing its edges inside the piece.
struct Arc {
    x0: f32,
    cx: f32,
    cy: f32,
    r: f32,
    bw: f32,
}

/// Newton's square root, for the self-test's expected values (`core` has none).
fn sqrt(v: f32) -> f32 {
    let mut x = v.max(1.0);
    for _ in 0..24 {
        x = 0.5 * (x + v / x);
    }
    x
}

const ARCS: [Arc; 2] = [
    Arc { x0: 0.0, cx: 0.0, cy: 10.0, r: 6.0, bw: 2.0 },
    Arc { x0: 8.0, cx: -62.0, cy: -60.0, r: 100.0, bw: 3.0 },
];

fn self_test(gpu: &mut Gpu, mut ui: Ui) -> Result<Ui, &'static str> {
    let target = Buffer::new(SIDE * SIDE).ok_or("no memory")?;
    let mut argb = Buffer::new(16).ok_or("no memory")?;
    let mut line = Buffer::new(LINE).ok_or("no memory")?;
    let rgb = Buffer::new(16).ok_or("no memory")?;
    argb.words()[0] = 0xFFFF_0000;
    for (i, t) in line.words().iter_mut().enumerate() {
        // The index in blue and green.
        *t = 0xFF00_0000 | i as u32;
    }
    // 565: a red texel; and a texture of 24 texels a line, stride 48: line 0 red, line 1 green.
    {
        // SAFETY: the buffer's 64 bytes, as half-words.
        let halves = unsafe { core::slice::from_raw_parts_mut(rgb.ptr as *mut u16, 32) };
        halves[0] = 0xF800;
    }
    let odd = Buffer::new(48 * 2 / 4).ok_or("no memory")?;
    {
        // SAFETY: the buffer's 96 bytes, as half-words.
        let halves = unsafe { core::slice::from_raw_parts_mut(odd.ptr as *mut u16, 48) };
        for x in 0..24 {
            halves[x] = 0xF800;
            halves[24 + x] = 0x07E0;
        }
    }
    for b in [&argb, &line, &rgb, &odd] {
        b.clean();
    }
    // The CPU does not touch the target until the frame is over: no line of it in the cache.
    mmu::discard_dcache_range(target.addr(), SIDE * SIDE * 4);
    let t = Target { address: target.addr(), stride: SIDE * 4, width: SIDE, height: SIDE };
    // SAFETY: our buffer, in the mapped heap; nothing else uses it.
    unsafe { gpu.set_target(t) }.map_err(|_| "target")?;

    let scale = [2.0 / SIDE as f32, 2.0 / SIDE as f32];
    let quad = |x0: f32, y0: f32, x1: f32, y1: f32| [[x0, y0], [x1, y0], [x0, y1], [x1, y0], [x1, y1], [x0, y1]];
    let mut frame = gpu.frame(Area::new(0, 0, SIDE, SIDE), 0xFF00_0000);
    let white = [[1.0f32; 4]; 6];
    // Textured rows: texture, its uv over the 16 pixels.
    let rows: [(usize, usize, usize, TexelFormat, [f32; 4], f32); 4] = [
        // (address, width, height, format, u0 u1 v0 v1, y)
        (argb.addr(), 16, 1, ui.argb, [0.0, 1.0 / 16.0, 0.0, 1.0], 0.0),
        (rgb.addr(), 16, 1, ui.rgb565, [0.0, 1.0 / 16.0, 0.0, 1.0], 2.0),
        (line.addr(), LINE, 1, ui.argb, [(LINE - 16) as f32 / LINE as f32, 1.0, 0.0, 1.0], 4.0),
        (odd.addr(), 24, 2, ui.rgb565, [0.0, 1.0 / 24.0, 0.5, 1.0], 8.0),
    ];
    for (address, w, h, format, [u0, u1, v0, v1], y) in rows {
        let stride = if address == odd.addr() { 48 } else { w * format.bytes as usize };
        // SAFETY: our buffers, mapped, cleaned, unchanged until `show` returns.
        let texture = match unsafe { frame.texture_at(address, w, h, stride, format, Filter::Nearest) } {
            Ok(t) => t,
            Err(_) => return Err("texture"),
        };
        let uv = [[u0, v0], [u1, v0], [u0, v1], [u1, v0], [u1, v1], [u0, v1]];
        let Ok(mesh) = frame
            .mesh(&ui.flat)
            .attribute("pos", &quad(0.0, y, SIDE as f32, y + 2.0))
            .attribute("uv", &uv)
            .attribute("color", &white)
            .build()
        else {
            return Err("mesh");
        };
        frame.draw(&mesh).uniform("scale", &scale).texture("t", &texture).blend(Blend::Premultiplied);
    }
    // Row 6: blue, then red at half alpha over it.
    for (fill, blend) in [([0.0, 0.0, 1.0, 1.0], Blend::Off), ([0.5, 0.0, 0.0, 0.5], Blend::Premultiplied)] {
        let Ok(mesh) = frame
            .mesh(&ui.round)
            .attribute("pos", &quad(0.0, 6.0, SIDE as f32, 8.0))
            .attribute("shape", &[[-256.0, -256.0, 0.0, 0.0]; 6])
            .attribute("arc", &[[0.0f32; 4]; 6])
            .attribute("fill", &[fill; 6])
            .attribute("border", &[[0.0f32; 4]; 6])
            .build()
        else {
            return Err("mesh");
        };
        frame.draw(&mesh).uniform("scale", &scale).blend(blend);
    }
    // Rows 10..16: two rounded corners, red fill and a green border, over black: a small one
    // (left half) and a large one (right half), each piece past its corner circle's centre.
    for arc in ARCS {
        let (x0, x1) = (arc.x0, arc.x0 + 8.0);
        let corners = quad(x0, 10.0, x1, 16.0);
        let mut shape = [[0.0f32; 4]; 6];
        for (s, [x, y]) in shape.iter_mut().zip(corners) {
            *s = [x - arc.cx, y - arc.cy, arc.r, arc.bw];
        }
        let Ok(mesh) = frame
            .mesh(&ui.round)
            .attribute("pos", &corners)
            .attribute("shape", &shape)
            .attribute("arc", &[crate::compose::arc(arc.r, arc.bw); 6])
            .attribute("fill", &[[1.0, 0.0, 0.0, 1.0]; 6])
            .attribute("border", &[[0.0, 1.0, 0.0, 1.0]; 6])
            .build()
        else {
            return Err("mesh");
        };
        frame.draw(&mesh).uniform("scale", &scale).blend(Blend::Premultiplied);
    }
    frame.show().map_err(|_| "frame")?;
    // Read what the GPU wrote: no stale line of it is cached (discarded above).
    let px = |x: usize, y: usize| unsafe { core::ptr::read_volatile(target.ptr.add(y * SIDE + x)) };
    let (red, blue) = (0x00FF_0000u32, 0x0000_00FFu32);
    let rgb_of = |p: u32| p & 0x00FF_FFFF;
    // Row 6 first: the blend, and that red and blue reach the target as the screen has them.
    let mixed = rgb_of(px(4, 6));
    let (r, g, b) = ((mixed >> 16) & 0xFF, (mixed >> 8) & 0xFF, mixed & 0xFF);
    if !(120..=136).contains(&r) || g > 4 || !(120..=136).contains(&b) {
        return Err(if b < 8 { "no blending (the premultiplied blend state did not take)" } else { "blend result" });
    }
    let swap_of = |p: u32| -> Result<bool, &'static str> {
        match rgb_of(p) {
            c if c == red => Ok(false),
            c if c == blue => Ok(true),
            _ => Err("texel colour"),
        }
    };
    if swap_of(px(0, 0))? {
        ui.argb = ui.argb.swapped();
    }
    if swap_of(px(0, 2))? {
        ui.rgb565 = ui.rgb565.swapped();
    }
    // Texel i holds i in blue and green; with ARGB swapped the test read it swapped too: the
    // index is in the green and red bytes then.
    for x in 0..SIDE {
        let p = px(x, 4);
        let want = (LINE - 16 + x) as u32;
        let got = if ui.argb.swap_rb == TexelFormat::BGRA_BYTES.swap_rb {
            p & 0xFFFF
        } else {
            ((p >> 16) & 0xFF) | (p & 0xFF00)
        };
        if got != want {
            return Err("texel coordinates not exact across a 1280-texel line");
        }
    }
    // The corners against their exact coverage (`ui_round`'s distance is an approximation
    // within 1/(2r) of a pixel near the edge; a broken shader is off by a whole pixel's worth).
    // Red and green are not swapped by the target (row 6 checked blue/red).
    for arc in ARCS {
        for y in 10..SIDE {
            for x in arc.x0 as usize..arc.x0 as usize + 8 {
                let (dx, dy) = (x as f32 + 0.5 - arc.cx, y as f32 + 0.5 - arc.cy);
                let len = sqrt(dx * dx + dy * dy);
                let outer = (0.5 - (len - arc.r)).clamp(0.0, 1.0);
                let inner = (0.5 - (len - (arc.r - arc.bw))).clamp(0.0, 1.0);
                let p = rgb_of(px(x, y));
                let (r, g) = (((p >> 16) & 0xFF) as f32, ((p >> 8) & 0xFF) as f32);
                if (r - inner * 255.0).abs() > 48.0 || (g - (outer - inner) * 255.0).abs() > 48.0 {
                    let mut grid = alloc::string::String::new();
                    for y in 10..SIDE {
                        for x in 0..SIDE {
                            let p = rgb_of(px(x, y));
                            grid.push_str(&alloc::format!("{:02x}{:02x} ", (p >> 16) & 0xFF, (p >> 8) & 0xFF));
                        }
                        grid.push('|');
                    }
                    exynos_4412_rs::println!(
                        "ferrite: self-test corner r {} at ({}, {}): red/green {} {}, want {} {}; rows 10..16 (red green): {}",
                        arc.r,
                        x,
                        y,
                        r as u32,
                        g as u32,
                        (inner * 255.0) as u32,
                        ((outer - inner) * 255.0) as u32,
                        grid
                    );
                    CORNER_DETAIL.lock(|d| {
                        *d.borrow_mut() = alloc::format!(
                            "r {} at ({}, {}): red/green {} {}, want {} {}; rows 10..16: {}",
                            arc.r,
                            x,
                            y,
                            r as u32,
                            g as u32,
                            (inner * 255.0) as u32,
                            ((outer - inner) * 255.0) as u32,
                            grid
                        )
                    });
                    return Err("rounded corner coverage (ui_round)");
                }
            }
        }
    }
    // Line 1 of the 48-byte stride texture: green, whatever the swap.
    ui.any_565_stride = rgb_of(px(4, 8)) == 0x00_FF00;
    drop(target);
    Ok(ui)
}
