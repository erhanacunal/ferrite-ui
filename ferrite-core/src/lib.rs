#![cfg_attr(not(feature = "host"), no_std)]

extern crate alloc;

#[cfg(feature = "host")]
extern crate std;

// --- HAL backend traits + generic wrappers + Platform ---

pub mod backlight;
pub mod flash;
pub mod gfx_glyph;
pub mod lcd;
#[cfg(feature = "mock")]
pub mod mock;
pub mod platform;
pub mod rtc;
pub mod sdcard;
pub mod systick;
pub mod touch;
pub mod usart;

// --- Framework modules ---

pub mod clip;
pub mod config;
pub mod ctx;
pub mod embedded_font;
pub mod font;
pub mod fs;
pub mod heap;
pub mod image;
pub mod keyboard;
pub mod proto;
pub mod protocol;
pub mod render;
pub mod runtime;
pub mod strpool;
pub mod types;
pub mod vm;
pub mod widget;

// --- Mock monomorphization guard ---
//
// Forces the compiler to instantiate the generic framework against
// `crate::mock::MockPlatform`, so `cargo check --features mock` verifies
// trait-bound resolution for a concrete platform — not just definition-time
// well-formedness. Has no runtime role.
#[cfg(feature = "mock")]
#[allow(dead_code)]
fn _mock_instantiation_guard() {
    use crate::mock::{MockFlash, MockLcd, MockPlatform as M};
    let _: fn(&keyboard::Keyboard, &lcd::Lcd, &font::FontList, &flash::Flash) =
        keyboard::Keyboard::draw::<MockLcd, MockFlash>;
    let _: fn(&mut ctx::Ctx<M>, &vm::Vm) = render::render_dirty::<M>;
    let _: fn(&mut ctx::Ctx<M>, &vm::Vm) = render::render_all::<M>;
    let _: fn(&mut ctx::Ctx<M>, &vm::Vm) = render::render_buffered::<M>;
    let _: fn(&mut ctx::Ctx<M>) -> bool = render::buffered_has_dirty::<M>;
    let _: fn(&mut vm::Vm, &mut ctx::Ctx<M>) = vm::Vm::run::<M>;
    let _: fn(&mut vm::Vm, &mut ctx::Ctx<M>) = vm::Vm::step::<M>;
    let _: fn(&mut vm::Vm, &mut ctx::Ctx<M>) = vm::Vm::drain_callbacks::<M>;
    let _: fn(&mut vm::Vm, &mut ctx::Ctx<M>) = vm::Vm::try_resume_modal::<M>;

    // protocol.rs: feed (flash-generic) + send_* (usart-generic).
    let _: fn(&mut protocol::Protocol, u8, &flash::Flash) -> protocol::RxEvent =
        protocol::Protocol::feed;
    let _: fn(&usart::Usart, u8) = protocol::send_error;
    let _: fn(&usart::Usart) = protocol::send_pong;
    let _: fn(&usart::Usart, u32) = protocol::send_meminfo;
    let _: fn(&usart::Usart, u32, u32) = protocol::send_stackinfo;
    let _: fn(&usart::Usart, &touch::CalParams) = protocol::send_touch_cal;
}
