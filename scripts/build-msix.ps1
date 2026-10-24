# Build an unsigned Microsoft Store submission; run on Windows with its SDK.
[CmdletBinding()]
param(
    # Store reserves the fourth component for itself. Override only to produce
    # a higher package version when repackaging the same application release.
    [string]$PackageVersion,
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-Checked([string]$Tool, [string[]]$Arguments) {
    & $Tool @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Tool failed with exit code $LASTEXITCODE" }
}

function Find-SdkTool([string]$Name) {
    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    $sdkBin = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits/10/bin'
    if (Test-Path -LiteralPath $sdkBin) {
        $sdk = Get-ChildItem -LiteralPath $sdkBin -Directory |
            Where-Object { $_.Name -match '^10\.\d+\.\d+\.\d+$' -and (Test-Path (Join-Path $_.FullName "x64/$Name")) } |
            Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
        if ($sdk) { return Join-Path $sdk.FullName "x64/$Name" }
    }
    throw "Install the Windows 10/11 SDK, including $Name."
}

if ($env:OS -ne 'Windows_NT') { throw 'Run this script on Windows.' }
$root = Split-Path $PSScriptRoot -Parent
$target = 'x86_64-pc-windows-msvc'

# Keep Store and direct-download build outputs separate. Use a static CRT so
# the package does not rely on a separately installed VC++ redistributable.
$previousTargetDir = $env:CARGO_TARGET_DIR
$previousFlags = $env:CARGO_ENCODED_RUSTFLAGS
$work = $null
Push-Location $root
try {
    $env:CARGO_TARGET_DIR = Join-Path $root 'target/store'
    $flags = if ($previousFlags) { $previousFlags } else { $env:RUSTFLAGS }
    # Plain RUSTFLAGS are whitespace separated; encoded flags use unit separators.
    if (-not $previousFlags -and $flags) {
        $flags = (($flags -split '\s+') | Where-Object { $_.Length -gt 0 }) -join [char]31
    }
    $staticCrt = '-Ctarget-feature=+crt-static'
    $env:CARGO_ENCODED_RUSTFLAGS = if ($flags) { "$flags$([char]31)$staticCrt" } else { $staticCrt }

    $metadata = & cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $version = (($metadata | ConvertFrom-Json).packages | Where-Object name -eq 'pigtail').version
    if (-not $PackageVersion) {
        if ($version -notmatch '^\d+\.\d+\.\d+$') { throw 'Store releases require a numeric major.minor.patch version.' }
        $PackageVersion = "$version.0"
    }
    if ($PackageVersion -notmatch '^([1-9]\d*)\.(\d+)\.(\d+)\.0$') {
        throw 'PackageVersion must be major.minor.patch.0, with a nonzero major version.'
    }
    foreach ($component in $PackageVersion.Split('.')) {
        if ([uint64]$component -gt 65535) { throw 'Each package version component must be at most 65535.' }
    }

    # SDK tools are not normally on PATH outside a Developer PowerShell prompt.
    $makeAppxPath = Find-SdkTool 'MakeAppx.exe'
    $makePriPath = Find-SdkTool 'MakePri.exe'

    if (-not $OutputDirectory) { $OutputDirectory = Join-Path $root 'target/release-assets/store' }
    $OutputDirectory = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory)
    New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
    # An isolated staging directory cannot accidentally retain old binaries.
    $work = Join-Path $root "target/store/staging-$([guid]::NewGuid().ToString('N'))"
    $stage = Join-Path $work 'layout'
    $assets = Join-Path $stage 'Assets'
    New-Item -ItemType Directory -Force -Path $assets | Out-Null

    Invoke-Checked rustup @('target', 'add', $target)
    Invoke-Checked cargo @('build', '--release', '--locked', '-p', 'pigtail', '--target', $target, '--no-default-features', '--features', 'microsoft-store')
    Copy-Item -LiteralPath "$env:CARGO_TARGET_DIR/$target/release/pigtail.exe", "$root/LICENSE" -Destination $stage
    $template = Get-Content -LiteralPath "$root/crates/pigtail/packaging/windows/msix/AppxManifest.xml" -Raw -Encoding utf8
    $manifest = $template.Replace('@VERSION@', $PackageVersion)
    [System.IO.File]::WriteAllText((Join-Path $stage 'AppxManifest.xml'), $manifest, [System.Text.UTF8Encoding]::new($false))

    # Derive Store tiles from the existing application artwork, without another
    # image-tool dependency. Include 200% assets for high-DPI displays.
    Add-Type -AssemblyName System.Drawing
    $image = [System.Drawing.Image]::FromFile("$root/crates/pigtail/packaging/icons/hicolor/512x512/apps/pigtail.png")
    try {
        foreach ($asset in @(@('StoreLogo', 50), @('Square44x44Logo', 44), @('Square150x150Logo', 150))) {
            foreach ($scale in @(1, 2)) {
                $size = [int]$asset[1] * $scale
                $name = if ($scale -eq 1) { "$($asset[0]).png" } else { "$($asset[0]).scale-200.png" }
                $bitmap = [System.Drawing.Bitmap]::new($size, $size)
                try {
                    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
                    try {
                        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
                        $graphics.DrawImage($image, 0, 0, $size, $size)
                    } finally { $graphics.Dispose() }
                    $bitmap.Save((Join-Path $assets $name), [System.Drawing.Imaging.ImageFormat]::Png)
                } finally { $bitmap.Dispose() }
            }
        }
    } finally { $image.Dispose() }

    # Index the scale-qualified images so Windows selects the high-DPI assets.
    # Keep one self-contained package instead of splitting resource packages.
    $priConfig = Join-Path $work 'priconfig.xml'
    Invoke-Checked $makePriPath @('createconfig', '/cf', $priConfig, '/dq', 'en-US', '/pv', '10.0.0', '/o')
    [xml]$priXml = Get-Content -LiteralPath $priConfig -Raw -Encoding utf8
    $packaging = $priXml.SelectSingleNode('/resources/packaging')
    if ($packaging) { [void]$packaging.ParentNode.RemoveChild($packaging) }
    $priXml.Save($priConfig)
    Invoke-Checked $makePriPath @('new', '/pr', $stage, '/cf', $priConfig, '/of', (Join-Path $stage 'resources.pri'), '/o')

    $package = Join-Path $OutputDirectory "pigtail-store-$PackageVersion-x64.msix"
    Invoke-Checked $makeAppxPath @('pack', '/o', '/h', 'SHA256', '/d', $stage, '/p', $package)
    Write-Host "Store submission package (unsigned): $package"
    Write-Host 'Upload this MSIX to Partner Center. Microsoft signs it after certification.'
} finally {
    try {
        if ($work -and (Test-Path -LiteralPath $work)) { Remove-Item -LiteralPath $work -Recurse -Force }
    } finally {
        $env:CARGO_TARGET_DIR = $previousTargetDir
        $env:CARGO_ENCODED_RUSTFLAGS = $previousFlags
        Pop-Location
    }
}
