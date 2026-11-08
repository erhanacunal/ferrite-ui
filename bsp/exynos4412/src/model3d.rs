//! 3D model widgets (`KIND_MODEL3D`), drawn by the Mali-400 GPU.
//!
//! The GPU is started on the first model drawn (`Gpu::start_offscreen`): it renders into a
//! 512 x 512 ARGB8888 buffer of ours ([`TARGET_W`] x [`TARGET_H`], on the heap, never touched by
//! the CPU), and the G2D blends the widget's part of it onto the shadow once per clip rect, as
//! the renderer does with sprites. A frame covers the widget's box rounded up to 16 pixels, at
//! most `gpu::MAX_TILES` tiles of 16 x 16 (512 x 256, 352 x 352...): a bigger widget shows only
//! its background.
//!
//! A frame is run only when what the widget shows changed (its [`Model`], size or background):
//! a restore behind something in front of it just copies the buffer again. One buffer is shared
//! by every model widget, so two of them on the screen re-render each other's frames.
//!
//! A model is one of the built-in [`shape`]s or a mesh resource (`RES_MESH`, from an OBJ file
//! by `ferrite_build`) in one colour, lit by a distant light from the upper left front (the
//! `lit` shader of `gpu_cube_test`: per-vertex light, a quarter ambient). A mesh is read from
//! flash into the GPU's memory the first time it is drawn and kept until restart (the GPU's
//! `LASTING_BYTES`, about 260 000 vertices in all); one that is missing or bad is remembered
//! as such, and its widget shows only its background (error 6).
//! Programs set it through syscalls ([`syscall`], `lib/model3d.fl`), which mark the widget dirty.
//!
//! Needs G2D drawing (`g2d_render`): without it the widget shows only its background.

use alloc::vec::Vec;
use core::alloc::Layout;
use core::f32::consts::TAU;

use exynos_4412_rs::drivers::mali::Filter;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::gpu::{self, Area, Gpu, Mat4, Mesh, Target, Texture, Vec3};
use exynos_4412_rs::mmu;
use ferrite_core::clip::ClipRegion;
use ferrite_core::ctx::Ctx;
use ferrite_core::fs::RES_MESH;
use ferrite_core::pool::ObjPool;
use ferrite_core::render;
use ferrite_core::types::Rect;
use ferrite_core::widget::{KIND_MODEL3D, WidgetId};

use crate::lcd;
use crate::platform::Exynos4412Platform as P;

mod shaders;

use shaders::LIT;

/// The GPU's buffer: the largest frame, 512 tiles, fits it in either direction.
const TARGET_W: usize = 512;
const TARGET_H: usize = 512;
/// Models kept (one per widget).
const MAX_MODELS: usize = 16;
/// Mesh resources kept in the GPU's memory (or remembered as unusable).
const MAX_MESHES: usize = 16;
/// A mesh resource's name: at most 15 bytes, as the FS table has it.
const NAME_LEN: usize = 15;
/// `RES_MESH` header: magic, version, flags, triangle count (fs.rs).
const MESH_HEADER: usize = 8;
/// Bytes of a mesh vertex: position and normal, three i16 each.
const MESH_VERTEX: usize = 12;
/// The camera: `2 atan(1 / 1.6)` from bottom to top.
const FOV_Y: f32 = 1.117;
const NEAR: f32 = 1.0;
const FAR: f32 = 20.0;
/// The model's distance from the camera at zoom 100.
const DISTANCE: f32 = 3.6;
/// Towards the light, in the camera's space: up, left and in front; of length 1.
const LIGHT: Vec3 = [-0.408_248_3, 0.408_248_3, 0.816_496_6];
/// Texels of the colour texture on a side.
const TEXTURE_SIDE: usize = 4;

/// Syscall ids, duplicated in `lib/model3d.fl` and `bsp/sim/src/platform.rs`: keep in sync.
pub const SYS_SHAPE: u8 = 0x60;
pub const SYS_TURN: u8 = 0x61;
pub const SYS_COLOR: u8 = 0x62;
pub const SYS_ZOOM: u8 = 0x63;
pub const SYS_STAT: u8 = 0x64;
pub const SYS_MESH: u8 = 0x65;

/// The built-in shapes, all about 1 from the centre to the edge.
pub mod shape {
    pub const CUBE: u8 = 0;
    pub const TORUS: u8 = 1;
    pub const SPHERE: u8 = 2;
    pub const COUNT: usize = 3;
}

/// `SYS_STAT` arguments.
mod stat {
    pub const FRAMES: i32 = 0;
    pub const GP_US: i32 = 1;
    pub const PPS_US: i32 = 2;
    pub const TOTAL_US: i32 = 3;
    pub const ERROR: i32 = 4;
    pub const TRIANGLES: i32 = 5;
}

/// `SYS_STAT` `ERROR` values: the last thing that went wrong.
mod error {
    pub const START: i32 = 1;
    pub const MESH: i32 = 2;
    pub const TOO_BIG: i32 = 3;
    pub const FRAME: i32 = 4;
    pub const NO_G2D: i32 = 5;
    pub const BAD_MESH: i32 = 6;
}

/// What a model widget shows.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Model {
    id: WidgetId,
    shape: u8,
    /// Turns in tenths of a degree: about the vertical axis, the horizontal, the view axis.
    yaw: i32,
    pitch: i32,
    roll: i32,
    /// RGB565.
    color: u16,
    /// Percent: 200 = twice as large.
    zoom: i32,
    /// A mesh resource's name (`mesh_len` bytes), shown instead of `shape` when not empty.
    mesh: [u8; NAME_LEN],
    mesh_len: u8,
}

impl Model {
    fn new(id: WidgetId) -> Self {
        Self { id, shape: shape::CUBE, yaw: 0, pitch: 0, roll: 0, color: 0x4D7F, zoom: 100, mesh: [0; NAME_LEN], mesh_len: 0 }
    }

    fn mesh_name(&self) -> &[u8] {
        &self.mesh[..self.mesh_len as usize]
    }
}

/// A mesh resource read into the GPU's memory, or None if it could not be.
struct Loaded {
    name: [u8; NAME_LEN],
    len: u8,
    mesh: Option<Mesh>,
}

/// What the buffer holds: the frame of this model at this size, over this background.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Drawn {
    model: Model,
    w: u16,
    h: u16,
    bg: u32,
}

struct Gl {
    gpu: Gpu,
    target: usize,
    meshes: [Mesh; shape::COUNT],
    loaded: Vec<Loaded>,
    texture: Texture,
    texture_color: Option<u16>,
}

struct State {
    gl: Option<Gl>,
    /// The GPU would not start: not tried again.
    failed: bool,
    models: Vec<Model>,
    drawn: Option<Drawn>,
    frames: u32,
    times: gpu::Times,
    triangles: u32,
    error: i32,
}

static mut STATE: State = State {
    gl: None,
    failed: false,
    models: Vec::new(),
    drawn: None,
    frames: 0,
    times: gpu::Times { gp_us: 0, pps_us: 0, total_us: 0 },
    triangles: 0,
    error: 0,
};

fn state() -> &'static mut State {
    // SAFETY: CPU0 only (renderer and VM syscalls, the same main loop), never reentrantly.
    unsafe { &mut *(&raw mut STATE) }
}

/// Draws a `KIND_MODEL3D` widget; false for other kinds, or when it cannot (the built-in
/// drawing then shows its background).
pub fn draw(ctx: &Ctx<P>, id: WidgetId, abs: &Rect, clip: &ClipRegion) -> bool {
    let w = ctx.objs.get(id);
    if w.kind != KIND_MODEL3D {
        return false;
    }
    let s = state();
    if !lcd::g2d_draw_ready() {
        s.error = error::NO_G2D;
        return false;
    }
    let (w16, h16) = ((abs.w as usize).next_multiple_of(16), (abs.h as usize).next_multiple_of(16));
    if w16 == 0 || h16 == 0 || w16 > TARGET_W || h16 > TARGET_H || (w16 / 16) * (h16 / 16) > gpu::MAX_TILES {
        s.error = error::TOO_BIG;
        return false;
    }
    if !s.start() {
        return false;
    }
    let ext = ctx.objs.ext(id);
    let bg = lcd::rgb565_to_xrgb(render::effective_bg(&ctx.objs, w, &ext));
    let model = s.models.iter().find(|m| m.id == id).copied().unwrap_or(Model::new(id));
    let want = Drawn { model, w: abs.w, h: abs.h, bg };
    if s.drawn != Some(want) {
        s.drawn = None;
        let Some(mesh) = s.mesh_for(ctx, &model) else {
            s.error = error::BAD_MESH;
            return false;
        };
        if !s.render(&model, &mesh, w16, h16, abs.w as usize, abs.h as usize, bg) {
            return false;
        }
        s.drawn = Some(want);
    }
    let target = s.gl.as_ref().map_or(0, |gl| gl.target) as *const u32;
    let screen = Rect::new(0, 0, ctx.lcd.width(), ctx.lcd.height());
    for cr in clip.iter() {
        let Some(r) = cr.intersection(abs).and_then(|r| r.intersection(&screen)) else { continue };
        let (sx, sy) = ((r.x - abs.x) as usize, (r.y - abs.y) as usize);
        if !lcd::blit_sprite(target, TARGET_W, TARGET_H, TARGET_W, sx, sy, r.x as usize, r.y as usize, r.w as usize, r.h as usize) {
            // The G2D gave up (it stays off): the background for this rect.
            render::draw_default(ctx, id, abs, &ClipRegion::from_rect(r));
        }
    }
    true
}

impl State {
    /// The GPU, its buffer, the shapes and the colour texture, the first time.
    fn start(&mut self) -> bool {
        if self.gl.is_some() {
            return true;
        }
        if self.failed {
            return false;
        }
        self.failed = true;
        let bytes = TARGET_W * TARGET_H * 4;
        let Ok(layout) = Layout::from_size_align(bytes, 64) else { return false };
        // SAFETY: a non-zero size. Never freed: the GPU keeps the buffer mapped.
        let target = unsafe { alloc::alloc::alloc(layout) } as usize;
        if target == 0 {
            self.error = error::START;
            exynos_4412_rs::println!("ferrite: no memory for the GPU's buffer");
            return false;
        }
        // The GPU writes it past the caches: no line of it may be written back later.
        mmu::discard_dcache_range(target, bytes);
        let t = Target { address: target, stride: TARGET_W * 4, width: TARGET_W, height: TARGET_H };
        // SAFETY: the buffer is ours for good, and only the G2D reads it.
        let mut gpu = match unsafe { Gpu::start_offscreen(Mct::new(), t) } {
            Ok(gpu) => gpu,
            Err(e) => {
                self.error = error::START;
                exynos_4412_rs::println!("ferrite: GPU start failed: {:?}", e);
                return false;
            }
        };
        let made = (|| -> Result<_, gpu::Error> {
            let texture = gpu.texture_with(TEXTURE_SIDE, TEXTURE_SIDE, Filter::Nearest, |_, _| 0xFFFF_FFFF)?;
            let meshes = [build(&mut gpu, cube())?, build(&mut gpu, torus(32, 16))?, build(&mut gpu, sphere(24, 16))?];
            Ok((texture, meshes))
        })();
        let (texture, meshes) = match made {
            Ok(made) => made,
            Err(e) => {
                self.error = error::MESH;
                exynos_4412_rs::println!("ferrite: GPU meshes failed: {:?}", e);
                return false;
            }
        };
        exynos_4412_rs::println!("ferrite: GPU started (offscreen {}x{})", TARGET_W, TARGET_H);
        self.failed = false;
        self.gl = Some(Gl { gpu, target, meshes, loaded: Vec::new(), texture, texture_color: None });
        true
    }

    /// The mesh `model` shows: a built-in shape, or its mesh resource, read the first time.
    fn mesh_for(&mut self, ctx: &Ctx<P>, model: &Model) -> Option<Mesh> {
        let gl = self.gl.as_mut()?;
        let name = model.mesh_name();
        if name.is_empty() {
            return Some(gl.meshes[(model.shape as usize).min(shape::COUNT - 1)]);
        }
        if let Some(l) = gl.loaded.iter().find(|l| &l.name[..l.len as usize] == name) {
            return l.mesh;
        }
        let mesh = load_mesh(&mut gl.gpu, ctx, name);
        if mesh.is_none() {
            exynos_4412_rs::println!("ferrite: mesh {:?} missing or bad", core::str::from_utf8(name).unwrap_or("?"));
        }
        if gl.loaded.len() < MAX_MESHES {
            gl.loaded.push(Loaded { name: model.mesh, len: model.mesh_len, mesh });
        }
        mesh
    }

    /// Runs a frame of `mesh` as `model` shows it into the buffer's top left `w16` x `h16`, the
    /// model centred in its top left `w` x `h` (the widget's box).
    #[allow(clippy::too_many_arguments)]
    fn render(&mut self, model: &Model, mesh: &Mesh, w16: usize, h16: usize, w: usize, h: usize, bg: u32) -> bool {
        let Some(gl) = self.gl.as_mut() else { return false };
        if gl.texture_color != Some(model.color) {
            let argb = lcd::rgb565_to_xrgb(model.color) | 0xFF00_0000;
            gl.gpu.write_texture(&gl.texture, |_, _| argb);
            gl.texture_color = Some(model.color);
        }
        // The perspective over the widget's box, then squeezed into its part of the frame:
        // scaled by w / w16, h / h16 and moved to the frame's top left (clip space, so the
        // move is in NDC: +1 is the frame's right and top edges).
        let (fx, fy) = (w as f32 / w16 as f32, h as f32 / h16 as f32);
        let projection = Mat4::translation(fx - 1.0, 1.0 - fy, 0.0)
            * Mat4::scale(fx, fy, 1.0)
            * Mat4::perspective(FOV_Y, w as f32 / h as f32, NEAR, FAR);
        let angle = |tenths: i32| (tenths % 3600) as f32 * (TAU / 3600.0);
        let distance = DISTANCE * 100.0 / model.zoom.clamp(10, 1000) as f32;
        let world = Mat4::translation(0.0, 0.0, -distance)
            * Mat4::rotation_x(angle(model.pitch))
            * Mat4::rotation_y(angle(model.yaw))
            * Mat4::rotation_z(angle(model.roll));
        let light = world.transform_direction_back(LIGHT);
        let mut frame = gl.gpu.frame(Area::new(0, 0, w16, h16), bg | 0xFF00_0000);
        frame
            .draw(mesh)
            .uniform("mvp", &(projection * world))
            .uniform("light", &light)
            .texture("t", &gl.texture)
            .depth_test(true);
        match frame.show() {
            Ok(times) => {
                self.frames = self.frames.wrapping_add(1);
                self.times = times;
                self.triangles = mesh.vertices() / 3;
                true
            }
            Err(e) => {
                if self.error != error::FRAME {
                    exynos_4412_rs::println!("ferrite: GPU frame failed: {:?}", e);
                }
                self.error = error::FRAME;
                false
            }
        }
    }

    /// The model of widget `id`, made if new (forgetting those of widgets that are no longer
    /// models). None if the table is full.
    fn model(&mut self, objs: &ObjPool, id: WidgetId) -> Option<&mut Model> {
        if let Some(at) = self.models.iter().position(|m| m.id == id) {
            return Some(&mut self.models[at]);
        }
        self.models.retain(|m| objs.get(m.id).kind == KIND_MODEL3D);
        if self.models.len() >= MAX_MODELS {
            return None;
        }
        self.models.push(Model::new(id));
        self.models.last_mut()
    }
}

/// The model syscalls ([`SYS_SHAPE`]..[`SYS_STAT`]); None for other ids.
///
/// - `SYS_SHAPE (w, shape)`, `SYS_TURN (w, yaw, pitch, roll)` (tenths of a degree),
///   `SYS_COLOR (w, rgb565)`, `SYS_ZOOM (w, percent)`: 1, or 0 if `w` is not a model widget
///   (or too many are);
/// - `SYS_MESH (w, name)`: the mesh resource `name` instead of a shape (1, or 0 for a bad
///   name; a missing mesh shows when drawn, as error 6);
/// - `SYS_STAT (what)`: frames run, the last frame's GP / PP / total microseconds, the last
///   error, the last mesh's triangles.
pub fn syscall(id: u8, args: &[i32], objs: &mut ObjPool) -> Option<i32> {
    let arg = |i: usize| args.get(i).copied().unwrap_or(0);
    let s = state();
    if id == SYS_STAT {
        let t = &s.times;
        return Some(match arg(0) {
            stat::FRAMES => s.frames as i32,
            stat::GP_US => t.gp_us as i32,
            stat::PPS_US => t.pps_us as i32,
            stat::TOTAL_US => t.total_us as i32,
            stat::ERROR => s.error,
            stat::TRIANGLES => s.triangles as i32,
            _ => 0,
        });
    }
    if !matches!(id, SYS_SHAPE | SYS_TURN | SYS_COLOR | SYS_ZOOM | SYS_MESH) {
        return None;
    }
    let w = WidgetId::from_vm(arg(0));
    if objs.get(w).kind != KIND_MODEL3D {
        return Some(0);
    }
    let Some(m) = s.model(objs, w) else { return Some(0) };
    let before = *m;
    match id {
        SYS_SHAPE => {
            m.shape = (arg(1).clamp(0, shape::COUNT as i32 - 1)) as u8;
            m.mesh_len = 0;
        }
        SYS_MESH => {
            let name = match u32::try_from(arg(1)) {
                Ok(p) => objs.str_get(p as _),
                Err(_) => &[],
            };
            if name.is_empty() || name.len() > NAME_LEN {
                return Some(0);
            }
            m.mesh = [0; NAME_LEN];
            m.mesh[..name.len()].copy_from_slice(name);
            m.mesh_len = name.len() as u8;
        }
        SYS_TURN => (m.yaw, m.pitch, m.roll) = (arg(1).rem_euclid(3600), arg(2).rem_euclid(3600), arg(3).rem_euclid(3600)),
        SYS_COLOR => m.color = arg(1) as u16,
        _ => m.zoom = arg(1).clamp(10, 1000),
    }
    if *m != before {
        objs.mark_dirty(w);
    }
    Some(1)
}

/// Triangles as the `lit` shader takes them: positions, texture coordinates (all the colour
/// texture's middle) and normals.
struct Tris {
    pos: Vec<[f32; 3]>,
    normal: Vec<[f32; 3]>,
}

impl Tris {
    fn new() -> Self {
        Self { pos: Vec::new(), normal: Vec::new() }
    }

    fn vertex(&mut self, p: Vec3, n: Vec3) {
        self.pos.push(p);
        self.normal.push(n);
    }
}

/// Mesh resource `name` into the GPU's memory; None if missing, malformed or too large.
fn load_mesh(gpu: &mut Gpu, ctx: &Ctx<P>, name: &[u8]) -> Option<Mesh> {
    let fs = ctx.fs.as_ref()?;
    let entry = fs.find_kind(&ctx.flash, name, RES_MESH)?;
    let mut head = [0u8; MESH_HEADER];
    fs.read_resource(&ctx.flash, &entry, 0, &mut head);
    let tris = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
    let vertices = tris.checked_mul(3)?;
    let bytes = vertices.checked_mul(MESH_VERTEX)?;
    if head[..3] != [b'M', b'S', 1] || tris == 0 || (entry.size as usize) < MESH_HEADER + bytes {
        return None;
    }
    let mut raw: Vec<u8> = Vec::new();
    raw.try_reserve_exact(bytes).ok()?;
    raw.resize(bytes, 0);
    fs.read_resource(&ctx.flash, &entry, MESH_HEADER as u32, &mut raw);
    let mut t = Tris::new();
    t.pos.try_reserve_exact(vertices).ok()?;
    t.normal.try_reserve_exact(vertices).ok()?;
    const ONE: f32 = 1.0 / 32767.0;
    for v in raw.chunks_exact(MESH_VERTEX) {
        let c = |i: usize| i16::from_le_bytes([v[i * 2], v[i * 2 + 1]]) as f32 * ONE;
        t.vertex([c(0), c(1), c(2)], [c(3), c(4), c(5)]);
    }
    drop(raw);
    build(gpu, t).ok()
}

fn build(gpu: &mut Gpu, t: Tris) -> Result<Mesh, gpu::Error> {
    let uv = alloc::vec![[0.5f32, 0.5]; t.pos.len()];
    gpu.mesh(&LIT).attribute("pos", &t.pos).attribute("uv", &uv).attribute("normal", &t.normal).build()
}

/// A cube of half side 0.8, flat faces.
fn cube() -> Tris {
    const H: f32 = 0.8;
    // Each face: its normal and two axes along it.
    let faces: [(Vec3, Vec3, Vec3); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
    ];
    let mut t = Tris::new();
    for (n, u, v) in faces {
        let corner = |a: f32, b: f32| -> Vec3 { core::array::from_fn(|i| H * (n[i] + a * u[i] + b * v[i])) };
        let c = [corner(-1.0, -1.0), corner(1.0, -1.0), corner(1.0, 1.0), corner(-1.0, 1.0)];
        for k in [0, 1, 2, 0, 2, 3] {
            t.vertex(c[k], n);
        }
    }
    t
}

/// A torus about the y axis: ring radius 0.7, tube radius 0.32, smooth.
fn torus(around: usize, tube: usize) -> Tris {
    let (big, small) = (0.7f32, 0.32f32);
    let point = |i: usize, j: usize| -> (Vec3, Vec3) {
        let (sa, ca) = gpu::sin_cos(i as f32 * TAU / around as f32);
        let (sb, cb) = gpu::sin_cos(j as f32 * TAU / tube as f32);
        let n = [ca * cb, sb, sa * cb];
        let r = big + small * cb;
        ([ca * r, small * sb, sa * r], n)
    };
    grid(around, tube, point)
}

/// A sphere of radius 1, smooth.
fn sphere(around: usize, rings: usize) -> Tris {
    let point = |i: usize, j: usize| -> (Vec3, Vec3) {
        let (sa, ca) = gpu::sin_cos(i as f32 * TAU / around as f32);
        let (sb, cb) = gpu::sin_cos(j as f32 * (TAU / 2.0) / rings as f32 - TAU / 4.0);
        let n = [ca * cb, sb, sa * cb];
        (n, n)
    };
    grid(around, rings, point)
}

/// Two triangles for each cell of a `cols` x `rows` grid of `point(i, j)`s (position, normal),
/// `i` wrapping around.
fn grid(cols: usize, rows: usize, point: impl Fn(usize, usize) -> (Vec3, Vec3)) -> Tris {
    let mut t = Tris::new();
    for i in 0..cols {
        for j in 0..rows {
            let q = [point(i, j), point(i + 1, j), point(i + 1, j + 1), point(i, j + 1)];
            for k in [0, 1, 2, 0, 2, 3] {
                t.vertex(q[k].0, q[k].1);
            }
        }
    }
    t
}
