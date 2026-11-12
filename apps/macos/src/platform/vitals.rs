//! What this process is using, read off the system.
//!
//! One call — [`ProcessStats::vitals`] — and the numbers it returns are the process's own: resident
//! memory, processor share, thread count. No permission is needed for any of them, which is why this is
//! the one service on this platform that cannot fail to install: a sandbox still describes its own
//! process.
//!
//! ## Why `sysinfo` rather than `task_info`
//!
//! Because the Windows build answers the same question with the same crate, and a seam that both
//! platforms implement the same way is a seam whose two halves can be compared. `mach_task_basic_info`
//! would give the resident size and a thread count, but the *processor share* is the awkward one — it
//! needs two samples of accumulated CPU time against a wall clock, divided by a core count taken from yet
//! another call — and every line of that arithmetic is a place for the figure to disagree with Activity
//! Monitor. `sysinfo` does it, is already compiled for this target (GPUI pulls it in), and its
//! `cpu_usage` follows the convention both platforms' own tools show.
//!
//! ## Why it is not in `note-ui`
//!
//! Because `sysinfo` declares `[target.…]` dependencies, and a crate on the view's own dependency edges
//! must not (`doc/MACOS.md` §6). The view gets [`Vitals`] — a plain struct of three numbers — and the
//! seam trait answers it; gathering them happens here, where an operating system is allowed to be named.

use note_ui::platform::SystemStats;
use note_ui::platform::Vitals;
use sysinfo::{
    get_current_pid, CpuRefreshKind, MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate,
    RefreshKind, System,
};

/// The process's own counters, kept between calls.
///
/// `System` is held rather than made per call because a processor figure is a *difference* between two
/// readings: a fresh `System` has no previous sample, so every reading would be the first and the CPU
/// column would sit at zero forever. It also caches the process table, which is what makes a refresh of
/// one PID cheap.
pub struct ProcessStats {
    system: System,
    pid: Pid,
    /// The last reading, so a refresh the platform refuses can answer with the one before it rather than
    /// with nothing: a figure a second old is more useful in a line that is rebuilt every second than a
    /// clause that flickers in and out.
    last: Option<Vitals>,
}

impl ProcessStats {
    /// Where the reading starts, or `None` when the system will not name this process at all.
    ///
    /// `Option` rather than a `Result`: a build that cannot number its own process is a status line
    /// without a hardware clause, and there is nothing a person could do about it.
    ///
    /// `System::new_with_specifics` answers a `Result` since `sysinfo` 0.40, where it used to hand back
    /// the `System` directly — the crate grew a fallible constructor, and a `None` here is the same
    /// answer this function already gave for a process it could not name.
    pub fn install() -> Option<Self> {
        let pid = get_current_pid().ok()?;

        // `nothing()` rather than `new()`: 0.40 renamed the empty constructor, and every `with_…` below
        // is what adds back the three things this reads. Asking for only those is what keeps the
        // refresh to one process's counters instead of the machine's whole table.
        let system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_memory(MemoryRefreshKind::everything())
                // `with_usage`, not `with_cpu_usage`: 0.40 shortened the name.
                .with_cpu(CpuRefreshKind::nothing().with_usage())
                .with_processes(ProcessRefreshKind::nothing().with_cpu().with_memory()),
        )
        .ok()?;

        Some(ProcessStats {
            system,
            pid,
            last: None,
        })
    }
}

impl SystemStats for ProcessStats {
    fn vitals(&mut self) -> Option<Vitals> {
        // One process, not the table: `Some(&[pid])` is what stops this from walking every process on
        // the machine once a second, which would be a status line that costs more than it reports.
        //
        // The `false` is `remove_dead_processes`, which 0.40 added as an explicit argument — it used to
        // be implied by `ProcessesToUpdate::All`. This asks about one process that is this one, so
        // there is never a dead one to drop: a `true` here would clear the entry it is about to read.
        self.system
            .refresh_processes(ProcessesToUpdate::Some(&[self.pid]), false);

        let Some(process) = self.system.process(self.pid) else {
            // The system would not describe this process. The previous reading, if any, rather than a
            // clause that blinks out of the line.
            return self.last;
        };

        let vitals = Vitals {
            resident_bytes: process.memory(),
            cpu_percent: process.cpu_usage(),
            threads: thread_count(self.pid.as_u32()),
        };

        self.last = Some(vitals);
        Some(vitals)
    }
}

/// How many threads this process has, from `proc_pidinfo(PROC_PIDTASKINFO)`.
///
/// A second call because `sysinfo` cannot answer this one: its `Process::tasks` is Linux-only and returns
/// `None` here. The struct it fills is the same one the crate reads for its own figures, so this agrees
/// with the memory in [`Vitals`] by construction rather than by luck.
///
/// `0` for a process the kernel will not describe, which [`Vitals::summary`] treats as "not worth
/// saying" and leaves out — the same answer as having no counter at all.
fn thread_count(pid: u32) -> usize {
    // SAFETY: `proc_pidinfo` fills the struct it is handed and reports how many bytes it wrote. The
    // pointer and the size are those of the same struct, and a short write is checked before the field
    // is read.
    let info = unsafe {
        let mut info = std::mem::zeroed::<libc::proc_taskinfo>();
        let written = libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int,
        );

        (written == std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int).then_some(info)
    };

    info.map_or(0, |info| info.pti_threadnum.max(0) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counters describe *this* process, and the numbers are of the sizes they claim to be.
    ///
    /// The assertions are deliberately loose — this is a live machine, and a test that pinned a
    /// megabyte count would fail on the next `cargo build`. What is checked is that the reading is
    /// plausible rather than exact: a running tester holds some memory, has at least one thread, and is
    /// not using a negative share of a core.
    #[test]
    fn the_counters_describe_this_process() {
        let mut stats = ProcessStats::install().expect("the process's own counters");

        let vitals = stats.vitals().expect("a reading for this process");

        assert!(
            vitals.resident_bytes > 0,
            "a running process holds memory: {} bytes",
            vitals.resident_bytes
        );
        assert!(
            vitals.threads > 0,
            "a running process has threads: {}",
            vitals.threads
        );
        assert!(
            vitals.cpu_percent >= 0.0,
            "a share of a core cannot be negative: {}",
            vitals.cpu_percent
        );
    }

    /// A second reading is still a reading.
    ///
    /// This is what makes the processor figure mean anything — it is a difference against the last
    /// refresh — so a `System` rebuilt per call would pass the test above and still report zero forever.
    /// The sleep is the crate's own minimum, which is what its docs ask of a caller before reading a
    /// share.
    #[test]
    fn a_second_reading_still_answers() {
        let mut stats = ProcessStats::install().expect("the process's own counters");

        stats.vitals().expect("the first reading");
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let second = stats.vitals().expect("the second reading");

        assert!(second.resident_bytes > 0);
        assert!(second.threads > 0);
    }

    /// What the clause looks like, and that it names the three figures it claims.
    ///
    /// Pinned as a shape rather than as a number, because the numbers are the machine's.
    #[test]
    fn the_clause_names_memory_cpu_and_threads() {
        let clause = Vitals {
            resident_bytes: 128 * 1024 * 1024,
            cpu_percent: 37.4,
            threads: 9,
        }
        .summary();

        assert_eq!(clause, "128 MiB · 37% cpu · 9 threads");
    }
}
