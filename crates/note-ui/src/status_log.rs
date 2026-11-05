//! The status line's log file: one line a second while a note is in front.
//!
//! The counters are the one part of the line that moves on its own, and a person measuring a stroke
//! should not have to copy them out of a pill by hand. The file sits beside the notes and the recent
//! index, so it is found the same way on every platform; a session header says when it started, and
//! each line carries a UTC timestamp. A file that grows past [`MAX_LOG_BYTES`] is rotated to
//! `status.log.1` on the next launch rather than growing without bound.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::note;

/// The file's name, beside the notes folder and `recent.json`.
pub const FILE: &str = "status.log";

/// How large the log may grow before the next launch rotates it.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// Where the log is written: beside the notes folder, so it does not depend on the working
/// directory and is found the same way as the recent index (see [`crate::recent::path`]).
pub fn path() -> PathBuf {
    let notes = note::root();
    match notes.parent() {
        Some(home) => home.join(FILE),
        None => notes.join(FILE),
    }
}

/// The open log, or a disabled one when the file could not be opened.
///
/// A log is a diagnostic, not a feature the note depends on: a folder that cannot be written to
/// leaves the app working and says `log: off` on the status line rather than refusing to start.
pub struct StatusLog {
    file: Option<File>,
    path: PathBuf,
}

impl StatusLog {
    /// Opens the log beside the notes, writing a session header.
    ///
    /// Tests get a disabled log: a unit test that builds the view must not write into the real
    /// application data folder. The log's own tests use [`Self::open_at`] with a temporary path.
    pub fn open() -> Self {
        if cfg!(test) {
            return Self {
                file: None,
                path: path(),
            };
        }

        Self::open_at(path())
    }

    /// Opens a log at `path`, for a caller that has chosen where it goes.
    fn open_at(path: PathBuf) -> Self {
        let mut log = Self { file: None, path };

        if let Some(parent) = log
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            if let Err(error) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "cheap-note: could not create the status log's folder {}: {error}",
                    parent.display()
                );
                return log;
            }
        }

        rotate_if_large(&log.path);

        match OpenOptions::new().create(true).append(true).open(&log.path) {
            Ok(mut file) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                let _ = writeln!(file, "# cheap-note status log");
                let _ = writeln!(
                    file,
                    "# session {} ({} {}, {})",
                    format_timestamp(now.as_secs(), now.subsec_millis()),
                    std::env::consts::OS,
                    env!("CARGO_PKG_VERSION"),
                    if cfg!(debug_assertions) {
                        "debug"
                    } else {
                        "release"
                    }
                );
                let _ = file.flush();
                log.file = Some(file);
            }
            Err(error) => {
                eprintln!(
                    "cheap-note: could not open the status log {}: {error}",
                    log.path.display()
                );
            }
        }

        log
    }

    /// Whether the log is open and will take lines.
    pub fn is_on(&self) -> bool {
        self.file.is_some()
    }

    /// Appends one timestamped line. A write that fails turns the log off rather than repeating the
    /// error every second; the status line then says `log: off`.
    pub fn write(&mut self, status: &str) {
        let Some(file) = self.file.as_mut() else {
            return;
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();

        if let Err(error) = writeln!(
            file,
            "{}  {status}",
            format_timestamp(now.as_secs(), now.subsec_millis())
        ) {
            eprintln!(
                "cheap-note: could not write the status log {}: {error}",
                self.path.display()
            );
            self.file = None;
        }
    }
}

/// Moves a log that has grown past [`MAX_LOG_BYTES`] aside, so the next session starts a new file.
fn rotate_if_large(path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() <= MAX_LOG_BYTES {
        return;
    }

    let old = path.with_extension("log.1");
    let _ = std::fs::remove_file(&old);
    let _ = std::fs::rename(path, old);
}

/// The UTC date and time of `secs` seconds and `millis` milliseconds since the epoch, in the
/// ISO 8601 shape a log reader expects.
fn format_timestamp(secs: u64, millis: u32) -> String {
    let days = (secs / 86_400) as i64;
    let of_day = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = of_day / 3_600;
    let minute = (of_day % 3_600) / 60;
    let second = of_day % 60;

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Days since 1970-01-01 to a proleptic Gregorian date, by Howard Hinnant's `civil_from_days`.
///
/// Written out rather than pulled in with a date library: the log needs one timestamp shape, and a
/// dependency for it would be larger than the whole module.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]

    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_iso_utc() {
        assert_eq!(format_timestamp(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_timestamp(1_700_000_000, 123),
            "2023-11-14T22:13:20.123Z"
        );
        assert_eq!(format_timestamp(951_782_400, 0), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn a_log_writes_a_timestamped_line() {
        let path =
            std::env::temp_dir().join(format!("cheap-note-status-log-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut log = StatusLog::open_at(path.clone());
        assert!(log.is_on());
        log.write("ink 1.2 ms");

        let text = std::fs::read_to_string(&path).expect("the log");
        assert!(text.contains("# cheap-note status log"));
        assert!(text.lines().any(|line| line.ends_with("  ink 1.2 ms")));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_path_is_beside_the_notes() {
        let log = path();
        assert_eq!(log.file_name(), Some(std::ffi::OsStr::new(FILE)));
        assert_eq!(log.parent(), note::root().parent());
    }
}
