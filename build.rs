use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::copy("device.x", out_dir.join("device.x")).unwrap();
    println!("cargo:rerun-if-changed=device.x");
    println!("cargo:rerun-if-changed=memory.x");
}
