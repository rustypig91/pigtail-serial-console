<#
.SYNOPSIS
Build all Windows x64 release assets, downloading portable packaging tools as needed.
#>
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-Checked {
    param([string]$Command, [string[]]$Arguments)
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Command failed with exit code $LASTEXITCODE" }
}

if ($env:OS -ne 'Windows_NT') { throw 'Run this script on Windows.' }
foreach ($tool in @('cargo', 'rustup')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { throw "Install $tool first (see README.md)." }
}
$root = Split-Path $PSScriptRoot -Parent
$previousTargetDir = $env:CARGO_TARGET_DIR
Push-Location $root
try {
    $env:CARGO_TARGET_DIR = Join-Path $root 'target'
    $target = 'x86_64-pc-windows-msvc'
    $metadata = & cargo metadata --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $version = (($metadata | ConvertFrom-Json).packages | Where-Object name -eq 'pigtail').version
    $name = "pigtail-v$version-$target"
    $out = Join-Path $root "target/release-assets/$target"
    $toolDir = Join-Path $root 'target/release-tools'
    $wix = Join-Path $toolDir 'wix-3.14.1'
    $inno = Join-Path $toolDir 'inno-6.7.3'
    New-Item -ItemType Directory -Force -Path $out, $toolDir | Out-Null

    if (-not (Test-Path "$wix/light.exe")) {
        $zip = Join-Path $toolDir 'wix-3.14.1.zip'
        Invoke-WebRequest -UseBasicParsing 'https://github.com/wixtoolset/wix3/releases/download/wix3141rtm/wix314-binaries.zip' -OutFile $zip
        Expand-Archive -LiteralPath $zip -DestinationPath $wix -Force
    }
    if (-not (Test-Path "$inno/ISCC.exe")) {
        $installer = Join-Path $toolDir 'innosetup-6.7.3.exe'
        Invoke-WebRequest -UseBasicParsing 'https://github.com/jrsoftware/issrc/releases/download/is-6_7_3/innosetup-6.7.3.exe' -OutFile $installer
        $process = Start-Process -FilePath $installer -Wait -PassThru -WindowStyle Hidden -ArgumentList @(
            '/PORTABLE=1', '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/DIR=`"$inno`""
        )
        if ($process.ExitCode -ne 0) { throw "Unpacking Inno Setup failed: $($process.ExitCode)" }
    }
    Invoke-Checked rustup @('target', 'add', $target)
    Invoke-Checked cargo @('build', '--locked', '--release', '--workspace', '--target', $target)
    $bin = Join-Path $root "target/$target/release"
    $stage = Join-Path $root "target/release-staging/$name"
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    Copy-Item -LiteralPath "$bin/pigtail.exe", "$root/README.md", "$root/LICENSE" -Destination $stage -Force
    Compress-Archive -LiteralPath $stage -DestinationPath "$out/$name.zip" -Force
    Copy-Item -LiteralPath "$bin/pigtail.exe" -Destination "$out/$name.exe" -Force

    $obj = Join-Path $root 'target/release-staging/main.wixobj'
    Invoke-Checked "$wix/candle.exe" @(
        '-nologo', '-arch', 'x64', "-dVersion=$version", '-dPlatform=x64',
        "-dCargoTargetBinDir=$bin", '-out', $obj, "$root/crates/pigtail/wix/main.wxs"
    )
    Invoke-Checked "$wix/light.exe" @(
        '-nologo', '-ext', 'WixUIExtension', '-cultures:en-us',
        '-out', "$out/$name.msi", $obj
    )
    Invoke-Checked "$inno/ISCC.exe" @(
        "/DAppVersion=$version", "/DSourceBinDir=$bin", "/O$out",
        "$root/crates/pigtail/packaging/windows/pigtail.iss"
    )
    Write-Host "`nRelease assets: $out"
} finally {
    $env:CARGO_TARGET_DIR = $previousTargetDir
    Pop-Location
}
