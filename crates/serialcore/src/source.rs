//! The [`ByteSource`] trait and its implementations.
//!
//! Everything downstream of a `ByteSource` (framing, storage, extraction) is
//! identical whether bytes come from a live serial port or a scripted test
//! fixture. This is what makes the pipeline testable without hardware
//! (spec §10).

use crate::config::PortConfig;
use std::io::{Read, Write};
use std::time::Duration;

/// Errors a source can surface. A `Disconnected` is the signal the reader uses
/// to transition to the `Lost` state and begin reconnecting.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The device went away (unplugged, reset). Recoverable via reconnect.
    #[error("source disconnected: {0}")]
    Disconnected(String),

    /// Opening the device failed. On Linux, permission-denied errors carry a
    /// hint naming whichever group actually owns the device node.
    #[error("{0}")]
    Open(String),

    /// Any other IO error while reading.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A source of raw bytes with a bounded read timeout.
///
/// `read` blocks up to the source's internal timeout and returns the number of
/// bytes placed in `buf`. A timeout with no data available returns `Ok(0)` — it
/// is not an error. A truly gone device returns `Err(SourceError::Disconnected)`.
pub trait ByteSource: Send {
    /// Read available bytes into `buf`. Returns `Ok(0)` on timeout.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, SourceError>;

    /// Human-readable description for logs and the UI.
    fn description(&self) -> String;

    /// Write bytes to the device (transmit). Sources that cannot transmit
    /// (e.g. scripted test fixtures) return an error.
    fn write(&mut self, _bytes: &[u8]) -> Result<(), SourceError> {
        Err(SourceError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this source is read-only",
        )))
    }

    /// Set the DTR control line. No-op for non-serial sources.
    fn set_dtr(&mut self, _on: bool) -> Result<(), SourceError> {
        Ok(())
    }

    /// Set the RTS control line. No-op for non-serial sources.
    fn set_rts(&mut self, _on: bool) -> Result<(), SourceError> {
        Ok(())
    }

    /// Send a serial break. No-op for non-serial sources.
    fn send_break(&mut self) -> Result<(), SourceError> {
        Ok(())
    }
}

/// Read timeout: kept short so the reader loop stays responsive to shutdown
/// and reconnect checks (spec §5).
const READ_TIMEOUT: Duration = Duration::from_millis(10);

/// Timeout borrowed for the duration of a write. `serialport`'s Windows
/// backend has one shared COMMTIMEOUTS setting, so `WriteTotalTimeoutConstant`
/// is set to whatever the read timeout is; leaving it at 10ms made any write
/// that didn't complete that fast (flow control stalls, USB-serial latency)
/// fail with "The semaphore timeout period has expired" (Windows error 121).
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// A live serial port opened via the `serialport` crate.
pub struct SerialSource {
    port: Box<dyn serialport::SerialPort>,
    path: String,
}

impl SerialSource {
    /// Open `path` with `config`.
    pub fn open(path: &str, config: &PortConfig) -> Result<SerialSource, SourceError> {
        let builder = serialport::new(path, config.baud)
            .data_bits(config.data_bits.into())
            .parity(config.parity.into())
            .stop_bits(config.stop_bits.into())
            .flow_control(config.flow_control.into())
            .timeout(READ_TIMEOUT);

        let mut port = builder.open().map_err(|e| map_open_error(path, e))?;

        // Toggling these lines resets many boards; apply the configured state.
        let _ = port.write_data_terminal_ready(config.dtr_on_open);
        let _ = port.write_request_to_send(config.rts_on_open);

        Ok(SerialSource {
            port,
            path: path.to_string(),
        })
    }
}

impl ByteSource for SerialSource {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, SourceError> {
        match self.port.read(buf) {
            Ok(0) => Ok(0),
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Ok(0),
            Err(e) if is_disconnect(&e) => {
                Err(SourceError::Disconnected(format!("{}: {e}", self.path)))
            }
            Err(e) => Err(SourceError::Io(e)),
        }
    }

    fn description(&self) -> String {
        format!("serial {}", self.path)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), SourceError> {
        // Widen the timeout for the write itself; see WRITE_TIMEOUT.
        let _ = self.port.set_timeout(WRITE_TIMEOUT);
        let result = self.port.write_all(bytes).and_then(|_| self.port.flush());
        let _ = self.port.set_timeout(READ_TIMEOUT);
        result?;
        Ok(())
    }

    fn set_dtr(&mut self, on: bool) -> Result<(), SourceError> {
        self.port
            .write_data_terminal_ready(on)
            .map_err(|e| SourceError::Io(std::io::Error::other(e.to_string())))
    }

    fn set_rts(&mut self, on: bool) -> Result<(), SourceError> {
        self.port
            .write_request_to_send(on)
            .map_err(|e| SourceError::Io(std::io::Error::other(e.to_string())))
    }

    fn send_break(&mut self) -> Result<(), SourceError> {
        // A ~250ms break is long enough for firmware break-detect logic.
        self.port
            .set_break()
            .map_err(|e| SourceError::Io(std::io::Error::other(e.to_string())))?;
        std::thread::sleep(Duration::from_millis(250));
        self.port
            .clear_break()
            .map_err(|e| SourceError::Io(std::io::Error::other(e.to_string())))
    }
}

fn is_disconnect(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        e.kind(),
        BrokenPipe | NotConnected | UnexpectedEof | ConnectionAborted | ConnectionReset
    ) || {
        // serialport surfaces device-gone as an OS error whose raw code varies;
        // treat "No such file or directory" / "device not configured" as gone.
        let s = e.to_string().to_lowercase();
        s.contains("no such")
            || s.contains("not configured")
            || s.contains("disconnected")
            || s.contains("access is denied") // Windows: handle invalidated on unplug
    }
}

fn map_open_error(path: &str, e: serialport::Error) -> SourceError {
    // POSIX surfaces EACCES as this exact kind, so no string matching needed.
    if e.kind() == serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied) {
        return SourceError::Open(format!(
            "{path}: permission denied. {}",
            permission_hint(path)
        ));
    }
    if e.kind() == serialport::ErrorKind::NoDevice {
        // On Windows, serialport-rs maps ERROR_ACCESS_DENIED to the same
        // ErrorKind::NoDevice as "no such device", and its message text is
        // localized (FormatMessageW), so the two can't be told apart by
        // string matching. If the port still shows up in the OS port
        // enumeration, the failure is a permission/exclusivity problem, not
        // the device being gone.
        if windows_port_still_present(path) {
            return SourceError::Open(format!(
                "{path}: permission denied. {}",
                permission_hint(path)
            ));
        }
        return SourceError::Open(format!("{path}: device not present"));
    }
    SourceError::Open(format!("{path}: {}", e))
}

#[cfg(target_os = "windows")]
fn windows_port_still_present(path: &str) -> bool {
    serialport::available_ports()
        .map(|ports| ports.iter().any(|p| p.port_name == path))
        .unwrap_or(false)
}

#[cfg(not(target_os = "windows"))]
fn windows_port_still_present(_path: &str) -> bool {
    false
}

/// The group that actually owns `path`, not a guess: distros differ on which
/// group gates serial devices (`dialout` on Debian/Ubuntu, `uucp` on Arch,
/// `lock`/`tty` elsewhere), so naming the wrong one would send a user down a
/// dead end.
#[cfg(target_os = "linux")]
fn permission_hint(path: &str) -> String {
    match device_group_name(path) {
        Some(group) => format!(
            "Add yourself to the '{group}' group: `sudo usermod -aG {group} $USER`, \
             then log out and back in."
        ),
        None => format!(
            "Add yourself to the group that owns the device (`ls -l {path}` shows which), \
             then log out and back in."
        ),
    }
}

#[cfg(not(target_os = "linux"))]
fn permission_hint(_path: &str) -> String {
    "Check that your account has permission to access this device.".to_string()
}

/// Resolve the group that owns the device node at `path` via `getent`, which
/// consults the same source (files, LDAP, sssd, ...) the system itself uses —
/// unlike parsing `/etc/group` directly, this stays correct wherever group
/// lookups are backed by something other than a local file.
///
/// The `stat` is repeated on every call (cheap, and keeps the reported group
/// current if the device's owning group changes mid-outage); only the
/// gid-to-name lookup is cached, since that mapping is a machine-wide fact
/// that isn't specific to a path.
#[cfg(target_os = "linux")]
fn device_group_name(path: &str) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let gid = std::fs::metadata(path).ok()?.gid();
    group_name_for_gid(gid)
}

/// Cached per gid: a reader stuck on a permission-denied device retries the
/// open every backoff cycle, and without a cache that would spawn `getent`
/// again on every single attempt for as long as the outage lasts.
#[cfg(target_os = "linux")]
fn group_name_for_gid(gid: u32) -> Option<String> {
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<u32, Option<String>>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
    }
    CACHE.with(|cache| {
        if let Some(cached) = cache.borrow().get(&gid) {
            return cached.clone();
        }
        let result = getent_group_name(gid);
        cache.borrow_mut().insert(gid, result.clone());
        result
    })
}

/// Runs `getent group <gid>`, bounded by a timeout so a reader thread can
/// never hang here: `getent` can block indefinitely if it's backed by an
/// unreachable network directory service (LDAP/sssd), and this call happens
/// inside the reader thread's open-retry loop, which must stay responsive to
/// `Shutdown` (see `ReaderHandle::shutdown_in_place`).
#[cfg(target_os = "linux")]
fn getent_group_name(gid: u32) -> Option<String> {
    use std::io::Read;

    const TIMEOUT: Duration = Duration::from_millis(500);
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    let mut child = std::process::Command::new("getent")
        .arg("group")
        .arg(gid.to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let deadline = std::time::Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait().ok()? {
            Some(status) => break status,
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut stdout = String::new();
    child.stdout.take()?.read_to_string(&mut stdout).ok()?;
    let name = stdout.split(':').next()?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// A scripted source for tests: a list of `(bytes, delay)` pairs delivered in
/// order. After the script is exhausted it returns `Ok(0)` forever (or
/// `Disconnected` if constructed with [`ScriptedSource::eof_when_done`]).
pub struct ScriptedSource {
    steps: std::collections::VecDeque<(Vec<u8>, Duration)>,
    carry: Vec<u8>,
    eof_when_done: bool,
    honor_delays: bool,
}

impl ScriptedSource {
    /// Build from `(bytes, delay)` steps. Delays are honored (the source sleeps
    /// before returning each step) so timing-sensitive behaviour can be tested.
    pub fn new(steps: Vec<(Vec<u8>, Duration)>) -> ScriptedSource {
        ScriptedSource {
            steps: steps.into(),
            carry: Vec::new(),
            eof_when_done: false,
            honor_delays: true,
        }
    }

    /// Like [`ScriptedSource::new`] but returns `Disconnected` once exhausted,
    /// which is useful for exercising the reconnect path.
    pub fn eof_when_done(mut self) -> ScriptedSource {
        self.eof_when_done = true;
        self
    }

    /// Ignore the per-step delays (deliver as fast as possible). Handy for
    /// throughput tests that don't want real sleeps.
    pub fn no_delays(mut self) -> ScriptedSource {
        self.honor_delays = false;
        self
    }
}

impl ByteSource for ScriptedSource {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, SourceError> {
        if self.carry.is_empty() {
            match self.steps.pop_front() {
                Some((bytes, delay)) => {
                    if self.honor_delays && !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                    self.carry = bytes;
                }
                None => {
                    if self.eof_when_done {
                        return Err(SourceError::Disconnected("script exhausted".into()));
                    }
                    return Ok(0);
                }
            }
        }
        let n = self.carry.len().min(buf.len());
        buf[..n].copy_from_slice(&self.carry[..n]);
        self.carry.drain(..n);
        Ok(n)
    }

    fn description(&self) -> String {
        "scripted".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_delivers_in_order_and_respects_buf_size() {
        let mut s = ScriptedSource::new(vec![
            (b"hello".to_vec(), Duration::ZERO),
            (b"world".to_vec(), Duration::ZERO),
        ])
        .no_delays();

        let mut buf = [0u8; 3];
        let n = s.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hel");
        let n = s.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"lo");
        let n = s.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"wor");
    }

    #[test]
    fn scripted_returns_zero_when_done() {
        let mut s = ScriptedSource::new(vec![]).no_delays();
        let mut buf = [0u8; 8];
        assert_eq!(s.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn scripted_eof_when_done_disconnects() {
        let mut s = ScriptedSource::new(vec![]).no_delays().eof_when_done();
        let mut buf = [0u8; 8];
        assert!(matches!(
            s.read(&mut buf),
            Err(SourceError::Disconnected(_))
        ));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn device_group_name_resolves_the_files_actual_group() {
        use std::os::unix::fs::MetadataExt;
        let path = std::env::temp_dir().join(format!("pigtail-test-{}", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        let path_str = path.to_str().unwrap();
        let gid = std::fs::metadata(&path).unwrap().gid();

        // `getent` may simply be absent (minimal containers, some CI images);
        // that is an environment gap, not a bug in `device_group_name`, so
        // skip rather than fail the suite.
        let Some(name) = device_group_name(path_str) else {
            std::fs::remove_file(&path).unwrap();
            eprintln!("skipping: `getent` unavailable in this environment");
            return;
        };

        // Round-trip through getent by name, rather than hardcoding an
        // expected group: CI and dev machines can have different primary
        // groups, so the only thing worth asserting is that the name we
        // returned actually maps back to the file's real gid.
        let out = std::process::Command::new("getent")
            .arg("group")
            .arg(&name)
            .output()
            .unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        let resolved_gid: u32 = stdout.split(':').nth(2).unwrap().trim().parse().unwrap();
        assert_eq!(resolved_gid, gid);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn map_open_error_recognizes_permission_denied_by_kind_not_message_text() {
        // Regression guard: this must key off `ErrorKind`, not a
        // language-specific substring of the description, since the OS
        // message can be localized (see `windows_port_still_present`).
        let e = serialport::Error::new(
            serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied),
            "Permission non accordée",
        );
        let err = map_open_error("/dev/ttyUSB0", e);
        assert!(matches!(err, SourceError::Open(msg) if msg.contains("permission denied")));
    }

    #[test]
    fn map_open_error_reports_device_not_present_for_no_device() {
        let e = serialport::Error::new(serialport::ErrorKind::NoDevice, "Le fichier n'existe pas");
        let err = map_open_error("/dev/ttyUSB0", e);
        assert!(matches!(err, SourceError::Open(msg) if msg.contains("device not present")));
    }
}
