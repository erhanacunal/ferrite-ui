@echo off
setlocal

set TARGET=thumbv7m-none-eabi
REM thumbv7m-none-eabi with "frame-pointer": "may-omit" (bsp\nextion\thumbv7m-none-eabi.json);
REM a custom spec has no prebuilt core/alloc, hence -Zbuild-std (nightly + rust-src).
set SPEC=bsp\nextion\%TARGET%.json
set ZFLAGS=-Zjson-target-spec -Zbuild-std=core,compiler_builtins,alloc -Zbuild-std-features=compiler-builtins-mem
set BIN=ferrite-ui
set OBJCOPY=D:\armeabi_none_toolchain\bin\arm-none-eabi-objcopy.exe
set ELF=target\%TARGET%\release\%BIN%

cargo build -p bsp-nextion --release --no-default-features --bin %BIN% --target %SPEC% %ZFLAGS%
if errorlevel 1 (
    echo Build failed.
    exit /b 1
)

REM if not exist "%OBJCOPY%" (
REM    echo Objcopy not found: %OBJCOPY%
REM    exit /b 1
REM )

"%OBJCOPY%" -v -O binary "%ELF%" firmware.bin
REM cargo objcopy --release --no-default-features --features firmware --bin ferrite-ui --target thumbv7m-none-eabi -- -O binary firmware.bin
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
