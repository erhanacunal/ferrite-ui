//! Puts `memory.x` on the linker search path; the HAL's `link.x` includes it.
//!
//! WiFi (`wifi` feature, `src/net/`): the BCM4334 firmware + NVRAM are not in
//! this repository. `WIFI_FIRMWARE_DIR` (the directory the HAL's `uart_boot`
//! uses) embeds them; without it the image gets empty files and the firmware
//! is read from CACHE at boot (`FERRITE/WIFIFW.BIN`, `FERRITE/WIFINV.TXT`).
//! The network comes from `FERRITE/WIFI.CFG` on CACHE; `WIFI_SSID` /
//! `WIFI_PASSWORD` / `WIFI_BAND` at build time are only a fallback (they end
//! up in the image: don't publish such a build).

use std::{env, fs, path::PathBuf};

const FIRMWARE: [(&str, &str); 2] = [
    ("brcmfmac4334-sdio.samsung,mehmet-board.bin", "wifi_firmware.bin"),
    ("brcmfmac4334-sdio.samsung,mehmet-board.txt", "wifi_nvram.txt"),
];

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    fs::copy("memory.x", out.join("memory.x")).expect("copy memory.x into OUT_DIR");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");

    println!("cargo:rerun-if-env-changed=WIFI_FIRMWARE_DIR");
    for name in ["WIFI_SSID", "WIFI_PASSWORD", "WIFI_BAND"] {
        println!("cargo:rerun-if-env-changed={name}");
        if let Ok(value) = env::var(name) {
            println!("cargo:rustc-env={name}={value}");
        }
    }
    let dir = env::var_os("WIFI_FIRMWARE_DIR").map(PathBuf::from);
    for (name, output) in FIRMWARE {
        match &dir {
            Some(dir) => {
                let source = dir.join(name);
                println!("cargo:rerun-if-changed={}", source.display());
                fs::copy(&source, out.join(output)).unwrap_or_else(|e| panic!("{}: {}", source.display(), e));
            }
            None => fs::write(out.join(output), []).expect("write an empty file"),
        }
    }
}
