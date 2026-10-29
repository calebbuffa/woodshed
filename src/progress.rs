//! Live phase reporting, independent of Cargo's captured build-script stdout.
//!
//! `WOODSHED_PROGRESS=0` disables stderr progress. `WOODSHED_LOG` names an
//! append-only progress log; its parent directory must already exist.
//! Log records contain Unix timestamps, process IDs and phase elapsed times.

use std::{
    fs::OpenOptions,
    io::{self, Write},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Emit a progress message to stderr and the optional `WOODSHED_LOG` file.
///
/// Never writes stdout. File errors are returned rather than silently ignored.
/// Concurrent processes serialize complete log records with an advisory lock.
pub fn report(message: &str) -> io::Result<()> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs_f64();
    let message = message.replace(['\r', '\n'], " ");
    let line = format!(
        "[{timestamp:.3} pid={}] woodshed: {message}\n",
        std::process::id()
    );
    if std::env::var_os("WOODSHED_PROGRESS").is_none_or(|value| value != "0") {
        io::stderr().lock().write_all(line.as_bytes())?;
    }
    if let Some(path) = std::env::var_os("WOODSHED_LOG") {
        append(std::path::Path::new(&path), &line)?;
    }
    Ok(())
}

fn append(path: &std::path::Path, line: &str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    fs4::fs_std::FileExt::lock_exclusive(&file)?;
    file.write_all(line.as_bytes())?;
    file.flush()
}

pub(crate) fn notify(message: &str) {
    if let Err(error) = report(message) {
        eprintln!("woodshed: progress reporting failed: {error}");
    }
}

pub(crate) struct Phase {
    label: String,
    start: Instant,
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Phase {
    pub(crate) fn new(label: impl Into<String>) -> Self {
        let label = label.into();
        let start = Instant::now();
        notify(&format!("starting {label}"));
        let (stop, receiver) = mpsc::channel();
        let task = label.clone();
        let worker = thread::spawn(move || {
            loop {
                match receiver.recv_timeout(Duration::from_secs(10)) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        notify(&format!(
                            "{task}: still running ({:.1}s elapsed)",
                            start.elapsed().as_secs_f64()
                        ));
                    }
                    _ => return,
                }
            }
        });
        Self {
            label,
            start,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err())
        {
            eprintln!("woodshed: progress heartbeat thread failed");
        }
        notify(&format!(
            "ended {} ({:.1}s elapsed; see operation result for success/failure)",
            self.label,
            self.start.elapsed().as_secs_f64()
        ));
    }
}

#[cfg(all(test, feature = "build"))]
mod tests {
    use super::*;

    #[test]
    fn concurrent_log_records_remain_complete_and_append() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("progress.log");
        std::thread::scope(|scope| {
            for id in 0..4 {
                let path = &path;
                scope.spawn(move || {
                    for sequence in 0..20 {
                        append(path, &format!("worker-{id}-{sequence}\n")).unwrap();
                    }
                });
            }
        });
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text.lines().count(), 80);
        for id in 0..4 {
            for sequence in 0..20 {
                assert!(
                    text.lines()
                        .any(|line| line == format!("worker-{id}-{sequence}"))
                );
            }
        }
    }

    #[test]
    fn invalid_log_destination_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        assert!(append(directory.path(), "event\n").is_err());
    }
}
