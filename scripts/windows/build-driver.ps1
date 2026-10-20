# Build only: no driver installation, certificate trust, or boot-policy changes.
[CmdletBinding()]
param([string]$PackagesDirectory = 'target/windows-kits', [string]$OutputDirectory = 'target/windows-driver')
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '../..')
$packages = [IO.Path]::GetFullPath($PackagesDirectory)
$output = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Force $packages,$output | Out-Null
# Pin matching, Microsoft-published WDK/SDK packages for repeatable builds.
nuget install Microsoft.Windows.WDK.x64 -Version 10.0.26100.1 -OutputDirectory $packages -Source https://api.nuget.org/v3/index.json -NonInteractive
if ($LASTEXITCODE -ne 0) { throw 'WDK acquisition failed' }
$km = Get-ChildItem $packages -Directory -Recurse | Where-Object { $_.FullName -match 'Include[\\]10\.0\.26100\.0[\\]km$' } | Select-Object -First 1
$shared = Get-ChildItem $packages -Directory -Recurse | Where-Object { $_.FullName -match 'Include[\\]10\.0\.26100\.0[\\]shared$' } | Select-Object -First 1
$ucrt = Get-ChildItem $packages -Directory -Recurse | Where-Object { $_.FullName -match 'Include[\\]10\.0\.26100\.0[\\]ucrt$' } | Select-Object -First 1
$lib = Get-ChildItem $packages -Filter storport.lib -Recurse | Where-Object { $_.FullName -match 'km[\\]x64[\\]' } | Select-Object -First 1
if (!$km -or !$shared -or !$ucrt -or !$lib) { throw 'Matching SDK/WDK headers/libraries missing' }
$vswhere = "${env:ProgramFiles(x86)}/Microsoft Visual Studio/Installer/vswhere.exe"
$vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (!$vs) { throw 'Install Visual Studio C++ build tools' }
$dev = Join-Path $vs 'Common7/Tools/VsDevCmd.bat'
# Import the compiler environment into this PowerShell process.
cmd /s /c "`"$dev`" -arch=x64 -host_arch=x64 >nul && set" | ForEach-Object {
    if ($_ -match '^([^=]+)=(.*)$') { [Environment]::SetEnvironmentVariable($matches[1],$matches[2],'Process') }
}
& cl.exe /nologo /c /TC /std:c11 /kernel /GS- /Zl /W4 /D_AMD64_ /DAMD64 /D_WIN64 /D_KERNEL_MODE /DNTDDI_VERSION=0x0A000000 /D_WIN32_WINNT=0x0A00 "/I$km" "/I$shared" "/I$ucrt" "/Fo$output/zcnblk.obj" windows/zcnblk/zcnblk.c
if ($LASTEXITCODE -ne 0) { throw 'Driver compilation failed' }
& link.exe /nologo /driver /subsystem:native,10.0 /entry:DriverEntry /nodefaultlib /machine:x64 /nxcompat /dynamicbase /integritycheck "/out:$output/zcnblk.sys" "$output/zcnblk.obj" (Join-Path $lib.DirectoryName 'ntoskrnl.lib') $lib.FullName
if ($LASTEXITCODE -ne 0) { throw 'Driver link failed' }
Copy-Item windows/zcnblk/zcnblk.inf $output
Write-Output "Unsigned driver: $output/zcnblk.sys"
