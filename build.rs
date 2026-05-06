use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Add OUT_DIR to the linker search path so copied linker-script files are found.
    println!("cargo:rustc-link-search=native={}", out_dir.display());

    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    if target_arch == "xtensa" {
        linker_be_nice();
        println!("cargo:rustc-link-arg=-Tlinkall.x");
        // Embed ferrite_fs.bin into the binary for XIP-safe flash reads.
        // If no ferrite_fs.bin exists, write a zero-filled placeholder so
        // include_bytes! always compiles; Fs::mount() will fail with BadMagic.
        let fs_src = PathBuf::from("ferrite_fs.bin");
        let fs_dst = out_dir.join("ferrite_fs.bin");
        if fs_src.exists() {
            let fs_image = fs::read(&fs_src).unwrap();
            fs::write(&fs_dst, normalize_ferrite_fs(&fs_image)).unwrap();
        } else {
            // Minimal placeholder — 4KB reserved sector + 16B of zeros (no FERR magic).
            fs::write(&fs_dst, &[0u8; 0x1010]).unwrap();
        }
        println!("cargo:rerun-if-changed=ferrite_fs.bin");
    } else {
        // Cortex-M / host build: supply the GD32F103 interrupt-vector definition.
        fs::copy("gd32-memory.x", out_dir.join("memory.x")).unwrap();
        if fs::copy("gd32-device.x", out_dir.join("device.x")).is_err() {
            // device.x is only required for the firmware target; ignore missing on host.
        }
        println!("cargo:rerun-if-changed=gd32-device.x");
        println!("cargo:rerun-if-changed=gd32-memory.x");
    }
}

fn normalize_ferrite_fs(image: &[u8]) -> Vec<u8> {
    const FS_BASE: usize = 0x1000;
    const MAGIC: &[u8; 4] = b"FERR";

    if image.len() >= FS_BASE + MAGIC.len() && &image[FS_BASE..FS_BASE + MAGIC.len()] == MAGIC {
        return image.to_vec();
    }

    if image.starts_with(MAGIC) {
        let mut flash_image = vec![0xFF; FS_BASE + image.len()];
        flash_image[FS_BASE..].copy_from_slice(image);
        return flash_image;
    }

    image.to_vec()
}

fn linker_be_nice() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let kind = &args[1];
        let what = &args[2];

        match kind.as_str() {
            "undefined-symbol" => match what.as_str() {
                what if what.starts_with("_defmt_") => {
                    eprintln!();
                    eprintln!(
                        "💡 `defmt` not found - make sure `defmt.x` is added as a linker script and you have included `use defmt_rtt as _;`"
                    );
                    eprintln!();
                }
                "_stack_start" => {
                    eprintln!();
                    eprintln!("💡 Is the linker script `linkall.x` missing?");
                    eprintln!();
                }
                what if what.starts_with("esp_rtos_") => {
                    eprintln!();
                    eprintln!(
                        "💡 `esp-radio` has no scheduler enabled. Make sure you have initialized `esp-rtos` or provided an external scheduler."
                    );
                    eprintln!();
                }
                "embedded_test_linker_file_not_added_to_rustflags" => {
                    eprintln!();
                    eprintln!(
                        "💡 `embedded-test` not found - make sure `embedded-test.x` is added as a linker script for tests"
                    );
                    eprintln!();
                }
                "free"
                | "malloc"
                | "calloc"
                | "get_free_internal_heap_size"
                | "malloc_internal"
                | "realloc_internal"
                | "calloc_internal"
                | "free_internal" => {
                    eprintln!();
                    eprintln!(
                        "💡 Did you forget the `esp-alloc` dependency or didn't enable the `compat` feature on it?"
                    );
                    eprintln!();
                }
                _ => (),
            },
            // we don't have anything helpful for "missing-lib" yet
            _ => {
                std::process::exit(1);
            }
        }

        std::process::exit(0);
    }

    println!(
        "cargo:rustc-link-arg=-Wl,--error-handling-script={}",
        std::env::current_exe().unwrap().display()
    );
}
