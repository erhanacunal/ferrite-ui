#Requires -Version 5.1
<#
.SYNOPSIS
    Build the ferrite-ui firmware for Nextion N32903 boards (Nuvoton N32903).

.DESCRIPTION
    Compiles the `bsp-n32903` crate from its own directory (its .cargo/config.toml
    selects the arm926ej-s JSON target + build-std + json-target-spec; its
    rust-toolchain.toml pins nightly + rust-src), then by default converts the
    ELF to a raw binary (arm-none-eabi-objcopy). The NAND loader
    (n32903_nand_loader_rs) loads that image to SDRAM 0x0 and jumps to it:

        python flash_tool.py COMx writeapp ferrite-ui.bin --boot -m

    arm-none-eabi-gcc must be on PATH (it links and assembles startup.s).

.PARAMETER Action
    'build' (default) — compile and emit the raw .bin image
    'elf'             — compile only (skip objcopy)

.PARAMETER Clean
    Run `cargo clean` before building.

.EXAMPLE
    .\n32903.ps1            # build + raw image
    .\n32903.ps1 elf        # just compile the ELF
    .\n32903.ps1 -Clean     # clean rebuild + image
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
$BspDir  = Join-Path $Root 'bsp\n32903'
$Target  = 'arm926ej-s'
$Bin     = 'ferrite-ui'
$Elf     = Join-Path $Root "target\$Target\release\$Bin"
$ImgBin  = Join-Path $Root "target\$Target\release\$Bin.bin"
$Objcopy = 'arm-none-eabi-objcopy'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo not found. Install Rust via rustup and retry."
}
if (-not (Get-Command arm-none-eabi-gcc -ErrorAction SilentlyContinue)) {
    Write-Error "arm-none-eabi-gcc not found on PATH (ARM GNU toolchain)."
}

# ── Build (from the BSP directory) ───────────────────────────────────────────
Write-Host ""
Write-Host "[1/2] Building n32903 firmware  (target: $Target)" -ForegroundColor Cyan
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

# ── ELF -> raw binary (what the NAND loader copies to SDRAM 0x0) ─────────────
Write-Host ""
Write-Host "[2/2] objcopy  ELF -> raw binary" -ForegroundColor Cyan
& $Objcopy -O binary $Elf $ImgBin
if ($LASTEXITCODE -ne 0) { Write-Error "objcopy failed." }
Write-Host "  BIN: $ImgBin ($((Get-Item $ImgBin).Length) bytes)" -ForegroundColor Green

Write-Host ""
Write-Host "Flash with the NAND loader's tool:" -ForegroundColor Green
Write-Host "  python flash_tool.py COMx writeapp `"$ImgBin`" --boot -m"
