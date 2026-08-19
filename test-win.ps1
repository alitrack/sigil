# cargo test runner under the MSVC dev environment (mirrors build-win.ps1)
Import-Module "$env:ProgramFiles\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\Microsoft.VisualStudio.DevShell.dll"
Enter-VsDevShell -DeveloperInstantiationFlag -SkipAutomaticLocation `
    -VsInstallPath "$env:ProgramFiles\Microsoft Visual Studio\2022\BuildTools" `
    -DevCmdArguments '-arch=x64 -host=x64' | Out-Null
Set-Location 'D:\wsl2\sigil'
$testFilter = if ($args.Count -gt 0) { $args[0] } else { 'openai' }
cargo test --release -p sigil-agent $testFilter 2>&1 | Tee-Object -FilePath 'D:\wsl2\sigil\test-win.log'
Write-Host "TEST_EXIT=$LASTEXITCODE"