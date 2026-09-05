//! Download and install an explicitly selected release, off the UI thread.

use super::{GITHUB_REPO, TIMEOUT};
use crate::wake::Wake;
use crossbeam_channel::{Receiver, Sender};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Binary,
    AppImage,
    WindowsSetup,
}

#[derive(Debug)]
pub struct PreparedUpdate {
    directory: tempfile::TempDir,
    file: PathBuf,
    target: PathBuf,
    format: Format,
}

#[derive(Debug)]
pub enum InstallOutcome {
    /// Relaunch this executable after the old application's shutdown completes.
    Restart(PathBuf),
    /// The installer will relaunch Pigtail after the application closes.
    InstallerStarted,
}

#[derive(Debug)]
pub enum InstallEvent {
    Progress { downloaded: u64, total: u64 },
    Downloaded(Result<PreparedUpdate, String>),
    Installed(Result<InstallOutcome, String>),
}

fn asset_name(version: &str, os: &str, arch: &str, format: Format) -> Result<String, String> {
    let version = version.strip_prefix('v').unwrap_or(version);
    if version.is_empty()
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
    {
        return Err("The release has an invalid version tag.".into());
    }
    match (os, arch, format) {
        ("windows", "x86_64", Format::WindowsSetup) => {
            Ok(format!("pigtail-v{version}-x86_64-setup.exe"))
        }
        ("windows", "x86_64", Format::Binary) => {
            Ok(format!("pigtail-v{version}-x86_64-pc-windows-msvc.exe"))
        }
        ("linux", "x86_64", Format::AppImage) => Ok(format!("pigtail-v{version}-x86_64.AppImage")),
        ("linux", "x86_64", Format::Binary) => {
            Ok(format!("pigtail-v{version}-x86_64-unknown-linux-gnu"))
        }
        _ => Err("Automatic updates are not available for this platform.".into()),
    }
}

fn destination() -> Result<(PathBuf, Format), String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    if cfg!(target_os = "linux") {
        if let Some(appimage) = std::env::var_os("APPIMAGE") {
            let path = PathBuf::from(appimage);
            if !path.is_absolute() || !path.is_file() {
                return Err("Could not locate the running AppImage.".into());
            }
            return Ok((path, Format::AppImage));
        }
    }
    if cfg!(windows)
        && executable
            .parent()
            .is_some_and(|dir| dir.join("unins000.exe").is_file())
    {
        return Ok((executable, Format::WindowsSetup));
    }
    Ok((executable, Format::Binary))
}

#[derive(Debug)]
struct Asset {
    url: String,
    size: u64,
    digest: String,
}

fn select_asset(json: &serde_json::Value, name: &str, version: &str) -> Result<Asset, String> {
    let asset = json["assets"].as_array()
        .and_then(|assets| assets.iter().find(|asset| asset["name"].as_str() == Some(name)))
        .ok_or_else(|| format!("This release does not contain {name}. Try again after the release finishes publishing."))?;
    let url = asset["browser_download_url"].as_str().unwrap_or_default();
    let expected = format!("https://github.com/{GITHUB_REPO}/releases/download/{version}/{name}");
    if url != expected {
        return Err("The release contains an unexpected download address.".into());
    }
    let size = asset["size"]
        .as_u64()
        .filter(|size| *size > 0 && *size <= MAX_DOWNLOAD)
        .ok_or("The update has an invalid download size.")?;
    let digest = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("This release has no SHA-256 checksum; it cannot be installed automatically.")?;
    Ok(Asset {
        url: url.into(),
        size,
        digest: digest.to_ascii_lowercase(),
    })
}

fn copy_verified(
    mut reader: impl Read,
    mut writer: impl Write,
    asset: &Asset,
    mut progress: impl FnMut(u64),
) -> Result<(), String> {
    let mut hash = Sha256::new();
    let mut downloaded = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|e| format!("Download interrupted: {e}"))?;
        if count == 0 {
            break;
        }
        downloaded += count as u64;
        if downloaded > asset.size {
            return Err("The download is larger than the published file.".into());
        }
        writer
            .write_all(&buffer[..count])
            .map_err(|e| format!("Could not save the update: {e}"))?;
        hash.update(&buffer[..count]);
        progress(downloaded);
    }
    if downloaded != asset.size {
        return Err("The download was incomplete. Please try again.".into());
    }
    if format!("{:x}", hash.finalize()) != asset.digest {
        return Err("The update checksum did not match. Please try again.".into());
    }
    Ok(())
}

fn download(
    version: &str,
    tx: &Sender<InstallEvent>,
    wake: &Wake,
) -> Result<PreparedUpdate, String> {
    let (target, format) = destination()?;
    let name = asset_name(
        version,
        std::env::consts::OS,
        std::env::consts::ARCH,
        format,
    )?;
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/tags/{version}");
    let body = ureq::get(&url)
        .header("User-Agent", concat!("pigtail/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .config()
        .timeout_global(Some(TIMEOUT))
        .build()
        .call()
        .map_err(|e| format!("Could not read the release: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let json = serde_json::from_str(&body).map_err(|e| format!("Invalid release metadata: {e}"))?;
    let asset = select_asset(&json, &name, version)?;
    let directory = tempfile::Builder::new()
        .prefix("pigtail-update-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let file = directory.path().join(&name);
    let mut output = std::fs::File::create(&file).map_err(|e| e.to_string())?;
    let mut response = ureq::get(&asset.url)
        .config()
        .timeout_global(Some(Duration::from_secs(600)))
        .build()
        .call()
        .map_err(|e| format!("Could not download the update: {e}"))?;
    let mut last_report = 0;
    copy_verified(
        response.body_mut().as_reader(),
        &mut output,
        &asset,
        |downloaded| {
            if downloaded - last_report >= 256 * 1024 || downloaded == asset.size {
                last_report = downloaded;
                let _ = tx.send(InstallEvent::Progress {
                    downloaded,
                    total: asset.size,
                });
                wake.signal();
            }
        },
    )?;
    output.sync_all().map_err(|e| e.to_string())?;
    Ok(PreparedUpdate {
        directory,
        file,
        target,
        format,
    })
}

pub fn spawn_download(version: String, wake: Wake) -> std::io::Result<Receiver<InstallEvent>> {
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("update-download".into())
        .spawn(move || {
            let result = download(&version, &tx, &wake);
            let _ = tx.send(InstallEvent::Downloaded(result));
            wake.signal();
        })?;
    Ok(rx)
}

impl PreparedUpdate {
    fn install(self) -> Result<InstallOutcome, String> {
        if self.format == Format::WindowsSetup {
            return self.launch_setup();
        }
        if self.format == Format::AppImage {
            replace_appimage(&self.file, &self.target)?;
        } else {
            self_replace::self_replace(&self.file).map_err(|e| format!(
                "Could not replace Pigtail: {e}. If it is installed in a protected folder or managed by a package manager, update it using its installer or package manager."
            ))?;
        }
        Ok(InstallOutcome::Restart(self.target))
    }

    #[cfg(windows)]
    fn launch_setup(self) -> Result<InstallOutcome, String> {
        use std::os::windows::process::CommandExt;
        let script = setup_script(
            &self.file,
            &self.target,
            self.directory.path(),
            std::process::id(),
        )?;
        let helper = self.directory.path().join("install.ps1");
        // Windows PowerShell needs the BOM for non-ASCII installation paths.
        std::fs::write(&helper, format!("\u{feff}{script}")).map_err(|e| e.to_string())?;
        std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&helper)
            .creation_flags(0x08000000) // CREATE_NO_WINDOW: only the installer has a window.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Could not start the installer: {e}"))?;
        // The helper owns cleanup after the application has exited.
        let _ = self.directory.keep();
        Ok(InstallOutcome::InstallerStarted)
    }

    #[cfg(not(windows))]
    fn launch_setup(self) -> Result<InstallOutcome, String> {
        let _ = &self.directory;
        Err("Windows installers cannot run on this platform.".into())
    }
}

#[cfg(windows)]
fn setup_script(
    file: &Path,
    target: &Path,
    directory: &Path,
    parent_pid: u32,
) -> Result<String, String> {
    fn quote(value: &Path) -> String {
        format!("'{}'", value.to_string_lossy().replace('\'', "''"))
    }
    let parent = target
        .parent()
        .ok_or("Could not locate the installation folder.")?;
    Ok(format!(
        r#"$ErrorActionPreference = 'Stop'
$dir = {dir}
$installer = {file}
$target = {target}
$staging = {staging}
try {{
    # Capture the parent before waiting so a subsequently reused PID cannot
    # cause us to wait on another application.
    $parent = Get-Process -Id {parent_pid} -ErrorAction SilentlyContinue
    if ($parent -and -not $parent.WaitForExit(120000)) {{
        throw 'Pigtail did not close in time. Please try updating again.'
    }}
    $key = Get-ItemProperty -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\{{374D0E66-90B2-4055-A852-0AF51237DA44}}_is1' -ErrorAction SilentlyContinue
    $scope = '/ALLUSERS'
    if ($key -and $key.InstallLocation.TrimEnd('\') -eq $dir) {{ $scope = '/CURRENTUSER' }}
    $process = Start-Process -FilePath $installer -ArgumentList @('/SILENT','/NORESTART','/NOCLOSEAPPLICATIONS','/NORESTARTAPPLICATIONS',$scope,('/DIR="'+$dir+'"')) -Wait -PassThru
    if ($process.ExitCode -ne 0) {{ throw "The installer returned exit code $($process.ExitCode)." }}
}} catch {{
    Add-Type -AssemblyName System.Windows.Forms
    [System.Windows.Forms.MessageBox]::Show($_.Exception.Message, 'Pigtail update failed') | Out-Null
}} finally {{
    # Relaunch as the original user, including after a cancelled UAC prompt.
    if (-not (Get-Process -Id {parent_pid} -ErrorAction SilentlyContinue)) {{
        try {{ Start-Process -FilePath $target -WorkingDirectory $dir }} catch {{
            Add-Type -AssemblyName System.Windows.Forms
            [System.Windows.Forms.MessageBox]::Show('Please open Pigtail again: ' + $_.Exception.Message, 'Pigtail update') | Out-Null
        }}
    }}
    # Delete only the explicitly created staging folder after setup has exited.
    $resolved = [IO.Path]::GetFullPath($staging)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
    if ($resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and
        [IO.Path]::GetFileName($resolved).StartsWith('pigtail-update-')) {{
        Remove-Item -LiteralPath $resolved -Recurse -Force -ErrorAction SilentlyContinue
    }}
}}
"#,
        dir = quote(parent),
        file = quote(file),
        target = quote(target),
        staging = quote(directory)
    ))
}

#[cfg(unix)]
fn replace_appimage(source: &Path, target: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let parent = target
        .parent()
        .ok_or("Could not locate the AppImage folder.")?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| format!("The AppImage folder is not writable: {e}"))?;
    std::io::copy(
        &mut std::fs::File::open(source).map_err(|e| e.to_string())?,
        &mut staged,
    )
    .map_err(|e| e.to_string())?;
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    staged.as_file().sync_all().map_err(|e| e.to_string())?;
    staged
        .persist(target)
        .map_err(|e| format!("Could not replace the AppImage: {e}"))?;
    Ok(())
}

#[cfg(not(unix))]
fn replace_appimage(_source: &Path, _target: &Path) -> Result<(), String> {
    Err("AppImage updates require Linux.".into())
}

pub fn spawn_install(
    update: PreparedUpdate,
    wake: Wake,
) -> std::io::Result<Receiver<InstallEvent>> {
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("update-install".into())
        .spawn(move || {
            let _ = tx.send(InstallEvent::Installed(update.install()));
            wake.signal();
        })?;
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_replacement_probe() {
        let Some(expected) = std::env::var_os("PIGTAIL_REPLACEMENT_PROBE") else {
            return;
        };
        let target = std::env::current_exe().unwrap();
        assert_eq!(
            target.canonicalize().unwrap(),
            PathBuf::from(expected).canonicalize().unwrap()
        );
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("replacement.exe");
        let mut bytes = std::fs::read(&target).unwrap();
        bytes.extend_from_slice(b"pigtail replacement test");
        std::fs::write(&file, bytes).unwrap();
        let prepared = PreparedUpdate {
            directory,
            file,
            target: target.clone(),
            format: Format::Binary,
        };
        assert!(
            matches!(prepared.install().unwrap(), InstallOutcome::Restart(path) if path == target)
        );
    }

    #[test]
    fn portable_update_replaces_only_the_isolated_running_copy() {
        let directory = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let original = std::fs::read(&executable).unwrap();
        let copy = directory.path().join("pigtail-probe.exe");
        std::fs::copy(&executable, &copy).unwrap();
        let mut command = std::process::Command::new(&copy);
        command
            .args([
                "--exact",
                "update::install::tests::portable_replacement_probe",
            ])
            .env("PIGTAIL_REPLACEMENT_PROBE", &copy);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(std::fs::read(copy)
            .unwrap()
            .ends_with(b"pigtail replacement test"));
        assert_eq!(std::fs::read(executable).unwrap(), original);
    }

    #[cfg(windows)]
    #[test]
    fn windows_helper_preserves_scope_quotes_paths_and_restarts_after_setup() {
        use std::os::windows::process::CommandExt;
        let directory = tempfile::Builder::new()
            .prefix("pigtail-update-")
            .tempdir()
            .unwrap();
        let target = directory
            .path()
            .join("O'Brien $() space")
            .join("pigtail.exe");
        let installer = directory.path().join("setup.exe");
        let log = directory.path().join("calls.jsonl");
        // Run the actual generated script with only OS side effects replaced.
        let mocks = r#"
function Get-Process { param($Id, $ErrorAction) return $null }
function Get-ItemProperty { param($LiteralPath, $ErrorAction) return @{ InstallLocation = $dir } }
function Start-Process {
    param($FilePath, $ArgumentList, [switch]$Wait, [switch]$PassThru, $WorkingDirectory)
    @{ file = $FilePath; arguments = $ArgumentList; waited = [bool]$Wait } |
        ConvertTo-Json -Compress | Add-Content -LiteralPath $env:PIGTAIL_UPDATE_TEST_LOG
    if ($Wait) { return @{ ExitCode = 0 } }
}
function Remove-Item { param($LiteralPath, [switch]$Recurse, [switch]$Force, $ErrorAction) }
"#;
        let helper = directory.path().join("test.ps1");
        let script = setup_script(&installer, &target, directory.path(), 12345).unwrap();
        std::fs::write(&helper, format!("\u{feff}{mocks}\n{script}")).unwrap();
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(helper)
            .env("PIGTAIL_UPDATE_TEST_LOG", &log)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let calls: Vec<serde_json::Value> = std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line.trim_start_matches('\u{feff}')).unwrap())
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["file"], installer.to_str().unwrap());
        assert_eq!(calls[0]["waited"], true);
        let arguments = calls[0]["arguments"].as_array().unwrap();
        assert!(arguments.contains(&serde_json::json!("/CURRENTUSER")));
        assert!(arguments.contains(&serde_json::json!(format!(
            "/DIR=\"{}\"",
            target.parent().unwrap().display()
        ))));
        assert_eq!(calls[1]["file"], target.to_str().unwrap());
    }

    #[test]
    fn matches_release_artifacts_and_rejects_other_platforms() {
        assert_eq!(
            asset_name("v1.2.3", "windows", "x86_64", Format::WindowsSetup).unwrap(),
            "pigtail-v1.2.3-x86_64-setup.exe"
        );
        assert_eq!(
            asset_name("v1.2.3", "windows", "x86_64", Format::Binary).unwrap(),
            "pigtail-v1.2.3-x86_64-pc-windows-msvc.exe"
        );
        assert_eq!(
            asset_name("v1.2.3", "linux", "x86_64", Format::AppImage).unwrap(),
            "pigtail-v1.2.3-x86_64.AppImage"
        );
        assert_eq!(
            asset_name("v1.2.3", "linux", "x86_64", Format::Binary).unwrap(),
            "pigtail-v1.2.3-x86_64-unknown-linux-gnu"
        );
        assert!(asset_name("v1.2.3", "linux", "aarch64", Format::Binary).is_err());
        assert!(asset_name("../evil", "windows", "x86_64", Format::Binary).is_err());
    }

    fn asset() -> Asset {
        Asset {
            url: String::new(),
            size: 3,
            digest: format!("{:x}", Sha256::digest(b"abc")),
        }
    }

    #[test]
    fn verifies_complete_download_before_installation() {
        let mut output = Vec::new();
        let mut progress = Vec::new();
        copy_verified(&b"abc"[..], &mut output, &asset(), |n| progress.push(n)).unwrap();
        assert_eq!(output, b"abc");
        assert_eq!(progress.last(), Some(&3));
        for invalid in [&b"ab"[..], &b"abcd"[..], &b"bad"[..]] {
            assert!(copy_verified(invalid, Vec::new(), &asset(), |_| {}).is_err());
        }
    }

    #[test]
    fn requires_exact_asset_trusted_url_size_and_checksum() {
        let name = "pigtail-v1.2.3-x86_64-setup.exe";
        let mut json = serde_json::json!({"assets": [{
            "name": name,
            "browser_download_url": format!("https://github.com/{GITHUB_REPO}/releases/download/v1.2.3/{name}"),
            "size": 3,
            "digest": format!("sha256:{}", asset().digest)
        }]});
        assert!(select_asset(&json, name, "v1.2.3").is_ok());
        assert!(select_asset(&json, "missing", "v1.2.3").is_err());
        assert!(select_asset(&json, name, "v1.2.4").is_err());
        json["assets"][0]["size"] = serde_json::json!(0);
        assert!(select_asset(&json, name, "v1.2.3").is_err());
        json["assets"][0]["size"] = serde_json::json!(3);
        json["assets"][0]["digest"] = serde_json::Value::Null;
        assert!(select_asset(&json, name, "v1.2.3").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn appimage_replacement_keeps_original_on_failure_and_sets_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Pigtail.AppImage");
        let source = dir.path().join("download");
        std::fs::write(&target, b"old").unwrap();
        assert!(replace_appimage(&source, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        std::fs::write(&source, b"new").unwrap();
        replace_appimage(&source, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(
            std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
