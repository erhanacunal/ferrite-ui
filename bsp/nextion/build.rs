use std::env;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    // Copy linker scripts for cortex-m-rt
    std::fs::copy("memory.x", out_dir.join("memory.x")).ok();
    std::fs::copy("device.x", out_dir.join("device.x")).ok();
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=device.x");
}
