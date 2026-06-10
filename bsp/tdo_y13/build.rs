use std::env;

fn main() {
    // The linker runs with CWD = workspace root, but `link.lds` lives in this
    // crate's directory. Add the manifest dir to the linker search path so the
    // `-T link.lds` argument resolves regardless of where cargo is invoked.
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search={manifest_dir}");
    println!("cargo:rustc-link-arg=-Tlink.lds");
    println!("cargo:rerun-if-changed=link.lds");
    println!("cargo:rerun-if-changed=build.rs");
}
