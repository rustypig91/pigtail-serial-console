# Native, non-elevated Windows Installer ownership/upgrade regression.
# Uses two temporary per-user packages containing the serialcore test executable.
# Refuses to run alongside any real Pigtail MSI registration.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$CoreTestExe,
    [string]$WixDirectory = 'target/release-tools/wix-3.14.1'
)
$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this smoke test without elevation.'
}
$installer = New-Object -ComObject WindowsInstaller.Installer
$upgradeCode = '{2A228B4C-BDDD-4A49-837E-0F43B737EBFA}'
if (@($installer.RelatedProducts($upgradeCode)).Count -ne 0) {
    throw 'A Pigtail MSI is registered. Run this test in a clean user/VM.'
}
$source = (Resolve-Path -LiteralPath $CoreTestExe).Path
$wix = (Resolve-Path -LiteralPath $WixDirectory).Path
$stage = Join-Path $root ('target/msi-update-smoke/' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stage -Force | Out-Null
$installDir = Join-Path $stage 'installed'
$codes = @(('{' + [guid]::NewGuid().ToString().ToUpperInvariant() + '}'),
           ('{' + [guid]::NewGuid().ToString().ToUpperInvariant() + '}'))
$previousProbe = $env:PIGTAIL_MSI_UPDATE_PROBE
$previousReplacement = $env:PIGTAIL_REPLACEMENT_PROBE

function Invoke-Msi([string[]]$MsiArguments) {
    $process = Start-Process -FilePath 'msiexec.exe' -ArgumentList $MsiArguments -Wait -PassThru -WindowStyle Hidden
    if ($process.ExitCode -ne 0) { throw "msiexec failed ($($process.ExitCode)); see $stage logs." }
}
function Invoke-Probe([string]$Exe, [string]$TestName) {
    $process = Start-Process -FilePath $Exe -ArgumentList @('--exact', $TestName, '--nocapture') -Wait -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $stage 'probe.stdout.log') -RedirectStandardError (Join-Path $stage 'probe.stderr.log')
    if ($process.ExitCode -ne 0) {
        Get-Content -LiteralPath (Join-Path $stage 'probe.stdout.log'), (Join-Path $stage 'probe.stderr.log')
        throw "Probe failed ($($process.ExitCode))."
    }
    if (-not (Select-String -LiteralPath (Join-Path $stage 'probe.stdout.log') -SimpleMatch '1 passed; 0 failed')) {
        throw 'The expected probe test did not run.'
    }
}
try {
    for ($revision = 0; $revision -lt 2; $revision++) {
        $version = "1.0.$revision"
        $code = $codes[$revision]
        $wxs = Join-Path $stage "probe-$revision.wxs"
        $object = Join-Path $stage "probe-$revision.wixobj"
        $msi = Join-Path $stage "probe-$revision.msi"
        $xmlSource = [Security.SecurityElement]::Escape($source)
        $xmlDestination = [Security.SecurityElement]::Escape($installDir)
        # Same upgrade/component identities as all released x64 Pigtail MSIs;
        # a per-user fixture avoids elevation or modifying Program Files.
        @"
<Wix xmlns="http://schemas.microsoft.com/wix/2006/wi">
  <Product Id="$code" Name="Pigtail updater smoke test" Language="1033" Version="$version"
           Manufacturer="Pigtail tests" UpgradeCode="$upgradeCode">
    <Package InstallerVersion="450" Compressed="yes" InstallScope="perUser" />
    <MajorUpgrade Schedule="afterInstallInitialize" DowngradeErrorMessage="Newer smoke test installed." />
    <MediaTemplate EmbedCab="yes" CompressionLevel="none" />
    <Directory Id="TARGETDIR" Name="SourceDir">
      <Directory Id="LocalAppDataFolder">
        <Directory Id="APPLICATIONFOLDER" Name="PigtailSmoke">
          <Component Id="MainExecutable" Guid="{545A7A2D-8DBD-5B22-AD6C-E11BD0B0C13C}">
            <File Id="PigtailExe" Name="pigtail.exe" Source="$xmlSource" KeyPath="yes" />
          </Component>
        </Directory>
      </Directory>
    </Directory>
    <Property Id="APPLICATIONFOLDER" Value="$xmlDestination" />
    <Feature Id="Complete" Level="1"><ComponentRef Id="MainExecutable" /></Feature>
  </Product>
</Wix>
"@ | Set-Content -LiteralPath $wxs -Encoding UTF8
        & (Join-Path $wix 'candle.exe') -nologo -arch x64 -out $object $wxs
        if ($LASTEXITCODE -ne 0) { throw 'candle failed.' }
        # ICE38 requires an HKCU key path for per-user packages; this fixture
        # deliberately retains production's executable key path for the probe.
        & (Join-Path $wix 'light.exe') -nologo -sice:ICE38 -sice:ICE64 -out $msi $object
        if ($LASTEXITCODE -ne 0) { throw 'light failed.' }
        Invoke-Msi @('/i', ('"' + $msi + '"'), '/qn', '/norestart', '/l*v', ('"' + (Join-Path $stage "install-$revision.log") + '"'))
        if ($installer.ProductInfo($code, 'VersionString') -ne $version) { throw 'MSI version mismatch.' }
        $env:PIGTAIL_MSI_UPDATE_PROBE = Join-Path $installDir 'pigtail.exe'
        Invoke-Probe $env:PIGTAIL_MSI_UPDATE_PROBE 'update::install::tests::msi_owned_update_probe'
        if ($revision -eq 1 -and @($installer.RelatedProducts($upgradeCode)).Count -ne 1) {
            throw 'The MSI upgrade did not remove the old product registration.'
        }
        # An identical portable copy must still be allowed to self-replace even
        # while the MSI copy is registered elsewhere on the same computer.
        $portable = Join-Path $stage 'portable.exe'
        Copy-Item -LiteralPath $source -Destination $portable -Force
        $env:PIGTAIL_REPLACEMENT_PROBE = $portable
        Invoke-Probe $portable 'update::install::tests::portable_replacement_probe'
    }
    Write-Host "MSI ownership, manual-update guards, portable coexistence and MSI upgrade passed. Logs: $stage"
} finally {
    $env:PIGTAIL_MSI_UPDATE_PROBE = $previousProbe
    $env:PIGTAIL_REPLACEMENT_PROBE = $previousReplacement
    foreach ($code in $codes) {
        if (@($installer.RelatedProducts($upgradeCode)) -contains $code) {
            Invoke-Msi @('/x', $code, '/qn', '/norestart', '/l*v', ('"' + (Join-Path $stage 'uninstall.log') + '"'))
        }
    }
    [Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) | Out-Null
}
