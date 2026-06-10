use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

pub fn init() {
    START.get_or_init(Instant::now);
}

pub fn millis() -> u32 {
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_millis() as u32
}

pub fn delay_ms(ms: u32) {
    std::thread::sleep(std::time::Duration::from_millis(ms as u64));
}
