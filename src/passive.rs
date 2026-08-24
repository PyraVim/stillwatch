//! Watching something you cannot modify.
//!
//! People will not add code to a job before they trust the tool, and a
//! consultant cannot modify a client's system before being hired. Passive mode
//! is what makes stillwatch usable on day one: point it at a pidfile, a log and
//! an output file and it can say something useful without anything changing.
//!
//! Every signal here is *indirect*, and that is carried through to the alerts
//! rather than left in the documentation. A pidfile says a process id exists.
//! A log says bytes were written. An artifact says a file has a size. None of
//! them says the job did its work.
//!
//! The trap this module is mostly built around is **log rotation**. Tail a file
//! by holding its handle and a rotation leaves you reading a dead inode
//! forever: the log looks permanently stale while the job is fine, or the tail
//! silently stops seeing anything while reporting that all is well. That is
//! precisely the failure this tool exists to catch, so it would be
//! embarrassing to ship it inside the tool.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime};

use regex::Regex;
use tokio::time::MissedTickBehavior;

use crate::config::JobConfig;
use crate::state::{FileFinding, LogSample, ProcessFinding, SharedState};

/// How often the filesystem is looked at.
///
/// Not configurable. These are a `stat` apiece and the thresholds they feed are
/// minutes to hours, so there is nothing here worth a knob.
const POLL: Duration = Duration::from_secs(15);

/// Most that is read out of a log in one poll.
///
/// A job that dumps a hundred megabytes between two looks is having a worse
/// problem than a missed error line, and stillwatch must not follow it into
/// memory exhaustion.
const MAX_SCAN_BYTES: u64 = 4 * 1024 * 1024;

/// Watches one job's filesystem signals.
///
/// Holds the tail position between looks, which is the only state a passive
/// watcher needs — and the reason this is a struct rather than a free function.
pub struct Watcher {
    job: JobConfig,
    tail: Tail,
}

impl Watcher {
    pub fn new(job: JobConfig) -> Self {
        Self {
            job,
            tail: Tail::default(),
        }
    }

    /// Takes one look at everything this job is watched by.
    pub fn poll(&mut self, state: &SharedState, now: SystemTime) {
        let job = &self.job;

        if let Some(watch) = &job.process {
            let finding = read_pidfile(&watch.pidfile);
            tracing::trace!(job = %job.name, ?finding, "pidfile");
            state.record_process(&job.name, now, finding);
        }

        if let Some(watch) = &job.log {
            let sample = self.tail.poll(&watch.path, watch.error_regex.as_ref());
            if sample.rotated {
                tracing::info!(job = %job.name, path = %watch.path.display(), "log rotated");
            }
            state.record_log(&job.name, now, sample);
        }

        if let Some(watch) = &job.artifact {
            let finding = look(&watch.path);
            state.record_artifact(&job.name, finding);
        }
    }
}

/// Watches one job's filesystem signals forever.
pub async fn run(job: JobConfig, state: SharedState) {
    let mut ticker = tokio::time::interval(POLL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut watcher = Watcher::new(job);

    loop {
        ticker.tick().await;
        watcher.poll(&state, SystemTime::now());
    }
}

// ---------------------------------------------------------------------------
// files
// ---------------------------------------------------------------------------

/// Stats a path, without holding on to anything.
///
/// Deliberately by path every time rather than through a kept handle: after a
/// rotation a handle points at the file that *was* there, which is how a
/// perfectly healthy job comes to look permanently stale.
fn look(path: &Path) -> FileFinding {
    match std::fs::metadata(path) {
        Ok(metadata) => FileFinding::Present {
            size: metadata.len(),
            modified: metadata.modified().ok(),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => FileFinding::Missing,
        Err(err) => FileFinding::Unreadable(err.to_string()),
    }
}

/// Identifies a file well enough to notice it has been replaced.
///
/// `(device, inode)` on Unix and `(volume serial, file index)` on Windows —
/// the same idea under two names, and exact on both.
///
/// Creation time was tried first on Windows and is not good enough: NTFS
/// "tunneling" hands a newly created file the timestamps of one deleted from
/// the same directory moments earlier, so a rotated log looks like the file it
/// replaced. A test caught it. `std`'s `file_index` is nightly-only, hence the
/// direct call below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId(u64, u64);

#[cfg(unix)]
fn identify(metadata: &std::fs::Metadata, _path: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    Some(FileId(metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn identify(_metadata: &std::fs::Metadata, path: &Path) -> Option<FileId> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let file = File::open(path).ok()?;

    // SAFETY: the handle comes from a live `File` that outlives the call, and
    // the struct is written to only by the callee.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
    if ok == 0 {
        return None;
    }

    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Some(FileId(u64::from(info.dwVolumeSerialNumber), index))
}

#[cfg(not(any(unix, windows)))]
fn identify(_metadata: &std::fs::Metadata, _path: &Path) -> Option<FileId> {
    None
}

/// Follows a log across rotations.
#[derive(Debug, Default)]
struct Tail {
    id: Option<FileId>,
    offset: u64,
}

impl Tail {
    fn poll(&mut self, path: &Path, error_regex: Option<&Regex>) -> LogSample {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Rotation often means a moment with no file at all. Forget
                // where we were so the replacement is read from its start.
                self.id = None;
                self.offset = 0;
                return LogSample {
                    finding: FileFinding::Missing,
                    rotated: false,
                    error_line: None,
                };
            }
            Err(err) => {
                return LogSample {
                    finding: FileFinding::Unreadable(err.to_string()),
                    rotated: false,
                    error_line: None,
                }
            }
        };

        let size = metadata.len();
        let id = identify(&metadata, path);

        // Two independent signals, because neither catches everything. A
        // changed identity catches rename-and-recreate. A file shorter than
        // what has already been read catches copytruncate, and covers the
        // identity check being a proxy rather than an inode.
        let replaced = self.id.is_some() && id != self.id;
        let truncated = size < self.offset;
        let rotated = replaced || truncated;

        if rotated {
            self.offset = 0;
        }
        self.id = id;

        let error_line = error_regex.and_then(|regex| self.scan(path, size, regex));
        // Whether or not anything was scanned, everything up to here has now
        // been accounted for.
        self.offset = size;

        LogSample {
            finding: FileFinding::Present {
                size,
                modified: metadata.modified().ok(),
            },
            rotated,
            error_line,
        }
    }

    /// Reads what has arrived since the last look and returns the last line
    /// matching the pattern.
    fn scan(&mut self, path: &Path, size: u64, regex: &Regex) -> Option<String> {
        if size <= self.offset {
            return None;
        }

        // A very large jump means either a rotation that was missed or a job
        // writing enormously. Read only the tail of it rather than all of it.
        let from = if size - self.offset > MAX_SCAN_BYTES {
            size - MAX_SCAN_BYTES
        } else {
            self.offset
        };

        let file = File::open(path).ok()?;
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(from)).ok()?;

        let mut last = None;
        for line in reader.take(MAX_SCAN_BYTES).lines() {
            let Ok(line) = line else { break };
            if regex.is_match(&line) {
                last = Some(line);
            }
        }

        self.offset = size;
        last
    }
}

// ---------------------------------------------------------------------------
// processes
// ---------------------------------------------------------------------------

fn read_pidfile(path: &Path) -> ProcessFinding {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return ProcessFinding::PidfileMissing
        }
        Err(err) => return ProcessFinding::PidfileUnreadable(err.to_string()),
    };

    let trimmed = text.trim();
    let Ok(pid) = trimmed.parse::<u32>() else {
        return ProcessFinding::PidfileUnreadable(format!(
            "expected a process id, found {:?}",
            trimmed.chars().take(40).collect::<String>()
        ));
    };

    // 0 and anything past `pid_t` parse as u32 but are not process ids. They
    // come from a truncated or half-written pidfile, and calling either one an
    // outage would blame the job for what is really an unreadable file.
    //
    // On Unix they are worse than meaningless: `kill` reads 0 as the caller's
    // own process group, and a value that overflows `pid_t` arrives as a
    // negative pid, which is a broadcast. Both answer "running" for a file that
    // says no such thing.
    if pid == 0 || pid > i32::MAX as u32 {
        return ProcessFinding::PidfileUnreadable(format!("not a process id: {pid}"));
    }

    if is_running(pid) {
        ProcessFinding::Running { pid }
    } else {
        ProcessFinding::NotRunning { pid }
    }
}

/// Whether a process with this id exists.
///
/// Says nothing about whether it is *the* process: process ids are reused, so a
/// stale pidfile whose number has been handed to something unrelated reads as
/// healthy. There is no portable fix, and the README says so plainly rather
/// than the alert implying more certainty than there is.
#[cfg(unix)]
fn is_running(pid: u32) -> bool {
    // `kill` reads a pid of 0 as "every process in the caller's own process
    // group" and a negative pid as a process group or a broadcast, so both
    // succeed for as long as stillwatch itself is alive. Unguarded, either
    // reports the watched job healthy on the strength of the watchdog being
    // healthy. Windows has no such aliasing, which is why this only ever failed
    // on Unix.
    if pid == 0 {
        return false;
    }
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };

    // Signal 0 performs the permission and existence checks without sending
    // anything. `EPERM` means the process exists and is not ours, which still
    // answers the question being asked.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn is_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // SAFETY: `OpenProcess` takes no pointers and returns a handle we close.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if !handle.is_null() {
        unsafe { CloseHandle(handle) };
        return true;
    }

    // As on Unix, being refused access means it is there.
    unsafe { GetLastError() == ERROR_ACCESS_DENIED }
}

#[cfg(not(any(unix, windows)))]
fn is_running(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::write(path, contents).expect("write");
    }

    fn append(path: &Path, contents: &str) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open for append");
        file.write_all(contents.as_bytes()).expect("append");
    }

    fn errors() -> Regex {
        Regex::new("(?i)(traceback|fatal|failed to write)").expect("valid regex")
    }

    // -- the rotation trap -------------------------------------------------

    /// The failure this module is built around: after a rotation the tail must
    /// follow the live file. Reading a handle to the old one leaves a healthy
    /// job looking permanently stale.
    #[test]
    fn a_rotated_log_is_followed_rather_than_left_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("etl.log");
        let rotated = dir.path().join("etl.log.1");

        write(&path, "starting up\nwrote 400 rows\n");
        let mut tail = Tail::default();
        let first = tail.poll(&path, Some(&errors()));
        assert!(!first.rotated);
        assert_eq!(first.error_line, None);

        // logrotate's usual move: rename the old file, create a new one.
        std::fs::rename(&path, &rotated).expect("rename");
        write(&path, "wrote 12 rows\nFATAL: out of memory\n");

        let second = tail.poll(&path, Some(&errors()));

        assert!(second.rotated, "the replacement must be noticed");
        assert_eq!(
            second.error_line.as_deref(),
            Some("FATAL: out of memory"),
            "and read from its start, not from the old offset"
        );
        assert!(matches!(second.finding, FileFinding::Present { .. }));
    }

    /// The other rotation style: same file, truncated in place. The identity
    /// never changes, so only the size check catches it.
    #[test]
    fn a_log_truncated_in_place_is_noticed_by_size_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("etl.log");

        write(&path, &"x".repeat(5_000));
        let mut tail = Tail::default();
        tail.poll(&path, Some(&errors()));
        assert_eq!(tail.offset, 5_000);

        // copytruncate: the file keeps its identity and loses its contents.
        write(&path, "Traceback (most recent call last):\n");
        let sample = tail.poll(&path, Some(&errors()));

        assert!(sample.rotated, "a shorter file must count as rotated");
        assert!(sample.error_line.is_some(), "and be re-read from the start");
    }

    #[test]
    fn ordinary_growth_is_not_mistaken_for_rotation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("etl.log");

        write(&path, "line one\n");
        let mut tail = Tail::default();
        tail.poll(&path, Some(&errors()));

        append(&path, "line two\n");
        let sample = tail.poll(&path, Some(&errors()));

        assert!(!sample.rotated);
        assert_eq!(sample.error_line, None);
    }

    #[test]
    fn a_log_that_vanishes_and_returns_is_read_from_the_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("etl.log");

        write(&path, &"x".repeat(4_000));
        let mut tail = Tail::default();
        tail.poll(&path, Some(&errors()));

        std::fs::remove_file(&path).expect("remove");
        let gone = tail.poll(&path, Some(&errors()));
        assert_eq!(gone.finding, FileFinding::Missing);

        write(&path, "failed to write batch 7\n");
        let back = tail.poll(&path, Some(&errors()));

        assert_eq!(
            back.error_line.as_deref(),
            Some("failed to write batch 7"),
            "the offset from the old file must not carry over"
        );
    }

    // -- scanning ----------------------------------------------------------

    #[test]
    fn only_lines_arriving_since_the_last_look_are_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("etl.log");

        write(&path, "FATAL: first\n");
        let mut tail = Tail::default();
        assert_eq!(
            tail.poll(&path, Some(&errors())).error_line.as_deref(),
            Some("FATAL: first")
        );

        // Nothing new: the same line must not be reported again forever.
        assert_eq!(tail.poll(&path, Some(&errors())).error_line, None);

        append(&path, "wrote 4 rows\n");
        assert_eq!(tail.poll(&path, Some(&errors())).error_line, None);

        append(&path, "Traceback (most recent call last):\n");
        assert_eq!(
            tail.poll(&path, Some(&errors())).error_line.as_deref(),
            Some("Traceback (most recent call last):")
        );
    }

    #[test]
    fn the_most_recent_match_wins_when_several_arrive_at_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("etl.log");

        write(&path, "FATAL: one\nok\nFATAL: two\n");
        let mut tail = Tail::default();

        assert_eq!(
            tail.poll(&path, Some(&errors())).error_line.as_deref(),
            Some("FATAL: two")
        );
    }

    #[test]
    fn a_log_with_no_pattern_configured_is_still_watched_for_movement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("etl.log");
        write(&path, "anything\n");

        let mut tail = Tail::default();
        let sample = tail.poll(&path, None);

        assert!(matches!(
            sample.finding,
            FileFinding::Present { size: 9, .. }
        ));
        assert_eq!(sample.error_line, None);
    }

    #[test]
    fn a_log_that_never_existed_reports_missing_not_stale() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tail = Tail::default();

        let sample = tail.poll(&dir.path().join("never-here.log"), Some(&errors()));

        assert_eq!(sample.finding, FileFinding::Missing);
        assert!(!sample.rotated);
    }

    // -- artifacts ---------------------------------------------------------

    #[test]
    fn looking_at_a_file_reports_its_size_and_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daily.csv");
        write(&path, "id,name\n1,a\n");

        match look(&path) {
            FileFinding::Present { size, modified } => {
                assert_eq!(size, 12);
                assert!(modified.is_some());
            }
            other => panic!("expected a present file, got {other:?}"),
        }
    }

    #[test]
    fn looking_at_a_missing_file_says_missing_rather_than_erroring() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(look(&dir.path().join("nope.csv")), FileFinding::Missing);
    }

    // -- pidfiles ----------------------------------------------------------

    #[test]
    fn a_pidfile_naming_this_process_reads_as_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("etl.pid");
        write(&path, &std::process::id().to_string());

        assert_eq!(
            read_pidfile(&path),
            ProcessFinding::Running {
                pid: std::process::id()
            }
        );
    }

    #[test]
    fn a_pidfile_with_trailing_whitespace_still_parses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("etl.pid");
        write(&path, &format!("{}\n", std::process::id()));

        assert!(matches!(
            read_pidfile(&path),
            ProcessFinding::Running { .. }
        ));
    }

    #[test]
    fn an_absent_pidfile_is_told_apart_from_an_unreadable_one() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert_eq!(
            read_pidfile(&dir.path().join("nothing.pid")),
            ProcessFinding::PidfileMissing
        );

        let garbage = dir.path().join("garbage.pid");
        write(&garbage, "not a pid at all");
        assert!(matches!(
            read_pidfile(&garbage),
            ProcessFinding::PidfileUnreadable(_)
        ));
    }

    #[test]
    fn an_empty_pidfile_is_unreadable_rather_than_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.pid");
        write(&path, "");

        assert!(matches!(
            read_pidfile(&path),
            ProcessFinding::PidfileUnreadable(_)
        ));
    }

    #[test]
    fn this_process_is_running_and_a_reserved_id_is_not() {
        assert!(is_running(std::process::id()));
        assert!(!is_running(0));
    }

    /// A pidfile of `0` used to report the job *running*, because on Unix
    /// `kill(0, 0)` asks about the caller's own process group and therefore
    /// always succeeds. A watchdog answering "your job is fine" on the strength
    /// of its own process existing is the exact failure this tool is against, so
    /// it gets a test with its name on it.
    #[test]
    fn a_pidfile_of_zero_is_corrupt_rather_than_a_healthy_process() {
        let dir = tempfile::tempdir().expect("tempdir");

        // 0 is the caller's process group; anything past `pid_t` arrives as a
        // negative pid, which is a broadcast. Both used to answer "running".
        for corrupt in ["0\n", "4294967295\n"] {
            let path = dir.path().join("corrupt.pid");
            write(&path, corrupt);

            let finding = read_pidfile(&path);
            assert!(
                matches!(finding, ProcessFinding::PidfileUnreadable(_)),
                "{corrupt:?} is not a process id: {finding:?}"
            );
        }
    }
}
