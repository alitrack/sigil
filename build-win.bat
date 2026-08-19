@echo off
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat" -arch=x64 -host_arch=x64 >"%~dp0build-win.log" 2>&1
echo VsDevCmd done >>"%~dp0build-win.log"
cd /d "%~dp0"
cargo build --release >>"%~dp0build-win.log" 2>&1
echo BUILD_EXIT=%ERRORLEVEL% >>"%~dp0build-win.log"