$ErrorActionPreference = 'Continue'
$log = 'D:\wsl2\sigil\build-win.log'
"=== sigil Windows build $(Get-Date -Format 's') ===" | Out-File $log
Import-Module 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\Microsoft.VisualStudio.DevShell.dll'
Enter-VsDevShell -VsInstallPath 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools' -DevCmdArguments '-arch=x64 -host_arch=x64' -SkipAutomaticLocation 2>&1 | Out-File $log -Append
"rustc: $(rustc --version 2>&1)" | Out-File $log -Append
Set-Location 'D:\wsl2\sigil'
cargo build --release 2>&1 | Out-File $log -Append
$code = $LASTEXITCODE
"BUILD_EXIT=$code" | Out-File $log -Append
Write-Host "DONE exit=$code"