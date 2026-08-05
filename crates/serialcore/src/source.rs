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
    /// hint about the `dialout` group.
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
    let msg = e.to_string();
    if e.kind() == serialport::ErrorKind::NoDevice || msg.to_lowercase().contains("no such") {
        return SourceError::Open(format!("{path}: device not present"));
    }
    // Permission denied is the first thing that bites a new Linux user.
    if msg.to_lowercase().contains("permission denied") {
        return SourceError::Open(format!(
            "{path}: permission denied. On Linux, add yourself to the 'dialout' \
             group: `sudo usermod -aG dialout $USER`, then log out and back in."
        ));
    }
    SourceError::Open(format!("{path}: {msg}"))
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
}
