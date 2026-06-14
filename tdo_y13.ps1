#Requires -Version 5.1
<#
.SYNOPSIS
    Build the ferrite-ui firmware for the TDO Y13 board (Allwinner F1C100s).

.DESCRIPTION
    Compiles the `bsp-tdo-y13` crate from its own directory (its .cargo/config.toml
    selects the armv5te JSON target + build-std + json-target-spec; its
    rust-toolchain.toml pins nightly + rust-src), then by default produces a
    bootable sunxi image: ELF -> raw binary (arm-none-eabi-objcopy) -> eGON boot
    header (mksunxi.exe). Flash the resulting .bin to the board's SPI NOR / SD.

.PARAMETER Action
    'build' (default) — compile and emit the bootable .bin image
    'elf'             — compile only (skip objcopy / mksunxi)

.PARAMETER Clean
    Run `cargo clean` before building.

.EXAMPLE
    .\tdo_y13.ps1            # build + bootable image
    .\tdo_y13.ps1 elf        # just compile the ELF
    .\tdo_y13.ps1 -Clean     # clean rebuild + image
#>
param(
    [ValidateSet('build', 'elf')]
    [string] $Action = 'build',
    [switch] $Clean
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ── Paths ────────────────────────────────────────────────────────────────────
$Root    = $PSScriptRoot
$BspDir  = Join-Path $Root 'bsp\tdo_y13'
$Target  = 'armv5te-none-eabi'
$Bin     = 'ferrite-ui'
$Elf     = Join-Path $Root "target\$Target\release\$Bin"
$ImgBin  = Join-Path $Root "target\$Target\release\$Bin.bin"
$Objcopy = 'D:\armeabi_none_toolchain\bin\arm-none-eabi-objcopy.exe'
$MkSunxi = Join-Path $BspDir 'mksunxi.exe'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo not found. Install Rust via rustup and retry."
}

# ── Build (from the BSP directory) ───────────────────────────────────────────
Write-Host ""
Write-Host "[1/3] Building tdo_y13 firmware  (target: $Target)" -ForegroundColor Cyan
Push-Location $BspDir
try {
    if ($Clean) {
        Write-Host "  cargo clean" -ForegroundColor Yellow
        cargo clean
    }
    cargo build --release
    if ($LASTEXITCODE -ne 0) { Write-Error "Build failed." }
}
finally {
    Pop-Location
}

if (-not (Test-Path $Elf)) {
    Write-Error "ELF not found: $Elf"
}
Write-Host "  ELF: $Elf ($((Get-Item $Elf).Length) bytes)" -ForegroundColor Green

if ($Action -eq 'elf') {
    Write-Host ""
    Write-Host "Done (ELF only)." -ForegroundColor Green
    return
}

# ── ELF -> raw binary ────────────────────────────────────────────────────────
Write-Host ""
Write-Host "[2/3] objcopy  ELF -> raw binary" -ForegroundColor Cyan
if (-not (Test-Path $Objcopy)) {
    Write-Error "objcopy not found: $Objcopy  (update the path to your ARM GNU toolchain)."
}
& $Objcopy -O binary $Elf $ImgBin
if ($LASTEXITCODE -ne 0) { Write-Error "objcopy failed." }
Write-Host "  BIN: $ImgBin ($((Get-Item $ImgBin).Length) bytes)" -ForegroundColor Green

# ── Wrap as a bootable sunxi image (patches the eGON header in place) ─────────
Write-Host ""
Write-Host "[3/3] mksunxi  -> bootable image" -ForegroundColor Cyan
if (-not (Test-Path $MkSunxi)) {
    Write-Error "mksunxi.exe not found: $MkSunxi"
}
& $MkSunxi $ImgBin
if ($LASTEXITCODE -ne 0) { Write-Error "mksunxi failed." }
Write-Host "  Bootable image: $ImgBin" -ForegroundColor Green

scp -i $HOME\.ssh\eniac01  $ImgBin developer@192.168.88.6:/home/developer

Write-Host ""
Write-Host "Done." -ForegroundColor Green
