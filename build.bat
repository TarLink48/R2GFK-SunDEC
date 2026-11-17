@echo off
REM Этот батник собирает только ядро Rust и обновляет библиотеку rust_bridge.dll.
cd rust-bridge
cargo build --release
copy target\release\rust_bridge.dll ..\release\ >nul
pause