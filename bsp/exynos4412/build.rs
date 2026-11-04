//! Puts `memory.x` on the linker search path; the HAL's `link.x` includes it.

use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    fs::copy("memory.x", out.join("memory.x")).expect("copy memory.x into OUT_DIR");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");
}
