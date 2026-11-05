#Requires -Version 5.1
<#
.SYNOPSIS
    Build the ferrite-ui firmware for the Exynos 4412 tablet, and optionally
    send it to the board's uart_boot.

.DESCRIPTION
    Compiles the `bsp-exynos4412` crate from its own directory (its
    .cargo/config.toml selects armv7a-none-eabihf; building from the root
    would pick the workspace's default target), then converts the ELF to a
    raw image with rust-objcopy. The .bin is what uart_boot loads — the ELF
    alone is not, so a cargo build without objcopy leaves the old firmware
    in the .bin.

    Upload paths (the HAL's tools, vendor/exynos_4412_rs/tools):
      uart — uartboot.py over the console UART (115200, ~11 KB/s)
      http — httpboot.py over WiFi (uart_boot joins WiFi a few s after boot)
    By default the image is saved on CACHE (FERRITE/FERRITE.BIN) as the
    firmware uart_boot boots; -RamOnly runs it once from RAM instead.

    Needs: rustup target add armv7a-none-eabihf; cargo install cargo-binutils;
    rustup component add llvm-tools; Python 3 + pyserial (uart).

.PARAMETER Action
    'build' (default) — compile and emit the raw .bin image
    'elf'             — compile only (skip objcopy)
    'uart'            — build, then send over UART (-Port)
    'http'            — build, then send over WiFi (-BoardHost)

.PARAMETER Port
    Serial port for 'uart' (e.g. COM8). Default: $env:EXYNOS_PORT.

.PARAMETER BoardHost
    Board address or name for 'http'. Default: $env:EXYNOS_HOST, else the
    tool's default (exynos).

.PARAMETER RamOnly
    Run the image from RAM until the next restart instead of saving it.

.PARAMETER Wait
    Seconds to wait for uart_boot (power on / reset the board meanwhile).
    'uart': without it, a running ferrite is restarted first (--reset).

.PARAMETER Monitor
    'uart': print the board's output after sending (Ctrl+C to stop).

.PARAMETER Clean
    Run `cargo clean` before building.

.EXAMPLE
    .\exynos4412.ps1                        # build + raw image
    .\exynos4412.ps1 uart -Port COM8        # build, send over UART, save
    .\exynos4412.ps1 uart -Port COM8 -Monitor
    .\exynos4412.ps1 http -BoardHost 192.168.88.120
    .\exynos4412.ps1 http -Wait 30          # board is booting: keep retrying
    .\exynos4412.ps1 uart -Port COM8 -RamOnly
#>
param(
    [ValidateSet('build', 'elf', 'uart', 'http')]
    [string] $Action = 'build',
    [string] $Port = $env:EXYNOS_PORT,
    [string] $BoardHost = $env:EXYNOS_HOST,
    [switch] $RamOnly,
    [int] $Wait = 0,
    [switch] $Monitor,
    [switch] $Clean
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ── Paths ────────────────────────────────────────────────────────────────────
$Root    = $PSScriptRoot
$BspDir  = Join-Path $Root 'bsp\exynos4412'
$Target  = 'armv7a-none-eabihf'
$Bin     = 'ferrite-ui'
$Elf     = Join-Path $Root "target\$Target\release\$Bin"
$ImgBin  = Join-Path $Root "target\$Target\release\$Bin.bin"
$Tools   = Join-Path $Root 'vendor\exynos_4412_rs\tools'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo not found. Install Rust via rustup and retry."
}
if ($Action -ne 'elf' -and -not (Get-Command rust-objcopy -ErrorAction SilentlyContinue)) {
    Write-Error "rust-objcopy not found. Run: cargo install cargo-binutils; rustup component add llvm-tools"
}
if (-not (Test-Path (Join-Path $Root 'vendor\exynos_4412_rs\exynos_4412_rs\Cargo.toml'))) {
    Write-Error "HAL submodule missing. Run: git submodule update --init vendor/exynos_4412_rs"
}
if ($Action -eq 'uart' -and -not $Port) {
    Write-Error "'uart' needs -Port COMx (or `$env:EXYNOS_PORT)."
}

$Steps = if ($Action -in 'uart', 'http') { 3 } elseif ($Action -eq 'elf') { 1 } else { 2 }

# ── Build (from the BSP directory) ───────────────────────────────────────────
Write-Host ""
Write-Host "[1/$Steps] Building exynos4412 firmware  (target: $Target)" -ForegroundColor Cyan
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

# ── ELF -> raw binary (what uart_boot loads) ─────────────────────────────────
Write-Host ""
Write-Host "[2/$Steps] objcopy  ELF -> raw binary" -ForegroundColor Cyan
rust-objcopy -O binary $Elf $ImgBin
if ($LASTEXITCODE -ne 0) { Write-Error "objcopy failed." }
Write-Host "  BIN: $ImgBin ($((Get-Item $ImgBin).Length) bytes)" -ForegroundColor Green

if ($Action -eq 'build') {
    Write-Host ""
    Write-Host "Send it to uart_boot:" -ForegroundColor Green
    Write-Host "  .\exynos4412.ps1 uart -Port COMx          (or: http -BoardHost <address>)"
    return
}

# ── Upload ───────────────────────────────────────────────────────────────────
if (-not (Get-Command python -ErrorAction SilentlyContinue)) {
    Write-Error "python not found on PATH."
}
$Mode = if ($RamOnly) { 'run from RAM' } else { 'save on CACHE' }
Write-Host ""
if ($Action -eq 'uart') {
    Write-Host "[3/$Steps] uartboot  $Port  ($Mode)" -ForegroundColor Cyan
    $ToolArgs = @((Join-Path $Tools 'uartboot.py'), '--port', $Port)
    # A running ferrite answers on the same UART: restart it into uart_boot,
    # unless we're told to wait for a manual reset instead.
    if ($Wait -gt 0) { $ToolArgs += @('--wait', $Wait) } else { $ToolArgs += '--reset' }
    if ($Monitor) { $ToolArgs += '--monitor' }
}
else {
    $Shown = if ($BoardHost) { $BoardHost } else { '(default host)' }
    Write-Host "[3/$Steps] httpboot  $Shown  ($Mode)" -ForegroundColor Cyan
    $ToolArgs = @((Join-Path $Tools 'httpboot.py'))
    if ($BoardHost) { $ToolArgs += @('--host', $BoardHost) }
    if ($Wait -gt 0) { $ToolArgs += @('--wait', $Wait) }
}
if (-not $RamOnly) { $ToolArgs += '--save' }
$ToolArgs += $ImgBin

& python @ToolArgs
if ($LASTEXITCODE -ne 0) { Write-Error "Upload failed." }

Write-Host ""
Write-Host "Done." -ForegroundColor Green
