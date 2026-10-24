# Exercise failures without building Rust binaries or requiring the Windows SDK.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$source = Join-Path $PSScriptRoot '../build-msix.ps1'
$tokens = $null
$parseErrors = $null
[void][System.Management.Automation.Language.Parser]::ParseFile($source, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw ($parseErrors | Out-String) }

$fixture = Join-Path ([System.IO.Path]::GetTempPath()) "pigtail-msix-test-$([guid]::NewGuid().ToString('N'))"
$previousOs = $env:OS
$previousTargetDir = $env:CARGO_TARGET_DIR
$previousFlags = $env:CARGO_ENCODED_RUSTFLAGS
$previousProcessDirectory = [Environment]::CurrentDirectory
New-Item -ItemType Directory -Path (Join-Path $fixture 'scripts') -Force | Out-Null
Copy-Item -LiteralPath $source -Destination (Join-Path $fixture 'scripts/build-msix.ps1')

# These functions are inherited by the child script, but never launch tools.
function cargo {
    if ($args[0] -eq 'metadata') {
        $global:LASTEXITCODE = 0
        '{"packages":[{"name":"pigtail","version":"1.4.0"}]}'
    } else { throw 'Simulated build failure' }
}
function rustup { $global:LASTEXITCODE = 0 }
function Get-Command {
    param([string]$Name, [string]$ErrorAction)
    if ($Name -in @('MakeAppx.exe', 'MakePri.exe')) {
        [pscustomobject]@{ Source = 'unused-sdk-tool' }
    } else { throw "Unexpected tool lookup: $Name" }
}
function Remove-Item {
    param([string]$LiteralPath, [switch]$Recurse, [switch]$Force)
    if ($failCleanup) { throw 'Simulated staging cleanup failure' }
    Microsoft.PowerShell.Management\Remove-Item -LiteralPath $LiteralPath -Recurse:$Recurse -Force:$Force
}

Push-Location ([System.IO.Path]::GetTempPath())
try {
    $env:OS = 'Windows_NT'
    # Keep the process directory different from PowerShell's current location
    # and the fixture root, as happens in interactive PowerShell sessions.
    [Environment]::CurrentDirectory = [System.IO.Path]::GetTempPath()
    foreach ($failCleanup in @($false, $true)) {
        $env:CARGO_TARGET_DIR = 'original-target'
        $env:CARGO_ENCODED_RUSTFLAGS = '--cfg=original_flags'
        $location = (Get-Location).Path
        $expected = if ($failCleanup) { 'Simulated staging cleanup failure' } else { 'Simulated build failure' }
        $failure = $null
        try {
            & (Join-Path $fixture 'scripts/build-msix.ps1') -OutputDirectory 'review-output'
        } catch { $failure = $_.Exception.Message }
        if ($failure -ne $expected) { throw "Expected '$expected', got '$failure'" }
        if (-not (Test-Path -LiteralPath (Join-Path $fixture 'review-output'))) {
            throw 'Relative output was not resolved against the repository root'
        }
        if ($env:CARGO_TARGET_DIR -ne 'original-target' -or $env:CARGO_ENCODED_RUSTFLAGS -ne '--cfg=original_flags') {
            throw 'Cargo environment was not restored after failure'
        }
        if ((Get-Location).Path -ne $location) { throw 'Caller location was not restored after failure' }
        $staging = @(Get-ChildItem -LiteralPath (Join-Path $fixture 'target/store') -Directory -Filter 'staging-*')
        if (-not $failCleanup -and $staging.Count) { throw 'Failed build left staging behind' }
    }
    Write-Host 'MSIX script regression checks passed (relative output, build failure, cleanup failure, environment and location restoration).'
} finally {
    $env:OS = $previousOs
    $env:CARGO_TARGET_DIR = $previousTargetDir
    $env:CARGO_ENCODED_RUSTFLAGS = $previousFlags
    [Environment]::CurrentDirectory = $previousProcessDirectory
    Pop-Location
    Microsoft.PowerShell.Management\Remove-Item -LiteralPath $fixture -Recurse -Force
}
