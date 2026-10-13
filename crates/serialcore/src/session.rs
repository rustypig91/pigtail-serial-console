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

// Cleanup must never unlink a capture owned by a live reader, including when
// multiple readers happen to share the same device identity.
fn active_captures() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, u64>> {
    static ACTIVE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, u64>>> =
        std::sync::OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}

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
        // Publish the sidecar and register its writer under the same lock so
        // cleanup cannot mistake a newly created capture for a closed one.
        let mut active = active_captures().lock().expect("active captures lock");
        let (file, bin_path, meta_path) = create_unique(dir, &base)?;

        let json = serde_json::to_string_pretty(meta)
            .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
        crate::fs::atomic_write(&meta_path, json)?;

        let mut file = BufWriter::new(file);
        file.write_all(MAGIC)?;
        file.write_all(&VERSION.to_le_bytes())?;

        active.insert(bin_path.clone(), 0);
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
        let mut active = active_captures().lock().expect("active captures lock");
        // Invalidate incremental counts on every clear, even if new output
        // grows past the previous file size before the next cleanup pass.
        if let Some(generation) = active.get_mut(&self.bin_path) {
            *generation = generation.wrapping_add(1);
        }
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
        active_captures()
            .lock()
            .expect("active captures lock")
            .remove(&self.bin_path);
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

/// Visit complete records in bounded chunks. A torn final record is ignored.
fn scan_records(
    path: &Path,
    start: u64,
    end: Option<u64>,
    mut visit: impl FnMut(u64, u64, &[u8]),
) -> std::io::Result<u64> {
    let file = File::open(path)?;
    let len = file.metadata()?.len().min(end.unwrap_or(u64::MAX));
    let mut reader = BufReader::new(file);
    let mut header = [0; HEADER_LEN as usize];
    reader.read_exact(&mut header)?;
    check_header(&header)?;
    reader.seek(SeekFrom::Start(start))?;
    let mut pos = start;
    let mut buffer = [0; 64 * 1024];
    while len.saturating_sub(pos) >= 12 {
        let mut rh = [0; 12];
        reader.read_exact(&mut rh)?;
        let count = u64::from(u32::from_le_bytes(rh[8..].try_into().unwrap()));
        if count > len.saturating_sub(pos + 12) {
            break;
        }
        let mut remaining = count;
        while remaining > 0 {
            let n = remaining.min(buffer.len() as u64) as usize;
            reader.read_exact(&mut buffer[..n])?;
            visit(
                pos,
                u64::from_le_bytes(rh[..8].try_into().unwrap()),
                &buffer[..n],
            );
            remaining -= n as u64;
        }
        pos += 12 + count;
    }
    Ok(pos)
}

/// Raw records in capture order, with their original microsecond timestamps.
pub type SessionRecords = Vec<(u64, Vec<u8>)>;

/// Restore by log lines, preserving complete records and the first byte of a
/// line spanning records. Record boundaries can include extra leading lines;
/// the caller's line store evicts those. No age or byte budget gates restore.
#[derive(Clone, Copy)]
pub struct LineTail {
    start: u64,
    end: u64,
    pub lines: usize,
}

impl LineTail {
    pub fn has_records(&self) -> bool {
        self.end > self.start
    }
}

/// Locate the raw records covering the requested log tail without loading them.
pub fn line_tail(
    path: &Path,
    max_lines: usize,
    mode: crate::config::TerminalMode,
) -> std::io::Result<LineTail> {
    use std::collections::VecDeque;
    let mut framer = crate::framer::Framer::with_mode(mode);
    let mut lines = Vec::new();
    let mut starts = VecDeque::new();
    let mut total = 0usize;
    let mut origin = HEADER_LEN;
    let mut collect = |lines: &mut Vec<crate::framer::FramedLine>, origin: &mut u64, pos: u64| {
        for line in lines.drain(..) {
            total = total.saturating_add(1);
            starts.push_back((*origin).min(line.ts.micros as u64));
            *origin = pos;
            if starts.len() > max_lines.max(1) {
                starts.pop_front();
            }
        }
    };
    let end = scan_records(path, HEADER_LEN, None, |pos, _, bytes| {
        if !framer.has_pending_input() {
            origin = pos;
        }
        framer.push(
            bytes,
            crate::clock::Timestamp {
                wall: DateTime::UNIX_EPOCH,
                micros: pos as i64,
            },
            &mut lines,
        );
        collect(&mut lines, &mut origin, pos);
    })?;
    framer.flush_final(&mut lines);
    collect(&mut lines, &mut origin, end);
    // Control-only captures remain available to Hex and ANSI/VT.
    let start = starts.front().copied().unwrap_or(HEADER_LEN);
    Ok(LineTail {
        start,
        end,
        lines: total,
    })
}

/// Replay selected records in bounded chunks, preserving their timestamps.
pub fn visit_line_tail(
    path: &Path,
    tail: LineTail,
    mut visit: impl FnMut(u64, &[u8]),
) -> std::io::Result<()> {
    scan_records(path, tail.start, Some(tail.end), |_, micros, bytes| {
        visit(micros, bytes)
    })?;
    Ok(())
}

/// Read the selected tail, for callers that need owned raw records.
pub fn read_tail_records_by_lines(
    path: &Path,
    max_lines: usize,
    mode: crate::config::TerminalMode,
) -> std::io::Result<(SessionRecords, usize)> {
    let tail = line_tail(path, max_lines, mode)?;
    let start = tail.start;
    let end = tail.end;
    let mut reader = BufReader::new(File::open(path)?);
    reader.seek(SeekFrom::Start(start))?;
    let mut pos = start;
    let mut records = Vec::new();
    while pos < end {
        let mut rh = [0; 12];
        reader.read_exact(&mut rh)?;
        let count = u32::from_le_bytes(rh[8..].try_into().unwrap()) as usize;
        let mut bytes = vec![0; count];
        reader.read_exact(&mut bytes)?;
        records.push((u64::from_le_bytes(rh[..8].try_into().unwrap()), bytes));
        pos += 12 + count as u64;
    }
    Ok((records, tail.lines))
}

struct CaptureLines {
    pos: u64,
    framer: crate::framer::Framer,
    completed: usize,
    mode: crate::config::TerminalMode,
    cleared: bool,
    generation: Option<u64>,
}

/// Incremental line-based cleanup. Files still contributing to a device's log
/// are kept whole, including a partially visible oldest capture. The newest
/// active captures are protected while their reader is writing them.
#[derive(Default)]
pub struct SessionCleanup {
    counts: std::collections::HashMap<PathBuf, CaptureLines>,
}

impl SessionCleanup {
    fn lines(&mut self, path: &Path, meta: &SessionMeta) -> std::io::Result<usize> {
        let generation = active_captures()
            .lock()
            .expect("active captures lock")
            .get(path)
            .copied();
        let len = std::fs::metadata(path)?.len();
        let state = self
            .counts
            .entry(path.to_path_buf())
            .or_insert_with(|| CaptureLines {
                pos: HEADER_LEN,
                framer: crate::framer::Framer::with_mode(meta.config.terminal),
                completed: 0,
                mode: meta.config.terminal,
                cleared: meta.cleared,
                generation,
            });
        if len < state.pos
            || state.mode != meta.config.terminal
            || state.cleared != meta.cleared
            || state.generation != generation
        {
            *state = CaptureLines {
                pos: HEADER_LEN,
                framer: crate::framer::Framer::with_mode(meta.config.terminal),
                completed: 0,
                mode: meta.config.terminal,
                cleared: meta.cleared,
                generation,
            };
        }
        let mut framed = Vec::new();
        let mut framer = state.framer.clone();
        let mut completed = state.completed;
        let pos = scan_records(path, state.pos, None, |_, _, bytes| {
            framer.push(
                bytes,
                crate::clock::Timestamp {
                    wall: meta.start_wall,
                    micros: 0,
                },
                &mut framed,
            );
            completed = completed.saturating_add(framed.len());
            framed.clear();
        })?;
        if active_captures()
            .lock()
            .expect("active captures lock")
            .get(path)
            .copied()
            != generation
        {
            return Err(std::io::Error::new(
                ErrorKind::Interrupted,
                "capture changed during cleanup scan",
            ));
        }
        state.pos = pos;
        state.framer = framer;
        state.completed = completed;
        let mut tail = state.framer.clone();
        tail.flush_final(&mut framed);
        Ok(state.completed.saturating_add(framed.len()))
    }

    /// Apply the same line limit independently to each saved device identity.
    /// Invalid/unreadable captures are retained, never grounds for deleting
    /// older history. Sidecars are removed together with their raw captures.
    pub fn cleanup(&mut self, dir: &Path, max_lines: usize) -> std::io::Result<usize> {
        if !dir.exists() {
            return Ok(0);
        }
        let mut captures = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if !path.to_string_lossy().ends_with(".session.bin") {
                continue;
            }
            if let Ok(meta) = read_meta(&path) {
                let first = read_first_micros(&path).ok().flatten().unwrap_or(u64::MAX);
                captures.push((path, meta, first));
            }
        }
        captures.sort_by_key(|(path, meta, first)| (meta.start_wall, *first, path.clone()));
        let mut budgets: Vec<(PortIdentity, usize, bool)> = Vec::new();
        let mut remove = Vec::new();
        for (path, meta, _) in captures.iter().rev() {
            let newest = !budgets.iter().any(|(id, _, _)| id == &meta.identity);
            if newest {
                budgets.push((meta.identity.clone(), max_lines.max(1), false));
            }
            let (_, remaining, cleared) = budgets
                .iter_mut()
                .find(|(id, _, _)| id == &meta.identity)
                .unwrap();
            let active = active_captures()
                .lock()
                .expect("active captures lock")
                .contains_key(path);
            // Validate even captures outside the window before deleting them.
            let Ok(lines) = self.lines(path, meta) else {
                continue;
            };
            if !active && (*remaining == 0 || *cleared) {
                remove.push(path.clone());
                continue;
            }
            // Restored nonempty logs have one closing session marker;
            // live output does not have that marker yet.
            *remaining =
                remaining.saturating_sub(lines.saturating_add(usize::from(!active && lines > 0)));
            *cleared |= meta.cleared;
        }
        let removed = remove_session_paths(&remove)?;
        self.counts.retain(|path, _| path.exists());
        Ok(removed)
    }
}

/// Delete exactly the session captures in `paths`, along with their metadata.
///
/// Only captures outside the retained history should be passed here.
pub fn remove_session_paths(paths: &[PathBuf]) -> std::io::Result<usize> {
    let active = active_captures().lock().expect("active captures lock");
    let mut removed = 0;
    for path in paths {
        if active.contains_key(path) {
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        let _ = std::fs::remove_file(meta_path_for(path));
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

    fn saved_capture(dir: &Path, sequence: i64, device: &str, records: &[&[u8]]) -> PathBuf {
        let mut meta = sample_meta();
        meta.start_wall = DateTime::UNIX_EPOCH + chrono::Duration::seconds(sequence);
        meta.identity.serial_number = Some(device.into());
        let mut writer = SessionWriter::create(dir, &meta).unwrap();
        for (n, bytes) in records.iter().enumerate() {
            writer.write_record(n as u64, bytes).unwrap();
        }
        writer.bin_path().to_path_buf()
    }

    #[test]
    fn cleanup_keeps_partly_visible_captures_regardless_of_age() {
        let dir = tempfile::tempdir().unwrap();
        let oldest = saved_capture(dir.path(), 1, "A", &[b"old\n"]);
        let partial = saved_capture(dir.path(), 2, "A", &[b"one\ntwo\nthree\n"]);
        let newest = saved_capture(dir.path(), 3, "A", &[b"new\n"]);
        let other = saved_capture(dir.path(), 4, "B", &[b"other\n"]);
        let ancient = std::time::UNIX_EPOCH + Duration::from_secs(1);
        File::options()
            .write(true)
            .open(&partial)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(ancient))
            .unwrap();
        let mut cleanup = SessionCleanup::default();
        // Two lines from newest (text + marker), three remaining: partial
        // still contributes and is retained in full, oldest contributes none.
        assert_eq!(cleanup.cleanup(dir.path(), 5).unwrap(), 1);
        assert!(!oldest.exists());
        assert!(!meta_path_for(&oldest).exists());
        assert!(partial.exists());
        assert!(newest.exists());
        assert!(other.exists());
        assert_eq!(
            read_tail_records(&partial, usize::MAX).unwrap()[0].1,
            b"one\ntwo\nthree\n"
        );
        assert_eq!(cleanup.cleanup(dir.path(), 2).unwrap(), 1);
        assert!(!partial.exists());
        assert!(newest.exists());
        assert!(other.exists());
    }

    #[test]
    fn growing_active_capture_evicts_old_files_without_deleting_live_writers() {
        let dir = tempfile::tempdir().unwrap();
        let old = saved_capture(dir.path(), 1, "A", &[b"old\n"]);
        let mut meta = sample_meta();
        meta.identity.serial_number = Some("A".into());
        let mut writer = SessionWriter::create(dir.path(), &meta).unwrap();
        let live = writer.bin_path().to_path_buf();
        let mut cleanup = SessionCleanup::default();
        writer.write_record(0, b"par").unwrap();
        writer.flush().unwrap();
        assert_eq!(cleanup.cleanup(dir.path(), 3).unwrap(), 0);
        writer.write_record(1, b"tial\nsecond\nthird\n").unwrap();
        writer.flush().unwrap();
        assert_eq!(cleanup.cleanup(dir.path(), 3).unwrap(), 1);
        assert!(!old.exists());
        assert!(live.exists());
        assert_eq!(cleanup.lines(&live, &meta).unwrap(), 3);
        assert_eq!(
            remove_session_paths(std::slice::from_ref(&live)).unwrap(),
            0
        );
        writer.truncate().unwrap();
        writer.write_record(2, b"after clear\n").unwrap();
        writer.flush().unwrap();
        assert_eq!(cleanup.lines(&live, &read_meta(&live).unwrap()).unwrap(), 1);
    }

    #[test]
    fn cleanup_respects_clear_boundaries_and_preserves_invalid_captures() {
        let dir = tempfile::tempdir().unwrap();
        let old = saved_capture(dir.path(), 1, "A", &[b"discarded\n"]);
        let mut meta = sample_meta();
        meta.identity.serial_number = Some("A".into());
        let mut writer = SessionWriter::create(dir.path(), &meta).unwrap();
        writer.truncate().unwrap();
        writer.flush().unwrap();
        let live = writer.bin_path().to_path_buf();
        let invalid = dir.path().join("invalid.session.bin");
        std::fs::write(&invalid, b"invalid").unwrap();
        assert_eq!(
            SessionCleanup::default().cleanup(dir.path(), 100).unwrap(),
            1
        );
        assert!(!old.exists());
        assert!(live.exists());
        assert!(invalid.exists());
    }

    #[test]
    fn cleanup_preserves_invalid_captures_outside_the_line_window() {
        let dir = tempfile::tempdir().unwrap();
        let invalid = saved_capture(dir.path(), 1, "A", &[b"old\n"]);
        std::fs::write(&invalid, b"invalid header").unwrap();
        let newest = saved_capture(dir.path(), 2, "A", &[b"new\n"]);
        assert_eq!(SessionCleanup::default().cleanup(dir.path(), 2).unwrap(), 0);
        assert!(invalid.exists());
        assert!(meta_path_for(&invalid).exists());
        assert!(newest.exists());
    }

    #[test]
    fn cleanup_recounts_after_repeated_clear_and_regrowth() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = SessionWriter::create(dir.path(), &sample_meta()).unwrap();
        let path = writer.bin_path().to_path_buf();
        let mut cleanup = SessionCleanup::default();
        writer.truncate().unwrap();
        writer.write_record(0, b"a\nb\nc\n").unwrap();
        writer.flush().unwrap();
        assert_eq!(cleanup.lines(&path, &read_meta(&path).unwrap()).unwrap(), 3);
        writer.truncate().unwrap();
        // Regrow beyond the cached offset without crossing another record
        // boundary there; size and the already-set clear flag cannot detect it.
        writer
            .write_record(1, b"one much longer replacement line\n")
            .unwrap();
        writer.flush().unwrap();
        assert_eq!(cleanup.lines(&path, &read_meta(&path).unwrap()).unwrap(), 1);
    }

    #[test]
    fn line_tail_keeps_a_line_spanning_records_and_ignores_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = saved_capture(
            dir.path(),
            1,
            "A",
            &[b"old\n", b"start ", b"middle ", b"end\n", b"last"],
        );
        let (records, total) =
            read_tail_records_by_lines(&path, 2, crate::config::TerminalMode::Classic).unwrap();
        assert_eq!(total, 3);
        assert_eq!(
            records
                .into_iter()
                .flat_map(|(_, bytes)| bytes)
                .collect::<Vec<_>>(),
            b"start middle end\nlast"
        );
        let mut file = File::options().append(true).open(&path).unwrap();
        file.write_all(&0u64.to_le_bytes()).unwrap();
        file.write_all(&100u32.to_le_bytes()).unwrap();
        file.write_all(b"torn").unwrap();
        assert_eq!(
            read_tail_records_by_lines(&path, 1, crate::config::TerminalMode::Classic).unwrap(),
            (vec![(4, b"last".to_vec())], 3)
        );
    }

    #[test]
    fn line_tail_preserves_escape_sequences_split_before_visible_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = saved_capture(dir.path(), 1, "A", &[b"old\n", b"\x1b[", b"31", b"mred\n"]);
        let (records, lines) =
            read_tail_records_by_lines(&path, 1, crate::config::TerminalMode::Vt100).unwrap();
        assert_eq!(lines, 2);
        assert_eq!(
            records
                .into_iter()
                .flat_map(|(_, bytes)| bytes)
                .collect::<Vec<_>>(),
            b"\x1b[31mred\n"
        );
    }

    #[test]
    fn streaming_restore_bounds_chunks_and_preserves_record_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = vec![b'x'; crate::framer::MAX_LINE_LEN * 3 + 1];
        let path = saved_capture(dir.path(), 1, "A", &[&bytes, b"\nlast\n"]);
        let tail = line_tail(&path, 100, crate::config::TerminalMode::Classic).unwrap();
        let mut output = Vec::new();
        let mut stamps = Vec::new();
        visit_line_tail(&path, tail, |micros, chunk| {
            assert!(chunk.len() <= 64 * 1024);
            output.extend_from_slice(chunk);
            stamps.push(micros);
        })
        .unwrap();
        assert_eq!(output, [bytes.as_slice(), b"\nlast\n"].concat());
        assert_eq!(stamps, [0, 0, 0, 0, 1]);
    }

    #[test]
    fn line_count_uses_terminal_framing_and_forced_long_line_breaks() {
        let dir = tempfile::tempdir().unwrap();
        let path = saved_capture(dir.path(), 1, "A", &[b"a\rb\rc\n"]);
        assert_eq!(
            read_tail_records_by_lines(&path, 100, crate::config::TerminalMode::Classic)
                .unwrap()
                .1,
            3
        );
        assert_eq!(
            read_tail_records_by_lines(&path, 100, crate::config::TerminalMode::Vt100)
                .unwrap()
                .1,
            1
        );
        let long = vec![b'x'; crate::framer::MAX_LINE_LEN * 3 + 1];
        let path = saved_capture(dir.path(), 2, "A", &[&long]);
        assert_eq!(
            read_tail_records_by_lines(&path, 100, crate::config::TerminalMode::Classic)
                .unwrap()
                .1,
            4
        );
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
    fn removing_previewed_sessions_leaves_unpreviewed_sessions() {
        let dir = std::env::temp_dir().join(format!("smon-remove-preview-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let approved = dir.join("approved.session.bin");
        let unapproved = dir.join("unapproved.session.bin");
        std::fs::write(&approved, b"approved").unwrap();
        std::fs::write(&unapproved, b"unapproved").unwrap();

        remove_session_paths(std::slice::from_ref(&approved)).unwrap();

        assert!(!approved.exists());
        assert!(unapproved.exists());
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
