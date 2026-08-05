//! Raw session log: the source of truth (spec §5, rule 3; §7.5).
//!
//! Two files per session, named by ISO timestamp and port label:
//! - `<name>.session.meta.json` — identity, config, start time, app version.
//! - `<name>.session.bin` — magic `SMON`, u16 version, then records of
//!   `{ u64 micros_since_session_start, u32 byte_count, [u8] bytes }`.
//!
//! Bytes are written before any parsing, so a capture survives a panic. On
//! reconnect/startup, the trailing bytes of a port's previous capture are
//! read back in (`read_tail_records`) to re-display recent history.

use crate::config::{PortConfig, PortIdentity};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAGIC: &[u8; 4] = b"SMON";
const VERSION: u16 = 1;
const HEADER_LEN: u64 = 6;

/// Sidecar metadata written as JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub identity: PortIdentity,
    pub config: PortConfig,
    pub start_wall: DateTime<Utc>,
    pub app_version: String,
    pub port_label: String,
}

/// Writes the raw session log. Flushes at least once per second; never fsyncs
/// per write.
pub struct SessionWriter {
    file: BufWriter<File>,
    bin_path: PathBuf,
    meta_path: PathBuf,
    last_flush: Instant,
}

impl SessionWriter {
    /// Create both files in `dir`. Returns a writer with the header already
    /// written and the meta sidecar flushed.
    pub fn create(dir: &Path, meta: &SessionMeta) -> std::io::Result<SessionWriter> {
        std::fs::create_dir_all(dir)?;
        let stamp = meta.start_wall.format("%Y%m%dT%H%M%S");
        let stem = format!("{}_{}", stamp, sanitize_label(&meta.port_label));
        let bin_path = dir.join(format!("{stem}.session.bin"));
        let meta_path = dir.join(format!("{stem}.session.meta.json"));

        let json = serde_json::to_string_pretty(meta)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&meta_path, json)?;

        let mut file = BufWriter::new(File::create(&bin_path)?);
        file.write_all(MAGIC)?;
        file.write_all(&VERSION.to_le_bytes())?;

        Ok(SessionWriter {
            file,
            bin_path,
            meta_path,
            last_flush: Instant::now(),
        })
    }

    /// Append one record. Auto-flushes if more than a second has passed.
    pub fn write_record(&mut self, micros: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write_all(&micros.to_le_bytes())?;
        self.file.write_all(&(bytes.len() as u32).to_le_bytes())?;
        self.file.write_all(bytes)?;
        if self.last_flush.elapsed() >= Duration::from_secs(1) {
            self.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        self.last_flush = Instant::now();
        Ok(())
    }

    pub fn bin_path(&self) -> &Path {
        &self.bin_path
    }
    pub fn meta_path(&self) -> &Path {
        &self.meta_path
    }
}

impl Drop for SessionWriter {
    fn drop(&mut self) {
        let _ = self.file.flush();
    }
}

/// Read the meta sidecar for a session bin file, if present.
pub fn read_meta(bin_path: &Path) -> std::io::Result<SessionMeta> {
    let meta_path = meta_path_for(bin_path);
    let s = std::fs::read_to_string(meta_path)?;
    serde_json::from_str(&s).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Read raw records from a capture, keeping only the trailing `max_bytes` worth
/// (older records are dropped as newer ones arrive) so preloading a large
/// capture into memory stays bounded. Returns `(micros, bytes)` records in file
/// order. Used to re-display a port's previous output on startup.
pub fn read_tail_records(
    bin_path: &Path,
    max_bytes: usize,
) -> std::io::Result<Vec<(u64, Vec<u8>)>> {
    use std::collections::VecDeque;

    let mut reader = BufReader::new(File::open(bin_path)?);
    let mut hdr = [0u8; HEADER_LEN as usize];
    reader.read_exact(&mut hdr)?;
    if &hdr[..4] != MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad magic (not an SMON capture)",
        ));
    }
    // Unknown future versions: refuse rather than misparse.
    let version = u16::from_le_bytes([hdr[4], hdr[5]]);
    if version != VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported capture version {version}"),
        ));
    }

    let mut out: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
    let mut total = 0usize;
    loop {
        let mut rh = [0u8; 12];
        match reader.read_exact(&mut rh) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        let micros = u64::from_le_bytes(rh[..8].try_into().unwrap());
        let count = u32::from_le_bytes(rh[8..12].try_into().unwrap()) as usize;
        let mut bytes = vec![0u8; count];
        reader.read_exact(&mut bytes)?;
        total += count;
        out.push_back((micros, bytes));
        // Drop from the front once over budget, but always keep at least one.
        while total > max_bytes && out.len() > 1 {
            if let Some((_, dropped)) = out.pop_front() {
                total -= dropped.len();
            }
        }
    }
    Ok(out.into())
}

fn meta_path_for(bin_path: &Path) -> PathBuf {
    // `<stem>.session.bin` -> `<stem>.session.meta.json`
    let s = bin_path.to_string_lossy();
    if let Some(prefix) = s.strip_suffix(".session.bin") {
        PathBuf::from(format!("{prefix}.session.meta.json"))
    } else {
        bin_path.with_extension("meta.json")
    }
}

/// Delete session files (`.session.bin` and their `.meta.json`) whose modified
/// time is older than `days` (spec §7.5). Returns the number of bin files
/// removed.
pub fn cleanup_old_sessions(dir: &Path, days: u32) -> std::io::Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(Duration::from_secs(days as u64 * 86_400))
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut removed = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(".session.bin") {
            continue;
        }
        let modified = entry.metadata().and_then(|m| m.modified());
        if let Ok(modified) = modified {
            if modified < cutoff {
                std::fs::remove_file(&path)?;
                let _ = std::fs::remove_file(meta_path_for(&path));
                removed += 1;
            }
        }
    }
    Ok(removed)
}

fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "port".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PortConfig;

    fn sample_meta() -> SessionMeta {
        SessionMeta {
            identity: PortIdentity {
                vid: Some(0x0483),
                pid: Some(0x374B),
                ..Default::default()
            },
            config: PortConfig::default(),
            start_wall: Utc::now(),
            app_version: "test".into(),
            port_label: "Nucleo/1".into(),
        }
    }

    #[test]
    fn write_then_read_meta_and_tail_records_roundtrip() {
        let dir = std::env::temp_dir().join(format!("smon-test-{}", std::process::id()));
        let mut w = SessionWriter::create(&dir, &sample_meta()).unwrap();
        w.write_record(0, b"hello ").unwrap();
        w.write_record(1000, b"world\n").unwrap();
        let bin = w.bin_path().to_path_buf();
        w.flush().unwrap();
        drop(w);

        // Meta reads back.
        let meta = read_meta(&bin).unwrap();
        assert_eq!(meta.identity.vid, Some(0x0483));

        // Records read back in order, used to re-display history on reconnect.
        let records = read_tail_records(&bin, 1024).unwrap();
        assert_eq!(
            records,
            vec![(0, b"hello ".to_vec()), (1000, b"world\n".to_vec())]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_tail_records_rejects_bad_magic() {
        let dir = std::env::temp_dir().join(format!("smon-badmagic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.session.bin");
        std::fs::write(&path, b"NOPEnotacapture").unwrap();
        assert!(read_tail_records(&path, 1024).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
