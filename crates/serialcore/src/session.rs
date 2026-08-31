//! Raw session log: the source of truth (spec §5, rule 3; §7.5).
//!
//! Two files per session, named by ISO timestamp and port label (plus a `-1`,
//! `-2`, … suffix where that would collide — the timestamp is the app's clock
//! anchor, shared by every capture taken in one run):
//! - `<name>.session.meta.json` — identity, config, start time, app version.
//! - `<name>.session.bin` — magic `SMON`, u16 version, then records of
//!   `{ u64 micros_since_session_start, u32 byte_count, [u8] bytes }`.
//!
//! Bytes are written before any parsing, so a capture survives a panic. On
//! reconnect/startup, the trailing bytes of a port's previous captures are
//! read back in (`read_tail_records`) to re-display recent history.

use crate::config::{PortConfig, PortIdentity};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, ErrorKind, Read, Seek, SeekFrom, Write};
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
    /// Set once the user has cleared the console during this session: the
    /// records before that point were discarded on purpose. Restoring history
    /// stops at such a capture rather than reaching into older ones, which
    /// would put back exactly what was cleared. Captures written before the
    /// flag existed have none, hence the default.
    #[serde(default)]
    pub cleared: bool,
}

/// Writes the raw session log. Flushes at least once per second; never fsyncs
/// per write.
pub struct SessionWriter {
    file: BufWriter<File>,
    bin_path: PathBuf,
    meta_path: PathBuf,
    /// Kept so the sidecar can be rewritten when the log is cleared.
    meta: SessionMeta,
    last_flush: Instant,
}

impl SessionWriter {
    /// Create both files in `dir`. Returns a writer with the header already
    /// written and the meta sidecar flushed.
    pub fn create(dir: &Path, meta: &SessionMeta) -> std::io::Result<SessionWriter> {
        std::fs::create_dir_all(dir)?;
        let stamp = meta.start_wall.format("%Y%m%dT%H%M%S");
        let base = format!("{}_{}", stamp, sanitize_label(&meta.port_label));

        // `start_wall` is the app-global clock anchor, so every capture taken in
        // one run of the app carries the same one, and the stem collides
        // whenever a device is captured twice in a run — reopening its tab, or
        // applying new port options, which respawns the reader. Claiming the
        // name with `create_new` gives each capture a file of its own; sharing
        // one truncates the earlier capture and then interleaves the two
        // writers, each at its own file offset.
        let (file, bin_path, meta_path) = create_unique(dir, &base)?;

        let json = serde_json::to_string_pretty(meta)
            .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
        crate::fs::atomic_write(&meta_path, json)?;

        let mut file = BufWriter::new(file);
        file.write_all(MAGIC)?;
        file.write_all(&VERSION.to_le_bytes())?;

        Ok(SessionWriter {
            file,
            bin_path,
            meta_path,
            meta: meta.clone(),
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

    /// Discard every record written so far, leaving the header — the on-disk
    /// half of "clear console". The session keeps writing to the same file, so
    /// subsequent records carry their original micros-since-session-start and
    /// still line up with the start time in the meta sidecar.
    ///
    /// The buffered writes are flushed first and the file position moved back
    /// with the truncation: a `BufWriter` position left past the new end would
    /// otherwise reopen the hole as a run of zero bytes.
    pub fn truncate(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        let file = self.file.get_mut();
        file.set_len(HEADER_LEN)?;
        file.seek(SeekFrom::Start(HEADER_LEN))?;
        self.last_flush = Instant::now();
        // Note the clear in the sidecar, so restoring history on a later launch
        // stops here instead of reaching past it into older captures.
        if !self.meta.cleared {
            let mut meta = self.meta.clone();
            meta.cleared = true;
            if let Ok(json) = serde_json::to_string_pretty(&meta) {
                crate::fs::atomic_write(&self.meta_path, json)?;
                // Marked as done only once it is actually on disk: setting the
                // flag first would make every later clear skip this block, and a
                // single failed write would leave `cleared: false` on disk for
                // good — which is the one thing this guards against.
                self.meta = meta;
            }
        }
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

/// Claim an unused `<stem>.session.bin` in `dir`, returning the created file
/// and the pair of paths. `-1`, `-2`, … disambiguate captures that would
/// otherwise share a stem.
fn create_unique(dir: &Path, base: &str) -> std::io::Result<(File, PathBuf, PathBuf)> {
    for n in 0..1000u32 {
        let stem = if n == 0 {
            base.to_string()
        } else {
            format!("{base}-{n}")
        };
        let bin_path = dir.join(format!("{stem}.session.bin"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&bin_path)
        {
            Ok(file) => {
                let meta_path = dir.join(format!("{stem}.session.meta.json"));
                return Ok((file, bin_path, meta_path));
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        ErrorKind::AlreadyExists,
        format!("no free capture file name for {base}"),
    ))
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

    let file = File::open(bin_path)?;
    let file_len = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let mut hdr = [0u8; HEADER_LEN as usize];
    reader.read_exact(&mut hdr)?;
    check_header(&hdr)?;

    let mut out: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
    let mut total = 0usize;
    let mut pos = HEADER_LEN;
    loop {
        let mut rh = [0u8; 12];
        match reader.read_exact(&mut rh) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        pos += 12;
        let micros = u64::from_le_bytes(rh[..8].try_into().unwrap());
        let count = u64::from(u32::from_le_bytes(rh[8..12].try_into().unwrap()));
        // A capture the app died partway through ends in a torn record, and a
        // damaged one can name a length that runs off the end of the file.
        // Everything read before it is still good history, so stop there and
        // return it rather than failing the read and dropping all of it.
        if count > file_len.saturating_sub(pos) {
            break;
        }
        let mut bytes = vec![0u8; count as usize];
        match reader.read_exact(&mut bytes) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        pos += count;
        total += bytes.len();
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

/// The micros stamp on a capture's first record, or `None` when it holds none.
/// Captures written in the same run of the app share a `start_wall` — the clock
/// is app-global — so this is what orders them against one another.
pub fn read_first_micros(bin_path: &Path) -> std::io::Result<Option<u64>> {
    let mut reader = BufReader::new(File::open(bin_path)?);
    let mut hdr = [0u8; HEADER_LEN as usize];
    reader.read_exact(&mut hdr)?;
    check_header(&hdr)?;
    let mut rh = [0u8; 8];
    match reader.read_exact(&mut rh) {
        Ok(()) => Ok(Some(u64::from_le_bytes(rh))),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

fn check_header(hdr: &[u8; HEADER_LEN as usize]) -> std::io::Result<()> {
    if &hdr[..4] != MAGIC {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "bad magic (not an SMON capture)",
        ));
    }
    // Unknown future versions: refuse rather than misparse.
    let version = u16::from_le_bytes([hdr[4], hdr[5]]);
    if version != VERSION {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("unsupported capture version {version}"),
        ));
    }
    Ok(())
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
            cleared: false,
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
    fn truncate_drops_records_and_keeps_writing() {
        let dir = std::env::temp_dir().join(format!("smon-truncate-{}", std::process::id()));
        let mut w = SessionWriter::create(&dir, &sample_meta()).unwrap();
        w.write_record(0, b"cleared away\n").unwrap();
        w.truncate().unwrap();
        // The file is a valid, empty capture...
        let bin = w.bin_path().to_path_buf();
        assert!(read_tail_records(&bin, 1024).unwrap().is_empty());
        // ...and the same session keeps appending to it, with no zero-filled
        // hole left where the discarded record used to be.
        w.write_record(2000, b"after\n").unwrap();
        w.flush().unwrap();
        assert_eq!(
            read_tail_records(&bin, 1024).unwrap(),
            vec![(2000, b"after\n".to_vec())]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_sidecar_write_leaves_the_clear_to_be_recorded_again() {
        // The flag is the whole clear-safety guarantee: if it never reaches the
        // sidecar, the next launch restores what the clear discarded. So a
        // failed write has to leave the writer ready to try again, not
        // remembering a clear that was never recorded.
        let dir = std::env::temp_dir().join(format!("smon-clearfail-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let mut w = SessionWriter::create(&dir, &sample_meta()).unwrap();
        w.write_record(0, b"cleared away\n").unwrap();

        // Make the sidecar unwritable by standing a directory in its place, so
        // noting the clear in it fails.
        let meta_path = w.meta_path().to_path_buf();
        let sidecar = std::fs::read_to_string(&meta_path).unwrap();
        std::fs::remove_file(&meta_path).unwrap();
        std::fs::create_dir(&meta_path).unwrap();
        assert!(w.truncate().is_err(), "the sidecar write failed");

        // Put the original sidecar back, untouched: the clear got nowhere.
        std::fs::remove_dir(&meta_path).unwrap();
        std::fs::write(&meta_path, &sidecar).unwrap();
        assert!(
            !read_meta(w.bin_path()).unwrap().cleared,
            "nothing was recorded, which is what the retry has to make up for"
        );

        // Clearing again — or any later clear in this session — records it.
        w.truncate().unwrap();
        assert!(
            read_meta(w.bin_path()).unwrap().cleared,
            "the clear reached the sidecar on the retry"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn captures_from_one_run_get_a_file_each() {
        // Every capture in a run of the app shares `start_wall` (the clock is
        // app-global), so the stem alone collides whenever a device is captured
        // twice — reopening its tab, or applying new port options. Sharing the
        // file truncated the first capture and lost everything in it.
        let dir = std::env::temp_dir().join(format!("smon-unique-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let meta = sample_meta();

        let mut first = SessionWriter::create(&dir, &meta).unwrap();
        first.write_record(0, b"first capture\n").unwrap();
        first.flush().unwrap();

        let mut second = SessionWriter::create(&dir, &meta).unwrap();
        second.write_record(1000, b"second capture\n").unwrap();
        second.flush().unwrap();

        assert_ne!(first.bin_path(), second.bin_path());
        assert_ne!(first.meta_path(), second.meta_path());
        assert_eq!(
            read_tail_records(first.bin_path(), 1024).unwrap(),
            vec![(0, b"first capture\n".to_vec())],
            "the earlier capture is intact"
        );
        assert_eq!(
            read_tail_records(second.bin_path(), 1024).unwrap(),
            vec![(1000, b"second capture\n".to_vec())]
        );
        // Each keeps its own readable sidecar.
        assert!(read_meta(first.bin_path()).is_ok());
        assert!(read_meta(second.bin_path()).is_ok());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_torn_trailing_record_keeps_what_came_before_it() {
        let dir = std::env::temp_dir().join(format!("smon-torn-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let mut w = SessionWriter::create(&dir, &sample_meta()).unwrap();
        w.write_record(0, b"good\n").unwrap();
        w.write_record(1000, b"also good\n").unwrap();
        let bin = w.bin_path().to_path_buf();
        w.flush().unwrap();
        drop(w);

        // Killed between a record's length and its bytes, as a crash or a power
        // cut leaves it: a header claiming 32 bytes with 4 on disk.
        let mut f = OpenOptions::new().append(true).open(&bin).unwrap();
        f.write_all(&2000u64.to_le_bytes()).unwrap();
        f.write_all(&32u32.to_le_bytes()).unwrap();
        f.write_all(b"cut!").unwrap();
        drop(f);

        assert_eq!(
            read_tail_records(&bin, 1024).unwrap(),
            vec![(0, b"good\n".to_vec()), (1000, b"also good\n".to_vec())],
            "the whole history is kept, not discarded with the torn record"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncate_marks_the_capture_cleared() {
        let dir = std::env::temp_dir().join(format!("smon-cleared-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let mut w = SessionWriter::create(&dir, &sample_meta()).unwrap();
        w.write_record(0, b"discarded\n").unwrap();
        let bin = w.bin_path().to_path_buf();
        assert!(!read_meta(&bin).unwrap().cleared);

        w.truncate().unwrap();
        assert!(
            read_meta(&bin).unwrap().cleared,
            "restoring history must stop at a capture the user cleared"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn first_micros_orders_captures_made_in_one_run() {
        let dir = std::env::temp_dir().join(format!("smon-first-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let meta = sample_meta();

        let mut a = SessionWriter::create(&dir, &meta).unwrap();
        a.write_record(5_000, b"earlier\n").unwrap();
        a.flush().unwrap();
        let mut b = SessionWriter::create(&dir, &meta).unwrap();
        b.write_record(9_000, b"later\n").unwrap();
        b.flush().unwrap();
        let mut empty = SessionWriter::create(&dir, &meta).unwrap();
        empty.flush().unwrap();

        assert_eq!(read_first_micros(a.bin_path()).unwrap(), Some(5_000));
        assert_eq!(read_first_micros(b.bin_path()).unwrap(), Some(9_000));
        assert_eq!(read_first_micros(empty.bin_path()).unwrap(), None);

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
