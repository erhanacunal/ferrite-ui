//! Nextion BSP — GD32F103RBT6 (Cortex-M3) + NX8048K070 display.
//!
//! This binary owns only device bring-up (clock tree, GPIO ports, peripheral
//! construction); the entire application loop lives in `ferrite_core::run`.
//!
//! Build:  cargo build -p bsp-nextion --release

#![no_std]
#![no_main]
#![allow(dead_code)]

extern crate alloc;

use alloc::boxed::Box;
use cortex_m_rt::entry;

use ferrite_core::ctx::Ctx;
use ferrite_core::font::FontList;
use ferrite_core::heap;
use ferrite_core::image::ImageList;
use ferrite_core::strpool::StringPool;
use ferrite_core::widget::WidgetTree;

// --- Concrete hardware backends (type aliases + free-fn constructors) ---
mod backlight;
mod flash;
mod lcd;
mod rtc;
mod sdcard;
mod systick;
mod touch;
mod usart;

// --- Device-only modules (binary-local) ---
mod gpio;
mod irq;
mod fat;
mod panic;
mod platform;

use gpio::Gpio;
use platform::NextionPlatform;

// === Hardware addresses (port init) ===

const RCU_APB2EN: u32 = 0x4002_1018;
const RCU_APB1EN: u32 = 0x4002_101C;
const RCU_APB1RST: u32 = 0x4002_1010;
const RCU_BDCTL: u32 = 0x4002_1020;
const AFIO_PCF0: u32 = 0x4001_0004;
const PMU_CTL: u32 = 0x4000_7000;
const FMC_WS: u32 = 0x4002_2000;

const GPIOA: u32 = 0x4001_0800;
const GPIOB: u32 = 0x4001_0C00;
const GPIOC: u32 = 0x4001_1000;
const GPIOD: u32 = 0x4001_1400;

const TIMER2: u32 = 0x4000_0400;

// === RCU registers ===

const RCU_BASE: u32 = 0x4002_1000;
const RCU_CTL: u32 = RCU_BASE + 0x00;
const RCU_CFG0: u32 = RCU_BASE + 0x04;
const RCU_INT: u32 = RCU_BASE + 0x08;

const CTL_IRC8MEN: u32 = 1 << 0;
const CTL_IRC8MSTB: u32 = 1 << 1;
const CTL_HXTALEN: u32 = 1 << 16;
const CTL_HXTALBPS: u32 = 1 << 18;
const CTL_CKMEN: u32 = 1 << 19;
const CTL_PLLEN: u32 = 1 << 24;
const CTL_PLLSTB: u32 = 1 << 25;

const CFG0_SCS: u32 = 0x03;
const CFG0_SCSS: u32 = 0x0C;
const CFG0_AHBPSC: u32 = 0xF0;
const CFG0_APB1PSC: u32 = 0x700;
const CFG0_APB2PSC: u32 = 0x3800;
const CFG0_ADCPSC: u32 = 0xC000;
const CFG0_PLLSEL: u32 = 1 << 16;
const CFG0_PREDV0: u32 = 1 << 17;
const CFG0_PLLMF: u32 = 0xF << 18;
const CFG0_USBDPSC: u32 = 0x3 << 22;
const CFG0_CKOUT0SEL: u32 = 0x7 << 24;
const CFG0_PLLMF_4: u32 = 1 << 27;
const CFG0_ADCPSC_2: u32 = 1 << 28;

// PLL source = IRC8M/2, ×27 → 108MHz.
const PLL_MUL27: u32 = CFG0_PLLMF_4 | (0xA << 18);
const APB1_DIV2: u32 = 0x4 << 8;
const CKSYSSRC_PLL: u32 = 0x02;
const SCSS_PLL: u32 = 0x02 << 2;
const IRC8M_STARTUP_TIMEOUT: u32 = 0x0500;

/// System clock init: IRC8M → PLL ×27 → 108MHz SYSCLK.
fn system_init() {
    unsafe {
        let val = core::ptr::read_volatile(FMC_WS as *const u32);
        core::ptr::write_volatile(FMC_WS as *mut u32, (val & !0x7) | 2);

        let val = core::ptr::read_volatile(RCU_CTL as *const u32);
        core::ptr::write_volatile(RCU_CTL as *mut u32, val | CTL_IRC8MEN);
        while core::ptr::read_volatile(RCU_CTL as *const u32) & CTL_IRC8MSTB == 0 {}

        let val = core::ptr::read_volatile(RCU_CFG0 as *const u32);
        core::ptr::write_volatile(RCU_CFG0 as *mut u32, val & !CFG0_SCS);

        let val = core::ptr::read_volatile(RCU_CTL as *const u32);
        core::ptr::write_volatile(
            RCU_CTL as *mut u32,
            val & !(CTL_HXTALEN | CTL_CKMEN | CTL_PLLEN),
        );

        let val = core::ptr::read_volatile(RCU_CFG0 as *const u32);
        core::ptr::write_volatile(
            RCU_CFG0 as *mut u32,
            val & !(CFG0_SCS
                | CFG0_AHBPSC
                | CFG0_APB1PSC
                | CFG0_APB2PSC
                | CFG0_ADCPSC
                | CFG0_ADCPSC_2
                | CFG0_CKOUT0SEL
                | CFG0_PLLSEL
                | CFG0_PREDV0
                | CFG0_PLLMF
                | CFG0_USBDPSC
                | CFG0_PLLMF_4),
        );

        let val = core::ptr::read_volatile(RCU_CTL as *const u32);
        core::ptr::write_volatile(RCU_CTL as *mut u32, val & !CTL_HXTALBPS);

        core::ptr::write_volatile(RCU_INT as *mut u32, 0x009F_0000);

        let val = core::ptr::read_volatile(RCU_CTL as *const u32);
        core::ptr::write_volatile(RCU_CTL as *mut u32, val | CTL_IRC8MEN);

        let mut timeout: u32 = 0;
        while core::ptr::read_volatile(RCU_CTL as *const u32) & CTL_IRC8MSTB == 0 {
            timeout += 1;
            if timeout >= IRC8M_STARTUP_TIMEOUT {
                loop {
                    cortex_m::asm::nop();
                }
            }
        }

        let val = core::ptr::read_volatile(RCU_CFG0 as *const u32);
        core::ptr::write_volatile(RCU_CFG0 as *mut u32, val | APB1_DIV2);

        let val = core::ptr::read_volatile(RCU_CFG0 as *const u32);
        core::ptr::write_volatile(
            RCU_CFG0 as *mut u32,
            (val & !(CFG0_PLLMF | CFG0_PLLMF_4)) | PLL_MUL27,
        );

        let val = core::ptr::read_volatile(RCU_CTL as *const u32);
        core::ptr::write_volatile(RCU_CTL as *mut u32, val | CTL_PLLEN);
        while core::ptr::read_volatile(RCU_CTL as *const u32) & CTL_PLLSTB == 0 {}

        let val = core::ptr::read_volatile(RCU_CFG0 as *const u32);
        core::ptr::write_volatile(RCU_CFG0 as *mut u32, (val & !CFG0_SCS) | CKSYSSRC_PLL);
        while (core::ptr::read_volatile(RCU_CFG0 as *const u32) & CFG0_SCSS) != SCSS_PLL {}
    }
}

/// Initialize all GPIO ports, AFIO, TIMER2 PWM.
fn init_ports() {
    unsafe {
        let val = core::ptr::read_volatile(RCU_APB2EN as *const u32);
        core::ptr::write_volatile(RCU_APB2EN as *mut u32, val | 0x503D);

        let val = core::ptr::read_volatile(AFIO_PCF0 as *const u32);
        core::ptr::write_volatile(AFIO_PCF0 as *mut u32, (val & 0xF8FF_FFFF) | 0x0200_0000);

        core::ptr::write_volatile((GPIOB + 0x00) as *mut u32, 0x3333_3333);
        core::ptr::write_volatile((GPIOB + 0x04) as *mut u32, 0x3333_3333);

        core::ptr::write_volatile((GPIOA + 0x00) as *mut u32, 0xB8B3_3334);

        let ctl1 = (GPIOA + 0x04) as *mut u32;
        let mut val = core::ptr::read_volatile(ctl1);
        val &= !(0xF << 0);
        val |= 0x3 << 0;
        val &= !(0xF << 4);
        val |= 0xB << 4;
        val &= !(0xF << 8);
        val |= 0x4 << 8;
        val &= !(0xF << 12);
        val |= 0x3 << 12;
        val &= !(0xF << 16);
        val |= 0x3 << 16;
        val &= !(0xF << 28);
        val |= 0x3 << 28;
        core::ptr::write_volatile(ctl1, val);

        core::ptr::write_volatile(
            (GPIOA + 0x10) as *mut u32,
            (1 << 2) | (1 << 4) | (1 << 11) | (1 << 12),
        );
        core::ptr::write_volatile((GPIOA + 0x14) as *mut u32, (1 << 1) | (1 << 3));

        core::ptr::write_volatile((GPIOC + 0x00) as *mut u32, 0x4444_4477);

        let ctl1 = (GPIOC + 0x04) as *mut u32;
        let mut val = core::ptr::read_volatile(ctl1);
        val &= !(0xFF);
        val |= 0x44;
        val &= !(0xF << 12);
        val |= 0x3 << 12;
        val &= !(0xF << 16);
        val |= 0x3 << 16;
        val &= !(0xF << 20);
        val |= 0x3 << 20;
        val &= !(0xF << 24);
        val |= 0x4 << 24;
        core::ptr::write_volatile(ctl1, val);

        core::ptr::write_volatile((GPIOC + 0x14) as *mut u32, 1 << 12);
        core::ptr::write_volatile((GPIOC + 0x10) as *mut u32, (1 << 0) | (1 << 1) | (1 << 13));
        core::ptr::write_volatile((GPIOC + 0x10) as *mut u32, 0x03FC_0000);

        let ctl0 = (GPIOD + 0x00) as *mut u32;
        let mut val = core::ptr::read_volatile(ctl0);
        val &= !(0xF << 8);
        val |= 0x3 << 8;
        core::ptr::write_volatile(ctl0, val);

        let val = core::ptr::read_volatile(AFIO_PCF0 as *const u32);
        core::ptr::write_volatile(AFIO_PCF0 as *mut u32, val | 0x0C00);

        let val = core::ptr::read_volatile(RCU_APB1EN as *const u32);
        core::ptr::write_volatile(RCU_APB1EN as *mut u32, val | (1 << 1));
        let val = core::ptr::read_volatile(RCU_APB1RST as *const u32);
        core::ptr::write_volatile(RCU_APB1RST as *mut u32, val | (1 << 1));
        core::ptr::write_volatile(RCU_APB1RST as *mut u32, val & !(1 << 1));

        core::ptr::write_volatile((TIMER2 + 0x18) as *mut u32, 0x6868);
        core::ptr::write_volatile((TIMER2 + 0x1C) as *mut u32, 0x6868);
        core::ptr::write_volatile((TIMER2 + 0x20) as *mut u32, 0);
        core::ptr::write_volatile((TIMER2 + 0x28) as *mut u32, 0);
        core::ptr::write_volatile((TIMER2 + 0x00) as *mut u32, 1);

        let val = core::ptr::read_volatile(PMU_CTL as *const u32);
        core::ptr::write_volatile(PMU_CTL as *mut u32, val | (1 << 8));
        let val = core::ptr::read_volatile(RCU_BDCTL as *const u32);
        core::ptr::write_volatile(RCU_BDCTL as *mut u32, val & !1);
    }
}

#[entry]
fn main() -> ! {
    system_init();
    heap::init();
    systick::init();
    init_ports();

    let gpio = Gpio::init();

    // Build the application context on the heap (too large for the stack).
    let ctx = Box::new(Ctx::<NextionPlatform> {
        lcd: lcd::new(gpio),
        flash: flash::init(),
        tree: WidgetTree::new(),
        fonts: FontList::new(),
        images: ImageList::new(),
        strpool: StringPool::new(),
        fs: None,
        backlight: backlight::init(),
        rtc: rtc::init(),
        usart: usart::init(),
        systick: systick::Systick::handle(),
        audio: ferrite_core::audio::AudioImpl::none(),
        cursor_visible: false,
    });

    let touch = touch::init();

    ferrite_core::runtime::run::<NextionPlatform>(ctx, touch)
}
