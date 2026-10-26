//! Read Windows Installer ownership without starting installation or repair.

use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
use windows_sys::Win32::System::ApplicationInstallationAndServicing::{
    MsiEnumRelatedProductsW, MsiGetComponentPathW, INSTALLSTATE_ABSENT, INSTALLSTATE_LOCAL,
    INSTALLSTATE_MOREDATA, INSTALLSTATE_SOURCE, INSTALLSTATE_UNKNOWN,
};

// Keep these identities in sync with wix/main.wxs. The component GUID is the
// original WiX-generated identity, also used by already released x64 MSIs.
const UPGRADE_CODE: &str = "{2A228B4C-BDDD-4A49-837E-0F43B737EBFA}";
const EXECUTABLE_COMPONENT: &str = "{545A7A2D-8DBD-5B22-AD6C-E11BD0B0C13C}";

pub(super) fn owns(executable: &Path) -> Result<bool, String> {
    let upgrade: Vec<u16> = UPGRADE_CODE.encode_utf16().chain(Some(0)).collect();
    let component: Vec<u16> = EXECUTABLE_COMPONENT.encode_utf16().chain(Some(0)).collect();
    for index in 0.. {
        // GUID including braces and terminator.
        let mut product = [0u16; 39];
        // SAFETY: terminated input and an output buffer of the documented size.
        let status =
            unsafe { MsiEnumRelatedProductsW(upgrade.as_ptr(), 0, index, product.as_mut_ptr()) };
        match status {
            ERROR_NO_MORE_ITEMS => return Ok(false),
            ERROR_SUCCESS => {}
            _ => {
                return Err(format!(
                    "Could not query Windows Installer products ({status})."
                ))
            }
        }
        let mut path = vec![0u16; 260];
        loop {
            let mut length = path.len() as u32;
            // SAFETY: both GUIDs are terminated; length describes the output buffer.
            let state = unsafe {
                MsiGetComponentPathW(
                    product.as_ptr(),
                    component.as_ptr(),
                    path.as_mut_ptr(),
                    &mut length,
                )
            };
            match state {
                INSTALLSTATE_MOREDATA => {
                    path.resize(length as usize + 1, 0);
                }
                INSTALLSTATE_LOCAL | INSTALLSTATE_SOURCE => {
                    let installed =
                        PathBuf::from(std::ffi::OsString::from_wide(&path[..length as usize]));
                    if same_executable(executable, &installed)? {
                        return Ok(true);
                    }
                    break;
                }
                // Advertised/absent products do not own a running executable.
                INSTALLSTATE_ABSENT | INSTALLSTATE_UNKNOWN => break,
                _ => {
                    return Err(format!(
                        "Could not query the Windows Installer executable ({state})."
                    ))
                }
            }
        }
    }
    unreachable!()
}

fn same_executable(executable: &Path, installed: &Path) -> Result<bool, String> {
    // current_exe and MSI can spell the same path differently (case, short names,
    // junctions, or the extended-path prefix). Canonicalize both before comparing.
    let executable = executable
        .canonicalize()
        .map_err(|e| format!("Could not resolve the running executable: {e}"))?;
    let installed = installed
        .canonicalize()
        .map_err(|e| format!("Could not resolve the MSI executable: {e}"))?;
    Ok(executable == installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installer_identities_match_the_package_and_legacy_component() {
        let wix = include_str!("../../pigtail/wix/main.wxs");
        assert!(wix.contains(&format!(
            "UpgradeCode=\"{}\"",
            UPGRADE_CODE.trim_matches(['{', '}'])
        )));
        assert!(wix.contains(&format!(
            "<Component Id=\"MainExecutable\" Guid=\"{EXECUTABLE_COMPONENT}\""
        )));
    }

    #[test]
    fn ownership_requires_the_same_executable_path() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("Pigtail.exe");
        let portable = directory.path().join("portable.exe");
        std::fs::write(&executable, b"same binary").unwrap();
        std::fs::copy(&executable, &portable).unwrap();
        assert!(same_executable(&executable, &directory.path().join("PIGTAIL.EXE")).unwrap());
        assert!(!same_executable(&portable, &executable).unwrap());
        assert!(same_executable(&executable, &directory.path().join("missing.exe")).is_err());
    }
}
