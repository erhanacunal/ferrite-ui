use std::env;
use std::path::PathBuf;

fn main() {
    // The linker runs with CWD = workspace root, but `link.ld` lives in this
    // crate's directory. Add the manifest dir to the linker search path so the
    // `-T link.ld` argument resolves regardless of where cargo is invoked.
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Exception vectors + stack setup from the HAL, assembled with the ARM GNU
    // toolchain (must be on PATH, or set CC_armv5te_none_eabi).
    let asm = manifest_dir.join("../../vendor/n32903_rs/asm/startup.s");
    let obj = out_dir.join("startup.o");
    let cc = env::var("CC_armv5te_none_eabi").unwrap_or_else(|_| "arm-none-eabi-gcc".to_string());
    let status = std::process::Command::new(&cc)
        .args(["-mcpu=arm926ej-s", "-marm", "-c"])
        .arg(&asm)
        .arg("-o")
        .arg(&obj)
        .status()
        .expect("failed to run arm-none-eabi-gcc for startup.s");
    assert!(status.success(), "arm-none-eabi-gcc failed to assemble startup.s");

    println!("cargo:rustc-link-search={}", manifest_dir.display());
    println!("cargo:rustc-link-arg={}", obj.display());
    println!("cargo:rustc-link-arg=-Tlink.ld");
    println!("cargo:rustc-link-arg=-nostartfiles");
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rerun-if-changed={}", asm.display());
    println!("cargo:rerun-if-changed=build.rs");
}
