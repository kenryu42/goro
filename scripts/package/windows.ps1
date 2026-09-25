# Build the Windows package: a zip with goro.exe (icon and manifest embedded).
#
# Usage: scripts/package/windows.ps1 -Version <version> -Target <x86_64|aarch64>-pc-windows-msvc -Out <dir>
#
# Signs goro.exe when WINDOWS_CERTIFICATE (base64 .pfx) and WINDOWS_CERTIFICATE_PASSWORD
# are set; otherwise the build is unsigned (SmartScreen will warn).
param(
    [Parameter(Mandatory)] [string] $Version,
    [Parameter(Mandatory)] [string] $Target,
    [Parameter(Mandatory)] [string] $Out
)
$ErrorActionPreference = "Stop"
$root = Resolve-Path "$PSScriptRoot/../.."
Set-Location $root
New-Item -ItemType Directory -Force -Path $Out | Out-Null

cargo build --release --locked -p goro --target $Target
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$exe = "target/$Target/release/goro.exe"

if ($env:WINDOWS_CERTIFICATE) {
    $pfx = Join-Path $env:RUNNER_TEMP "goro-signing.pfx"
    [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($env:WINDOWS_CERTIFICATE))
    signtool sign /f $pfx /p $env:WINDOWS_CERTIFICATE_PASSWORD /tr http://timestamp.digicert.com /td sha256 /fd sha256 $exe
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Remove-Item $pfx
} else {
    Write-Warning "WINDOWS_CERTIFICATE not set; goro.exe is unsigned"
}

$arch = $Target.Split("-")[0]
$staging = Join-Path ([IO.Path]::GetTempPath()) "goro-$Version-windows-$arch"
New-Item -ItemType Directory -Force -Path $staging | Out-Null
Copy-Item $exe, README.md $staging
$zip = Join-Path (Resolve-Path $Out) "goro-$Version-windows-$arch.zip"
Compress-Archive -Path "$staging/*" -DestinationPath $zip -Force
Remove-Item -Recurse $staging
Write-Output $zip
