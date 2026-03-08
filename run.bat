@echo off
REM run.bat — Execute a command using this project's Rust toolchain.
REM
REM Usage:
REM   run cargo check --workspace
REM   run cargo test -p zccache-hash
REM   run rustc --version
REM
REM Ensures the rustup-managed toolchain is used regardless of system PATH.

setlocal

REM Resolve cargo bin directory
if defined CARGO_HOME (
    set "RUSTUP_BIN=%CARGO_HOME%\bin"
) else if exist "%USERPROFILE%\.cargo\bin" (
    set "RUSTUP_BIN=%USERPROFILE%\.cargo\bin"
) else (
    echo error: Cannot find .cargo\bin. Run ./install first. >&2
    exit /b 1
)

REM Prepend rustup bin to PATH
set "PATH=%RUSTUP_BIN%;%PATH%"

REM Verify rustup exists
where rustup >nul 2>nul
if errorlevel 1 (
    echo error: rustup not found at %RUSTUP_BIN%. Run ./install first. >&2
    exit /b 1
)

REM Check arguments
if "%~1"=="" (
    echo usage: run ^<command^> [args...]
    echo   e.g. run cargo check --workspace
    exit /b 1
)

REM Execute the command
%*
