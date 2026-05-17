#Requires -Version 5.1
<#
.SYNOPSIS
    Build (and optionally flash) the ferrite-ui epaper firmware for ESP32-S3.

.PARAMETER Action
    'build' (default) — compile only
    'flash'           — compile and flash via espflash
    'monitor'         — compile, flash, and open serial monitor

.PARAMETER Port
    Optional COM port for flashing (e.g. COM3).
    Omit to let espflash auto-detect.

.PARAMETER FlashFs
    After a successful firmware flash, also write .\ferrite_fs.bin to the ferrite
    partition at 0x110000 (built with tools/ferrite_build.py). Ignored for `build`.

.EXAMPLE
    .\epaper.ps1
    .\epaper.ps1 flash
    .\epaper.ps1 flash COM3
    .\epaper.ps1 flash COM3 -FlashFs
    .\epaper.ps1 monitor

.NOTES
    partition.csv must use subtypes that espflash (esp-idf-part) accepts. A `data`
    row with hex subtype 0x40 can panic the flasher — see comments in partition.csv.
    `ferrite_fs.bin` must be a ferrite filesystem (ferrite_build.py), not vm bytecode from ferrite_lang.py.
#>
param(
    [string] $Action = 'build',
    [string] $Port = '',
    [switch] $FlashFs
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ── Repo root ────────────────────────────────────────────────────────────────
$Root = $PSScriptRoot
Set-Location $Root

$Target = 'xtensa-esp32s3-none-elf'
$Elf = "target\$Target\release\epaper"
$PartitionTable = "partition.csv"

# ── ESP environment ──────────────────────────────────────────────────────────
$ExportEsp = "$env:USERPROFILE\export-esp.ps1"
if (Test-Path $ExportEsp) {
    . $ExportEsp
}
else {
    Write-Warning "export-esp.ps1 not found at $ExportEsp"
    Write-Warning "Run 'espup install' and retry, or source the file manually."
}

# ── Build ────────────────────────────────────────────────────────────────────
Write-Host ""
Write-Host "[1/2] Building epaper firmware  (target: $Target)" -ForegroundColor Cyan
$Features = 'epaper'

cargo +esp build "-Zbuild-std=core,alloc" `
    --no-default-features `
    --features $Features `
    --bin epaper `
    --target $Target `
    --release

if ($LASTEXITCODE -ne 0) {
    Write-Host ""
    Write-Error "Build failed."
}

Write-Host ""
Write-Host "ELF: $Root\$Elf"

# ── Flash ─────────────────────────────────────────────────────────────────────
if ($Action -iin @('flash')) {
    if (-not (Get-Command espflash -ErrorAction SilentlyContinue)) {
        Write-Error "espflash not found. Install: cargo install espflash"
    }

    Write-Host ""
    Write-Host "[2/2] Flashing via espflash" -ForegroundColor Cyan

    $portArgs = if ($Port) { @('--port', $Port) } else { @() }    

    espflash $Action --chip esp32s3 --flash-mode dio --flash-size 16mb `
        --partition-table $PartitionTable --no-skip `
        @portArgs $Elf

    if ($LASTEXITCODE -ne 0) {
        Write-Error "Flash failed."
    }
    Write-Host "Flash complete." -ForegroundColor Green

    if ($FlashFs) {
        $FsBin = Join-Path $Root "ferrite_fs.bin"
        if (-not (Test-Path $FsBin)) {
            Write-Error "-FlashFs was set but ferrite_fs.bin not found. Build one with: python tools/ferrite_build.py <project.json> -o ferrite_fs.bin"
        }
        Write-Host ""
        Write-Host "[3/3] Writing ferrite_fs.bin to flash 0x110000 (ferrite partition)" -ForegroundColor Cyan
        $wbPort = if ($Port) { @('-p', $Port) } else { @() }
        espflash write-bin --chip esp32s3 @wbPort "0x110000" $FsBin
        if ($LASTEXITCODE -ne 0) {
            Write-Error "write-bin ferrite_fs.bin failed."
        }
        Write-Host "Filesystem image written." -ForegroundColor Green
    }
}
elseif ($Action -iin @('monitor')) {
    $portArgs = if ($Port) { @('--port', $Port) } else { @() }
    espflash monitor @portArgs
}
elseif ($Action -iin @('flashfs')) {
    $FsBin = Join-Path $Root "ferrite_fs.bin"
    if (-not (Test-Path $FsBin)) {
        Write-Error "-FlashFs was set but ferrite_fs.bin not found. Build one with: python tools/ferrite_build.py <project.json> -o ferrite_fs.bin"
    }
    Write-Host ""
    Write-Host "[3/3] Writing ferrite_fs.bin to flash 0x110000 (ferrite partition)" -ForegroundColor Cyan
    $wbPort = if ($Port) { @('-p', $Port) } else { @() }
    espflash write-bin --chip esp32s3 @wbPort "0x110000" $FsBin
    if ($LASTEXITCODE -ne 0) {
        Write-Error "write-bin ferrite_fs.bin failed."
    }
    Write-Host "Filesystem image written." -ForegroundColor Green
}
else {
    Write-Host ""
    Write-Host "To flash:   .\epaper.ps1 flash [COM?] [-FlashFs]"
    Write-Host "To monitor: .\epaper.ps1 monitor [COM?]"
}

Write-Host ""
Write-Host "Done." -ForegroundColor Green
