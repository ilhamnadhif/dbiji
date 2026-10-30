# Builds DBiji for Windows (x64) into dist\:
#   DBiji-<version>-windows-x64-setup.exe   installer (Inno Setup 6; per user, no administrator rights)
#   DBiji-<version>-windows-x64.zip         portable: dbiji.exe alone
#
#   pwsh scripts/bundle-windows.ps1            # builds the release binary first
#   pwsh scripts/bundle-windows.ps1 -NoBuild   # packages target\release\dbiji.exe
#
# Needs Rust (MSVC toolchain) and Inno Setup 6 (ISCC.exe; preinstalled on GitHub's Windows runners).
param([switch]$NoBuild)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$dist = Join-Path (Get-Location) 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null

if (-not $NoBuild) {
    # aws-lc (TLS) uses its prebuilt assembly when NASM is not installed.
    if (-not $env:AWS_LC_SYS_PREBUILT_NASM) { $env:AWS_LC_SYS_PREBUILT_NASM = '1' }
    Write-Host "==> cargo build --release ($version)"
    cargo build --release --locked -p dbiji-app
    if ($LASTEXITCODE) { throw 'cargo build failed' }
}
$exe = (Resolve-Path 'target\release\dbiji.exe').Path

# It starts and is the version being packaged (a GUI program: its output only
# reaches a redirected stream).
$out = Join-Path $env:TEMP "dbiji-version-$PID.txt"
$p = Start-Process -FilePath $exe -ArgumentList '--version' -RedirectStandardOutput $out -NoNewWindow -Wait -PassThru
$said = (Get-Content $out -Raw).Trim()
Remove-Item $out
if ($p.ExitCode -ne 0 -or $said -notmatch [regex]::Escape($version)) { throw "dbiji.exe --version said '$said' (exit $($p.ExitCode)), expected $version" }
Write-Host "==> $said"

$size = (Get-Item $exe).Length
Write-Host ("==> dbiji.exe: {0:N1} MB" -f ($size / 1MB))

$zip = Join-Path $dist "DBiji-$version-windows-x64.zip"
Write-Host "==> $zip"
Compress-Archive -Path $exe -DestinationPath $zip -CompressionLevel Optimal -Force

$iscc = @("${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 (ISCC.exe) is not installed' }
$base = "DBiji-$version-windows-x64-setup"
Write-Host "==> $dist\$base.exe"
& $iscc /Qp "/DAppVersion=$version" "/DSourceExe=$exe" "/DOutputDir=$dist" "/DOutputBaseFilename=$base" 'packaging\windows\dbiji.iss'
if ($LASTEXITCODE) { throw 'ISCC failed' }

Get-ChildItem $dist -Filter "DBiji-$version-windows-*" | ForEach-Object { Write-Host ("    {0}  {1:N1} MB" -f $_.Name, ($_.Length / 1MB)) }
