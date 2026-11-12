//! What this process is using, read off the system.
//!
//! The Windows half of the service the macOS build implements in `apps/macos/src/platform/vitals.rs`, and
//! deliberately the same shape: one call, three numbers, the process's own counters. The two platforms
//! differ in everything below this — `GetProcessMemoryInfo` and the PDH counters one way, `task_info` the
//! other — and in nothing above it, which is what makes the two status lines comparable.
//!
//! ## Why `sysinfo` rather than `GetProcessMemoryInfo` + PDH
//!
//! Because the processor share is the figure that needs the work: `GetProcessMemoryInfo` answers the
//! resident size in one call, but a *share of a core* is two samples of accumulated `KernelTime`/
//! `UserTime` against a wall clock, and a thread count is `NtQueryInformationProcess` or a toolhelp
//! snapshot. `sysinfo` wraps all three, is already compiled for this target (GPUI pulls it in), and — the
//! part that matters for a document like this — gives the macOS build the same convention, so the number
//! beside `MiB` means the same thing on both machines.
//!
//! ## Why it is not in `note-ui`
//!
//! Because `sysinfo` declares `[target.…]` dependencies, and a crate on the view's own dependency edges
//! must not (`doc/MACOS.md` §6). The view is handed [`Vitals`] — three numbers — and never learns which
//! API produced them.

use note_ui::platform::SystemStats;
use note_ui::platform::Vitals;
use sysinfo::{
    get_current_pid, CpuRefreshKind, MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate,
    RefreshKind, System,
};

/// The process's own counters, kept between calls.
///
/// `System` is held rather than made per call because the processor share is a *difference* against the
/// previous reading: a fresh `System` has no previous sample, so every reading would be the first and the
/// CPU column would read zero forever. Holding it also keeps the process table, which is what makes a
/// refresh of one PID cheap.
pub struct ProcessStats {
    system: System,
    pid: Pid,
    /// The last reading, so a refresh the system refuses answers with the one before it rather than
    /// nothing: a figure a second old is worth more in a line rebuilt every second than a clause that
    /// blinks.
    last: Option<Vitals>,
}

impl ProcessStats {
    /// Where the reading starts, or `None` when the system will not name this process.
    ///
    /// `Option` rather than a `Result`: a build that cannot number its own process is a status line
    /// without a hardware clause, and there is nothing a person could do about it.
    pub fn install() -> Option<Self> {
        let pid = get_current_pid().ok()?;

        let system = System::new_with_specifics(
            RefreshKind::new()
                .with_memory(MemoryRefreshKind::everything())
                .with_cpu(CpuRefreshKind::new().with_cpu_usage())
                .with_processes(ProcessRefreshKind::new().with_cpu().with_memory()),
        );

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
        self.system
            .refresh_processes(ProcessesToUpdate::Some(&[self.pid]));

        let Some(process) = self.system.process(self.pid) else {
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

/// How many threads this process has, from a toolhelp snapshot.
///
/// A second call because `sysinfo` cannot answer this one: its `Process::tasks` is Linux-only and returns
/// `None` here. A toolhelp snapshot of one process's threads is the documented way to ask without
/// reaching for `NtQueryInformationProcess`, which is not part of the documented API surface the rest of
/// this app stays inside.
///
/// `0` for a process the system will not describe or a snapshot it will not take, which
/// [`Vitals::summary`] treats as "not worth saying" and leaves out — the same answer as having no counter
/// at all, and a strictly better one than printing a wrong figure.
fn thread_count(pid: u32) -> usize {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };

    // SAFETY: the snapshot handle is closed on every path below, the structs are the ones the API
    // documents, and each call's result is checked before the next is made. The walk stops when the API
    // says there are no more entries rather than after a fixed count.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
            return 0;
        };

        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };

        // A failure here is an empty or refused snapshot, and the handle is the one thing that must not
        // leak: every early return below closes it.
        if Thread32First(snapshot, &mut entry).is_err() {
            let _ = CloseHandle(snapshot);
            return 0;
        }

        let mut count = 0usize;
        loop {
            if entry.th32OwnerProcessID == pid {
                count += 1;
            }

            if Thread32Next(snapshot, &mut entry).is_err() {
                break;
            }
        }

        let _ = CloseHandle(snapshot);
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counters describe *this* process, and the numbers are of the sizes they claim to be.
    ///
    /// Loose on purpose — this is a live machine and a test that pinned a megabyte count would fail on
    /// the next build. What is checked is that the reading is plausible: a running tester holds memory,
    /// has at least one thread, and is not using a negative share of a core.
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

    /// A second reading still answers, which is what makes the processor figure mean anything.
    #[test]
    fn a_second_reading_still_answers() {
        let mut stats = ProcessStats::install().expect("the process's own counters");

        stats.vitals().expect("the first reading");
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let second = stats.vitals().expect("the second reading");

        assert!(second.resident_bytes > 0);
        assert!(second.threads > 0);
    }
}
