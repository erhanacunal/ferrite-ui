@echo off
setlocal

set TARGET=thumbv7m-none-eabi
set BIN=ferrite-ui
REM set OBJCOPY=D:\armeabi_none_toolchain\bin\arm-none-eabi-objcopy.exe
set ELF=target\%TARGET%\release\%BIN%

cargo build --release --no-default-features --features firmware --bin %BIN% --target %TARGET%
if errorlevel 1 (
    echo Build failed.
    exit /b 1
)

REM if not exist "%OBJCOPY%" (
REM    echo Objcopy not found: %OBJCOPY%
REM    exit /b 1
REM )

REM "%OBJCOPY%" -v -O binary "%ELF%" firmware.bin
cargo objcopy --release --no-default-features --features firmware --bin ferrite-ui --target thumbv7m-none-eabi -- -O binary firmware.bin
if errorlevel 1 (
    echo Objcopy failed.
    exit /b 1
)

echo firmware.bin ready (%~dp0firmware.bin)

copy "firmware.bin" "D:\utils\stlink\bin\firmware.bin"
if errorlevel 1 (
    echo Copy to D:\utils\stlink\bin\firmware.bin failed.
    exit /b 1
)
