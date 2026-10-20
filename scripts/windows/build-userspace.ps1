$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '../..')
if (!$env:PROTOC) { $env:PROTOC = (Get-Command protoc -ErrorAction Stop).Source }
cargo build --locked --bin zcnblk-client --bin zcnblk-portable-target --bin zcnblk-windows-client --bin zccusan-crdgen --bin zccusan-operator --bin zccusan-telemetry-server
if ($LASTEXITCODE -ne 0) { throw 'Windows userspace build failed' }
