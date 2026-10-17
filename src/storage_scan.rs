//! Explicit, bounded filesystem analysis. No worker exists between requests.
use serde::Serialize;
use std::{
    io::{self, Read},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

const OUTPUT_LIMIT: usize = 256 * 1024;
const TIME_LIMIT: Duration = Duration::from_secs(10);

#[derive(Serialize)]
pub struct Entry {
    pub name: String,
    pub bytes: u64,
}
#[derive(Serialize)]
pub struct Scan {
    pub entries: Vec<Entry>,
    pub total_bytes: Option<u64>,
    pub files_bytes: Option<u64>,
    pub other_folders_bytes: u64,
    pub complete: bool,
    pub message: String,
}

pub struct Lease(Arc<AtomicBool>);
impl Lease {
    pub fn acquire(busy: &Arc<AtomicBool>) -> Option<Self> {
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .ok()
            .map(|_| Self(busy.clone()))
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn nonblocking(file: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: these calls operate only on the child's owned pipe descriptor.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn analyze(path: &Path, cancelled: impl Fn() -> bool) -> Result<Scan, String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("Invalid filesystem mount path".into());
    }
    let mut command = Command::new("/usr/bin/du");
    command
        .args([
            "--no-dereference",
            "--one-file-system",
            "--block-size=1",
            "--max-depth=1",
            "--null",
            "--",
        ])
        .arg(path)
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: the child only invokes async-signal-safe resource syscalls before
    // exec; no allocation, locks, filesystem writes, or privilege changes occur.
    unsafe {
        command.pre_exec(|| {
            libc::nice(19);
            let memory = libc::rlimit {
                rlim_cur: 64 * 1024 * 1024,
                rlim_max: 64 * 1024 * 1024,
            };
            let cpu = libc::rlimit {
                rlim_cur: 5,
                rlim_max: 5,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &memory) != 0
                || libc::setrlimit(libc::RLIMIT_CPU, &cpu) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("Cannot start folder analysis (GNU du required): {e}"))?;
    let mut output = child.stdout.take().unwrap();
    let mut errors = child.stderr.take().unwrap();
    if let Err(error) = nonblocking(&output).and_then(|_| nonblocking(&errors)) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.to_string());
    }
    let started = Instant::now();
    let mut bytes = Vec::new();
    let mut denied = false;
    let mut stopped = false;
    let mut buffer = [0u8; 8192];
    let status = loop {
        if cancelled() || started.elapsed() >= TIME_LIMIT || bytes.len() >= OUTPUT_LIMIT {
            stopped = true;
            let _ = child.kill();
        }
        match output.read(&mut buffer) {
            Ok(count) => bytes
                .extend_from_slice(&buffer[..count.min(OUTPUT_LIMIT.saturating_sub(bytes.len()))]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => {
                stopped = true;
                let _ = child.kill();
            }
        }
        match errors.read(&mut buffer) {
            Ok(count) => denied |= count > 0,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => denied = true,
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // The process has exited, so drain the remaining bounded pipe.
                loop {
                    match output.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => {
                            let room = OUTPUT_LIMIT.saturating_sub(bytes.len());
                            bytes.extend_from_slice(&buffer[..count.min(room)]);
                            if count > room {
                                stopped = true;
                            }
                        }
                    }
                }
                break status;
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.to_string());
            }
        }
    };
    let mut total = None;
    let mut entries = Vec::new();
    let root = path.to_string_lossy();
    for record in bytes.split_inclusive(|b| *b == 0) {
        if record.last() != Some(&0) {
            continue;
        }
        let Some(tab) = record.iter().position(|b| *b == b'\t') else {
            continue;
        };
        let Ok(size) = std::str::from_utf8(&record[..tab])
            .unwrap_or("")
            .parse::<u64>()
        else {
            continue;
        };
        let reported = String::from_utf8_lossy(&record[tab + 1..record.len() - 1]);
        if reported == root {
            total = Some(size);
            continue;
        }
        let Ok(relative) = Path::new(reported.as_ref()).strip_prefix(path) else {
            continue;
        };
        if relative.components().count() != 1 {
            continue;
        }
        entries.push(Entry {
            name: relative.to_string_lossy().into_owned(),
            bytes: size,
        });
    }
    let folder_bytes = entries
        .iter()
        .fold(0u64, |sum, e| sum.saturating_add(e.bytes));
    entries.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    let other = entries
        .iter()
        .skip(12)
        .fold(0u64, |sum, e| sum.saturating_add(e.bytes));
    entries.truncate(12);
    let complete = !stopped && !denied && status.success() && total.is_some();
    let message = if stopped {
        "Scan limit reached. These are partial results; unfinished folders may be absent."
    } else if !complete {
        "Some paths could not be read. These are partial results from accessible files."
    } else {
        "Allocated file space. Shared data, snapshots and filesystem metadata can make totals differ from used space."
    };
    Ok(Scan {
        entries,
        total_bytes: total,
        files_bytes: total.map(|n| n.saturating_sub(folder_bytes)),
        other_folders_bytes: other,
        complete,
        message: message.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_scan_returns_partial_and_reaps_child_promptly() {
        let directory =
            std::env::temp_dir().join(format!("oyasys-cancel-scan-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let started = Instant::now();
        let scan = analyze(&directory, || true);
        std::fs::remove_dir(&directory).unwrap();
        let scan = scan.unwrap();
        assert!(!scan.complete);
        assert!(started.elapsed() < Duration::from_secs(2));
        // try_wait has reaped the child before analyze returns.
    }
}
