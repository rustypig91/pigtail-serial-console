//! Immutable view snapshots and cancellable, atomic background exports.

use crate::clock::Timestamp;
use crate::store::{LineFlags, LineRef};
use crate::wake::Wake;
use crossbeam_channel::{Receiver, TryRecvError};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

struct Row {
    end: usize,
    ts: Timestamp,
    flags: LineFlags,
    tag: usize,
}

/// Copies only the selected text and export metadata, without formatting or
/// retaining GUI/connection state. One arena avoids an allocation per line.
/// Tags are already in the view's display form; `None` is a single-port export.
pub struct Snapshot {
    text: String,
    rows: Vec<Row>,
    tags: Option<Vec<String>>,
}

impl Snapshot {
    pub fn new(tags: Option<Vec<String>>) -> Self {
        Self {
            text: String::new(),
            rows: Vec::new(),
            tags,
        }
    }

    pub fn push(&mut self, line: LineRef<'_>, tag: usize) {
        self.text.push_str(line.text);
        self.rows.push(Row {
            end: self.text.len(),
            ts: line.meta.ts,
            flags: line.meta.flags,
            tag,
        });
    }

    fn write_to(&self, writer: &mut impl Write, csv: bool, control: &Control) -> io::Result<()> {
        control.check_cancelled()?;
        if csv {
            if self.tags.is_some() {
                write!(writer, "port,")?;
            }
            writeln!(writer, "wall_time,micros,flags,text")?;
        }
        let mut start = 0;
        // Check cancellation inside long lines too, including during CSV escaping.
        let mut writer = CancellableWriter { writer, control };
        for (index, row) in self.rows.iter().enumerate() {
            control.check_cancelled()?;
            let text = &self.text[start..row.end];
            start = row.end;
            if let Some(tags) = &self.tags {
                let tag = &tags[row.tag];
                if csv {
                    csv_field(&mut writer, tag)?;
                    write!(writer, ",")?;
                } else {
                    write!(writer, "{tag}  ")?;
                }
            }
            let wall = row.ts.wall.with_timezone(&chrono::Local);
            if csv {
                write!(
                    writer,
                    "{},{},{},",
                    wall.format("%Y-%m-%dT%H:%M:%S%.6f%:z"),
                    row.ts.micros,
                    row.flags.0,
                )?;
                csv_field(&mut writer, text)?;
                writeln!(writer)?;
            } else {
                writeln!(writer, "{}  {text}", wall.format("%Y-%m-%d %H:%M:%S%.3f"))?;
            }
            control.completed.store(index + 1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Serialize to a caller-owned writer. Background tasks also use this path,
    /// with progress and cancellation enabled.
    pub fn write(&self, writer: &mut impl Write, csv: bool) -> io::Result<()> {
        self.write_to(writer, csv, &Control::default())
    }
}

fn csv_field(writer: &mut impl Write, text: &str) -> io::Result<()> {
    if !text.contains([',', '"', '\n', '\r']) {
        return writer.write_all(text.as_bytes());
    }
    write!(writer, "\"")?;
    let mut parts = text.split('"');
    writer.write_all(parts.next().unwrap_or_default().as_bytes())?;
    for part in parts {
        write!(writer, "\"\"")?;
        writer.write_all(part.as_bytes())?;
    }
    write!(writer, "\"")
}

struct CancellableWriter<'a, W> {
    writer: &'a mut W,
    control: &'a Control,
}

impl<W: Write> Write for CancellableWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.control.check_cancelled()?;
        self.writer.write(&bytes[..bytes.len().min(64 * 1024)])
    }

    fn flush(&mut self) -> io::Result<()> {
        self.control.check_cancelled()?;
        self.writer.flush()
    }
}

const RUNNING: u8 = 0;
const CANCELLED: u8 = 1;
const COMMITTING: u8 = 2;

#[derive(Default)]
struct Control {
    phase: AtomicU8,
    completed: AtomicUsize,
}

impl Control {
    fn cancel(&self) -> bool {
        self.phase
            .compare_exchange(RUNNING, CANCELLED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn check_cancelled(&self) -> io::Result<()> {
        if self.phase.load(Ordering::SeqCst) == CANCELLED {
            Err(io::Error::other("Export cancelled"))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Cancelled,
}

/// Dropping the handle cancels an unfinished export and waits for worker cleanup,
/// including on app exit. An in-progress commit is allowed to finish.
/// Cancellation is accepted until the atomic replacement starts; after that
/// the UI should show Finishing and disable Cancel.
pub struct Task {
    pub path: PathBuf,
    pub total: usize,
    control: Arc<Control>,
    result: Receiver<io::Result<Outcome>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Task {
    pub fn start(snapshot: Snapshot, path: PathBuf, csv: bool, wake: Wake) -> io::Result<Self> {
        let total = snapshot.rows.len();
        let control = Arc::new(Control::default());
        let worker_control = control.clone();
        let worker_path = path.clone();
        let (tx, result) = crossbeam_channel::bounded(1);
        let worker = std::thread::Builder::new()
            .name("export".into())
            .spawn(move || {
                let result = run_export(&snapshot, &worker_path, csv, &worker_control);
                let _ = tx.send(result);
                wake.signal();
            })?;
        Ok(Self {
            path,
            total,
            control,
            result,
            worker: Some(worker),
        })
    }

    pub fn completed(&self) -> usize {
        self.control.completed.load(Ordering::Relaxed)
    }

    pub fn can_cancel(&self) -> bool {
        self.control.phase.load(Ordering::SeqCst) == RUNNING
    }

    pub fn cancelling(&self) -> bool {
        self.control.phase.load(Ordering::SeqCst) == CANCELLED
    }

    pub fn cancel(&self) -> bool {
        self.control.cancel()
    }

    pub fn poll(&self) -> Option<io::Result<Outcome>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(io::Error::other("Export worker stopped"))),
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.cancel();
        // Detached threads are terminated when main returns. Keep the process
        // alive until the temporary file has been removed or committed.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_export(
    snapshot: &Snapshot,
    path: &Path,
    csv: bool,
    control: &Control,
) -> io::Result<Outcome> {
    let result = crate::fs::atomic_write_with(path, |file| {
        let mut writer = BufWriter::new(file);
        snapshot.write_to(&mut writer, csv, control)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        // Linearize cancellation against commit: an accepted cancellation
        // always leaves the destination intact, even after the last row.
        control
            .phase
            .compare_exchange(RUNNING, COMMITTING, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| io::Error::other("Export cancelled"))?;
        Ok(())
    });
    match result {
        Err(_) if control.phase.load(Ordering::SeqCst) == CANCELLED => Ok(Outcome::Cancelled),
        Err(error) => Err(error),
        Ok(()) => Ok(Outcome::Completed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{LineMeta, PortId};
    use std::time::Duration;

    fn snapshot(text: &str) -> Snapshot {
        let meta = LineMeta {
            start: 0,
            len: text.len() as u32,
            ts: Timestamp {
                wall: chrono::DateTime::from_timestamp(1, 0).unwrap(),
                micros: 123,
            },
            port: PortId(1),
            flags: LineFlags::TX_ECHO,
            spans: Default::default(),
            cursor: None,
        };
        let mut snapshot = Snapshot::new(None);
        snapshot.push(LineRef { text, meta: &meta }, 0);
        snapshot
    }

    #[test]
    fn csv_preserves_unicode_quotes_newlines_empty_fields_and_metadata() {
        let snapshot = snapshot("é,\"quoted\"\r\nnext");
        let mut output = Vec::new();
        snapshot.write(&mut output, true).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("wall_time,micros,flags,text\n"));
        assert!(output.ends_with(",123,4,\"é,\"\"quoted\"\"\r\nnext\"\n"));
        let mut empty = Vec::new();
        csv_field(&mut empty, "").unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn worker_replaces_existing_file_reports_progress_and_wakes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.csv");
        std::fs::write(&path, "old").unwrap();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let task = Task::start(
            snapshot("received"),
            path.clone(),
            true,
            Wake::new(move || tx.send(()).unwrap()),
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(task.poll().unwrap().unwrap(), Outcome::Completed);
        assert_eq!(task.completed(), 1);
        assert!(!task.cancel());
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .ends_with(",123,4,received\n"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn cancellation_preserves_existing_destination_and_cleans_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.txt");
        std::fs::write(&path, "old").unwrap();
        let control = Control::default();
        assert!(control.cancel());
        assert_eq!(
            run_export(&snapshot("received"), &path, false, &control).unwrap(),
            Outcome::Cancelled
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn dropping_a_task_waits_for_temporary_file_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.txt");
        std::fs::write(&path, "old").unwrap();
        let control = Arc::new(Control::default());
        let worker_control = control.clone();
        let worker_path = path.clone();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let (result_tx, result_rx) = crossbeam_channel::bounded(1);
        let worker = std::thread::spawn(move || {
            let result = crate::fs::atomic_write_with(&worker_path, |file| {
                file.write_all(b"partial")?;
                // Hold an actual export temp file until Drop requests cancellation.
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                worker_control.check_cancelled()
            });
            result_tx.send(result.map(|()| Outcome::Completed)).unwrap();
        });
        let task = Task {
            path: path.clone(),
            total: 1,
            control: control.clone(),
            result: result_rx,
            worker: Some(worker),
        };
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        let (dropped_tx, dropped_rx) = crossbeam_channel::bounded(1);
        let dropping = std::thread::spawn(move || {
            drop(task);
            dropped_tx.send(()).unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while control.phase.load(Ordering::SeqCst) != CANCELLED {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(
            dropped_rx.recv_timeout(Duration::from_millis(100)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        );
        release_tx.send(()).unwrap();
        dropped_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        dropping.join().unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn cancellation_interrupts_a_large_line_and_csv_escaping() {
        struct CancelAfterChunk<'a> {
            control: &'a Control,
            written: usize,
        }
        impl Write for CancelAfterChunk<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.written += bytes.len();
                if self.written >= 64 * 1024 {
                    assert!(self.control.cancel());
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        for csv in [false, true] {
            let snapshot = snapshot(&format!("\"{}", "x".repeat(512 * 1024)));
            let control = Control::default();
            let mut writer = CancelAfterChunk {
                control: &control,
                written: 0,
            };
            assert!(snapshot.write_to(&mut writer, csv, &control).is_err());
            assert!(writer.written < 128 * 1024);
            assert_eq!(control.completed.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn errors_during_writing_and_commit_preserve_destination_and_clean_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.txt");
        std::fs::write(&path, "old").unwrap();
        let error = crate::fs::atomic_write_with(&path, |file| {
            file.write_all(b"partial")?;
            Err(io::Error::other("disk full"))
        });
        assert!(error.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        let directory = dir.path().join("directory");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("keep"), "old").unwrap();
        assert!(run_export(&snapshot("new"), &directory, true, &Control::default()).is_err());
        assert_eq!(
            std::fs::read_to_string(directory.join("keep")).unwrap(),
            "old"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn empty_exports_commit() {
        let dir = tempfile::tempdir().unwrap();
        for csv in [false, true] {
            let path = dir.path().join("empty");
            assert_eq!(
                run_export(&Snapshot::new(None), &path, csv, &Control::default()).unwrap(),
                Outcome::Completed
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                if csv {
                    "wall_time,micros,flags,text\n"
                } else {
                    ""
                }
            );
        }
    }

    #[test]
    fn cancelling_a_running_worker_preserves_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.csv");
        std::fs::write(&path, "old").unwrap();
        let mut snapshot = snapshot("received");
        let ts = snapshot.rows[0].ts;
        for _ in 0..100_000 {
            snapshot.text.push_str("received");
            snapshot.rows.push(Row {
                end: snapshot.text.len(),
                ts,
                flags: LineFlags::default(),
                tag: 0,
            });
        }
        let task = Task::start(snapshot, path.clone(), true, Wake::none()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while task.completed() == 0 && task.can_cancel() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let accepted = task.cancel();
        let outcome = task
            .result
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        if accepted {
            assert_eq!(outcome, Outcome::Cancelled);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        } else {
            // A fast worker is permitted to win the cancellation/commit race.
            assert_eq!(outcome, Outcome::Completed);
            assert_ne!(std::fs::read_to_string(&path).unwrap(), "old");
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
