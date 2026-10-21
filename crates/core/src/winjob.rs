//! Windows Job Object helper shared by the shell tool and the MCP stdio
//! transport: arm a fresh job around a just-spawned child, then terminate
//! the whole tree on demand.
//!
//! std has no process groups on Windows, and both callers spawn through
//! `cmd /C` (the shell tool's command line; npm's .cmd shims for MCP
//! servers) — so the process worth killing is a GRANDCHILD the direct
//! Child handle cannot reach. A Job Object is the tree-kill mechanism:
//! membership propagates to every descendant created after the assign
//! (nested jobs — Windows 8+ — compose with a host that already runs us
//! in a job, e.g. a CI runner step), and TerminateJobObject flattens the
//! tree. Extracted from tools/shell.rs (its P1 round) so the MCP transport
//! shares one implementation instead of a copy.

use std::os::windows::io::AsRawHandle;
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, FALSE, HANDLE,
};
use windows_sys::Win32::System::JobObjects as jo;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// What [`WinJob`]'s Drop does with a job that may still hold live members
/// — the two teardown philosophies of the two callers, made explicit
/// instead of implicit in per-caller Drop impls.
#[derive(Clone, Copy)]
pub(crate) enum JobDrop {
    /// Close the job only when it is EMPTY. A survivor — a background
    /// process the user's command deliberately left running, the shell
    /// tool's documented escape hatch (`start /B server >log 2>&1`) —
    /// keeps the job, and the host-death kill KILL_ON_JOB_CLOSE arms, by
    /// LEAKING our handle until this process exits. The leak is bounded
    /// by what the caller chose to leave running.
    LeakIfAlive,
    /// Terminate the whole job, then close. The owner is an exclusive,
    /// long-lived child (an MCP server) whose teardown must reclaim the
    /// tree deterministically — no escape hatch, no survivors.
    Terminate,
}

/// Owned Job Object handle wrapping one spawned child plus every
/// descendant it goes on to create. [`WinJob::assign`] returning `None`
/// (setup failed) means the caller degrades to the old
/// kill-the-direct-child behavior rather than failing a spawn that
/// already succeeded.
pub(crate) struct WinJob {
    handle: Option<HANDLE>,
    drop_policy: JobDrop,
}

impl WinJob {
    /// Arm a fresh Job Object for a just-spawned child: KILL_ON_JOB_CLOSE,
    /// then assign. Called immediately after spawn returns. The one
    /// theoretical escape — cmd spawning the grandchild in the gap before
    /// the assign lands (membership propagates only to processes created
    /// AFTER their parent joined) — loses by orders of magnitude: cmd
    /// needs milliseconds to parse its command line before it can create
    /// anything, while this assign is the next statement after
    /// CreateProcessW returned. The airtight alternative
    /// (CREATE_SUSPENDED, assign, resume) means hand-rolling
    /// CreateProcessW around std::Command — not worth it for a race this
    /// lopsided. Unnamed job: a fresh object per call, so concurrent
    /// spawns never share a kill domain. Any setup failure returns None
    /// and the caller degrades to the old direct-child kill.
    pub(crate) fn assign(child: &std::process::Child, drop_policy: JobDrop) -> Option<Self> {
        unsafe {
            let job = jo::CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job == 0 {
                return None;
            }
            // Zeroed limits + the single flag we want: no CPU/memory caps —
            // the job exists purely so TerminateJobObject can reach the whole
            // tree, and so the tree dies with the host if the host itself
            // crashes.
            let mut info: jo::JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = jo::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = jo::SetInformationJobObject(
                job,
                jo::JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<jo::JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if configured == 0 {
                CloseHandle(job);
                return None;
            }
            // CreateProcessW hands the parent a full-access handle to the
            // child, which covers AssignProcessToJobObject's
            // PROCESS_SET_QUOTA / PROCESS_TERMINATE requirements.
            let assigned = jo::AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE);
            if assigned == 0 {
                CloseHandle(job);
                return None;
            }
            Some(Self {
                handle: Some(job),
                drop_policy,
            })
        }
    }

    /// Terminate every process in the job. Exit code 1 = EXIT_FAILURE —
    /// the conventional "killed" marker, standing in for unix's SIGKILL;
    /// there is no signal story here. The job outlives the direct child
    /// exactly like a unix group survives reparenting, so this works
    /// post-exit too (reclaiming leftover pipe-holders). The return is
    /// ignored like unix ignores ESRCH: a valid job handle always
    /// terminates, and "already all dead" is harmless.
    pub(crate) fn terminate(&self) {
        if let Some(job) = self.handle {
            unsafe { jo::TerminateJobObject(job, 1) };
        }
    }

    /// Duplicate the job handle into a second owned [`WinJob`] pointing at
    /// the SAME job object, under [`JobDrop::LeakIfAlive`]. The duplicate
    /// exists so another thread (the MCP shutdown-reaper) can hold its own
    /// "killable handle": `TerminateJobObject` through ANY handle to the
    /// job flattens the same tree, so the reaper can kill the whole tree
    /// the moment the shutdown flag fires — even while the owning transport
    /// is mid-teardown. The ORIGINAL keeps the Terminate-on-drop policy and
    /// with it the role of deterministic reclaimer of record; the duplicate
    /// must never re-terminate (or close-kill) the tree on its own Drop —
    /// LeakIfAlive closes only an already-EMPTY job (and leaks otherwise,
    /// bounded by process exit), so dropping the copy is always a no-op
    /// with respect to the tree. `None` (DuplicateHandle refused — e.g.
    /// handle-count exhaustion) degrades to the caller's no-reaper path.
    pub(crate) fn duplicate(&self) -> Option<Self> {
        let source = self.handle?;
        // Same-process duplicate (current → current process) with
        // DUPLICATE_SAME_ACCESS: the copy inherits the original's full
        // access mask, which covers TerminateJobObject's JOB_OBJECT_TERMINATE
        // requirement. dwDesiredAccess is ignored under DUPLICATE_SAME_ACCESS
        // (0 for clarity), inheritHandle FALSE — a job handle is never
        // inherited by children.
        let mut dup: HANDLE = 0;
        let duplicated = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                source,
                GetCurrentProcess(),
                &mut dup,
                0,
                FALSE,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if duplicated == 0 || dup == 0 {
            return None;
        }
        Some(Self {
            handle: Some(dup),
            drop_policy: JobDrop::LeakIfAlive,
        })
    }

    /// Test-side probe over [`job_has_live_members`] — production reads it
    /// through Drop only, so the method itself is cfg(test) to keep
    /// non-test builds dead-code-clean.
    #[cfg(test)]
    fn has_live_members(&self) -> bool {
        match self.handle {
            Some(job) => job_has_live_members(job),
            None => true,
        }
    }
}

impl Drop for WinJob {
    /// End-of-ownership teardown per the caller-chosen policy.
    /// [`JobDrop::Terminate`] flattens the tree, then closes. With
    /// KILL_ON_JOB_CLOSE armed, closing the LAST handle is itself a tree
    /// kill — that is the host-death story: if this process dies, the OS
    /// closes its handles and the whole command tree dies with it (unix
    /// has no equivalent; its orphans reparent to init and outlive the
    /// host). [`JobDrop::LeakIfAlive`] closes only an EMPTY job — closing
    /// with a live member would kill it, cutting down the documented
    /// escape hatch (`start /B server >log 2>&1`: output redirected, pipes
    /// released, EOF arrived naturally; unix lets that server outlive the
    /// call, so the Windows job must not kill it either), so with a
    /// survivor the handle is deliberately LEAKED and the survivor keeps
    /// its job — and the host-death kill — armed until this process exits.
    /// A probe that cannot answer conservatively leaks rather than risk
    /// killing a survivor.
    fn drop(&mut self) {
        let Some(job) = self.handle.take() else {
            return;
        };
        match self.drop_policy {
            JobDrop::Terminate => unsafe {
                jo::TerminateJobObject(job, 1);
                CloseHandle(job);
            },
            JobDrop::LeakIfAlive => {
                if !job_has_live_members(job) {
                    unsafe { CloseHandle(job) };
                }
            }
        }
    }
}

/// True when at least one process still lives in the job. The probe reads
/// JobObjectBasicProcessIdList into a 256-slot buffer; the two u32 header
/// fields are picked out by hand (offsets 0 and 4 per the fixed Win32
/// layout) instead of casting the 1-aligned u8 vec to the struct. Either
/// count > 0 — or a query failure (a tree too big for the buffer reports
/// ERROR_MORE_DATA) — answers "alive": callers spend that answer on
/// close-vs-leak, where a wrong "alive" leaks one handle but a wrong
/// "dead" would kill a survivor. Terminated members drop out of this
/// list, so the just-reaped direct child cannot keep its own job "alive"
/// here.
fn job_has_live_members(job: HANDLE) -> bool {
    let mut buf = vec![0u8; 8 + 256 * std::mem::size_of::<usize>()];
    let queried = unsafe {
        jo::QueryInformationJobObject(
            job,
            jo::JobObjectBasicProcessIdList,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            std::ptr::null_mut(),
        )
    };
    if queried == 0 {
        return true;
    }
    let assigned = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let in_list = u32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]);
    assigned > 0 || in_list > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Run-unique marker file path per test (tests run in parallel; the
    /// marker both proves the grandchild started and fingerprints it in
    /// the process table).
    fn marker_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ta-winjob-{tag}-{}.marker", std::process::id()))
    }

    /// Spawn `cmd /C powershell …` — the exact spawn shape BOTH production
    /// callers produce — whose grandchild writes the marker file the
    /// moment it starts (proof it is up), then sleeps far past any test
    /// window. No double quotes around the -Command payload: std escapes
    /// them as \" when building cmd's command line, cmd passes that
    /// through verbatim, and powershell then parses the payload as a
    /// QUOTED STRING LITERAL — echoing it and exiting 0 instantly instead
    /// of running it. Quote-free, -Command simply swallows the rest of the
    /// line (single quotes around the path survive cmd untouched) and
    /// executes it. CREATE_NO_WINDOW for the same no-conhost-flash reason
    /// as production; stdout stays piped (the production shape) — the
    /// command emits nothing, so the unread pipe can never fill.
    fn spawn_marker_grandchild(marker: &str) -> std::process::Child {
        use std::os::windows::process::CommandExt;
        let command = format!(
            "powershell -NoProfile -Command Set-Content -Path '{marker}' -Value up; \
             Start-Sleep -Seconds 120"
        );
        std::process::Command::new("cmd")
            .args(["/C", &command])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .creation_flags(0x0800_0000)
            .spawn()
            .expect("spawn cmd /C powershell")
    }

    /// Poll for the grandchild's proof-of-life up to a generous budget
    /// (powershell cold-start can take seconds); false on timeout so the
    /// caller can fail with a reason instead of passing vacuously.
    fn wait_for_marker(marker: &std::path::Path) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if marker.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Live powershells whose command line references `marker` — a
    /// fingerprint only this test's grandchild can carry (image-name-only
    /// filters would false-positive on the machine's unrelated
    /// powershells). The querying powershell itself carries the marker in
    /// ITS command line, hence the $PID exclusion. CIM via powershell, not
    /// WMIC: WMIC is being removed from newer Windows and CI images while
    /// Windows PowerShell 5.1 remains a stable floor. A failed query
    /// reports usize::MAX so the assertion fails loudly. Same pattern as
    /// the shell tool's grandchild test.
    fn count_live_marker_powershells(marker: &str) -> usize {
        let script = format!(
            "@(Get-CimInstance Win32_Process -Filter \"Name = 'powershell.exe'\" | \
             Where-Object {{ $_.CommandLine -like '*{marker}*' -and $_.ProcessId -ne $PID }}).Count"
        );
        match std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()
        {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(usize::MAX),
            Err(_) => usize::MAX,
        }
    }

    /// Stop this test's leftover sleeper(s) by pid — needed after a
    /// LeakIfAlive drop: the leaked handle means the job never fires
    /// again, so the suite itself must reclaim the survivor (the cmd
    /// wrapper exits on its own once its powershell child dies).
    fn stop_live_marker_powershells(marker: &str) {
        let script = format!(
            "Get-CimInstance Win32_Process -Filter \"Name = 'powershell.exe'\" | \
             Where-Object {{ $_.CommandLine -like '*{marker}*' -and $_.ProcessId -ne $PID }} | \
             ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}"
        );
        let _ = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output();
    }

    #[test]
    fn terminate_flattens_the_job_killing_the_grandchild_cmd_wrapped_command() {
        // The helper's whole point: cmd /C makes the real command a
        // GRANDCHILD, and a plain child.kill() stops cmd and nothing else.
        // terminate() must flatten the job — after it, no powershell whose
        // command line carries this run-unique marker may remain.
        let marker = marker_path("terminate");
        let marker_str = marker.to_string_lossy().into_owned();
        let mut child = spawn_marker_grandchild(&marker_str);
        let job = WinJob::assign(&child, JobDrop::LeakIfAlive).expect("assign job");
        assert!(
            wait_for_marker(&marker),
            "grandchild never started (marker absent) — the kill test would pass vacuously"
        );
        job.terminate();
        let _ = child.wait(); // reap the direct child (also a job member)
                              // Let the termination settle into the process table before probing
                              // both the member list and the live-process count.
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            !job.has_live_members(),
            "a terminated job must report no live members"
        );
        drop(job);
        assert_eq!(
            count_live_marker_powershells(&marker_str),
            0,
            "grandchild powershell survived TerminateJobObject (tree not killed?)"
        );
        std::fs::remove_file(&marker).ok();
    }

    #[test]
    fn drop_under_terminate_policy_reclaims_the_whole_tree_without_an_explicit_terminate() {
        // The MCP transport's drop contract: StdioTransport's Drop kills
        // via kill_child(), but the job handle's OWN Drop (policy
        // Terminate) is the deterministic backstop — even a path that
        // forgot the explicit terminate must lose nothing. Here: no
        // terminate() call at all, just drop.
        let marker = marker_path("drop-terminate");
        let marker_str = marker.to_string_lossy().into_owned();
        let mut child = spawn_marker_grandchild(&marker_str);
        let job = WinJob::assign(&child, JobDrop::Terminate).expect("assign job");
        assert!(
            wait_for_marker(&marker),
            "grandchild never started (marker absent)"
        );
        drop(job);
        let _ = child.kill(); // died with the job; reap it
        let _ = child.wait();
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(
            count_live_marker_powershells(&marker_str),
            0,
            "grandchild survived a Terminate-policy drop (job handle closed without killing?)"
        );
        std::fs::remove_file(&marker).ok();
    }

    #[test]
    fn drop_under_leak_if_alive_policy_leaves_a_survivor_running_until_host_exit() {
        // The shell tool's drop contract: closing the job would kill the
        // survivor (KILL_ON_JOB_CLOSE fires on the LAST close), cutting
        // down the documented escape hatch — so the handle deliberately
        // LEAKS and the survivor keeps running until this process exits.
        // After the drop, the grandchild must STILL be alive; then it is
        // stopped by hand so the suite doesn't carry a 120s sleeper.
        let marker = marker_path("leak-if-alive");
        let marker_str = marker.to_string_lossy().into_owned();
        let mut child = spawn_marker_grandchild(&marker_str);
        let job = WinJob::assign(&child, JobDrop::LeakIfAlive).expect("assign job");
        assert!(
            wait_for_marker(&marker),
            "grandchild never started (marker absent)"
        );
        drop(job); // live members → the handle leaks, NO kill fires
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            count_live_marker_powershells(&marker_str) >= 1,
            "survivor must outlive a LeakIfAlive drop (job handle closed anyway?)"
        );
        // Hand cleanup: the leaked job never fires again by design.
        stop_live_marker_powershells(&marker_str);
        let _ = child.kill();
        let _ = child.wait();
        std::fs::remove_file(&marker).ok();
    }
}
