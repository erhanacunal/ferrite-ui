@echo off
setlocal enabledelayedexpansion

:: Change to the repo root regardless of where the script is called from
set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
cd /d "%ROOT%"

if "%~1"=="" (
    echo Usage: sim.bat ^<project^>
    echo.
    echo   ^<project^> can be a directory containing project.json,
    echo   or a direct path to a project.json file.
    echo.
    echo Examples:
    echo   sim.bat examples\brick_breaker
    echo   sim.bat examples\widget_demo
    exit /b 1
)

:: Resolve project.json and output directory
set "ARG=%~1"
if exist "%ARG%\project.json" (
    set "JSON=%ARG%\project.json"
    set "OUTDIR=%ARG%"
) else if exist "%ARG%" (
    set "JSON=%ARG%"
    for %%F in ("%ARG%") do set "OUTDIR=%%~dpF"
    if "!OUTDIR:~-1!"=="\" set "OUTDIR=!OUTDIR:~0,-1!"
) else (
    echo Error: "%ARG%" not found
    exit /b 1
)

set "FLASH=%OUTDIR%\flash.bin"

echo.
echo [1/2] Building flash image
echo       %JSON%
python tools\ferrite_build.py "%JSON%" -o "%FLASH%"
if errorlevel 1 (
    echo.
    echo Flash build failed.
    exit /b 1
)

echo.
echo [2/2] Launching simulator
cargo run --target x86_64-pc-windows-msvc --features host --bin sim -- --fsimage "%FLASH%"
