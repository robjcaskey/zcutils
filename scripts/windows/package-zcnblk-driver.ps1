# Package only. Does not install a driver, trust a certificate, or change boot policy.
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$InputDirectory,
    [Parameter(Mandatory=$true)][string]$OutputDirectory,
    [Parameter(Mandatory=$true)][string]$CertificateThumbprint,
    [Parameter(Mandatory=$true)][string]$SignToolPath,
    [Parameter(Mandatory=$true)][string]$Inf2CatPath
)
$ErrorActionPreference = 'Stop'
$source = (Resolve-Path $InputDirectory).Path
$destination = [IO.Path]::GetFullPath($OutputDirectory)
if ($source.TrimEnd('\') -eq $destination.TrimEnd('\')) { throw 'Use a separate output directory' }
New-Item -ItemType Directory -Force $destination | Out-Null
Copy-Item (Join-Path $source 'zcnblk.sys'),(Join-Path $source 'zcnblk.inf') $destination -Force
& $SignToolPath sign /fd SHA256 /sha1 $CertificateThumbprint /ph (Join-Path $destination 'zcnblk.sys')
if ($LASTEXITCODE -ne 0) { throw 'SYS signing failed' }
& $Inf2CatPath "/driver:$destination" /os:10_X64 /verbose
if ($LASTEXITCODE -ne 0) { throw 'Inf2Cat validation failed' }
& $SignToolPath sign /fd SHA256 /sha1 $CertificateThumbprint (Join-Path $destination 'zcnblk.cat')
if ($LASTEXITCODE -ne 0) { throw 'Catalog signing failed' }
Write-Output "Signed package: $destination"
