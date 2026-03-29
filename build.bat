@echo off
cargo build --release 2>&1 | findstr /r /c:"^error"
if %errorlevel% equ 0 (
    echo Build failed.
    exit /b 1
)
cargo objcopy --release -- -O binary firmware.bin >nul 2>&1
echo firmware.bin ready (%~dp0firmware.bin)

copy "firmware.bin" "D:\utils\stlink\bin\firmware.bin"