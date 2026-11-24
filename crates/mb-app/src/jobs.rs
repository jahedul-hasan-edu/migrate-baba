//! Background job processes: a data migration runs in a detached copy of this executable
//! (`--run-job <id>`), so it survives the window being closed. The worker holds an OS file lock
//! for its whole life; the OS releases it if the process dies, which is how the app tells a live
//! run from one interrupted by a crash or shutdown.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::process::{Command, Stdio};

pub const ARG: &str = "--run-job";

fn lock_file(dir: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join("lock"))
}

/// Take the job's exclusive lock (worker side). `None` if another worker holds it.
pub fn acquire(dir: &Path) -> Option<File> {
    let f = lock_file(dir).ok()?;
    match f.try_lock() {
        Ok(()) => Some(f),
        Err(_) => None,
    }
}

/// Whether a worker process currently owns the job.
pub fn alive(dir: &Path) -> bool {
    let Ok(f) = lock_file(dir) else { return false };
    match f.try_lock() {
        Ok(()) => {
            let _ = f.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    }
}

/// Ask the worker to pause or cancel at the next safe point.
pub fn request(dir: &Path, what: &str) -> std::io::Result<()> {
    std::fs::write(dir.join("control"), what)
}

/// Read and clear a pending request (worker side).
pub fn take_request(dir: &Path) -> Option<String> {
    let p = dir.join("control");
    let s = std::fs::read_to_string(&p).ok()?;
    let _ = std::fs::remove_file(&p);
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Start the worker for `job_id`, detached from this process.
pub fn spawn(job_id: &str) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let make = || {
        let mut c = Command::new(&exe);
        c.arg(ARG).arg(job_id).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        c
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        // Break away from a parent job object (e.g. a terminal) when allowed, so closing it
        // doesn't kill the migration.
        if make().creation_flags(base | CREATE_BREAKAWAY_FROM_JOB).spawn().is_ok() {
            return Ok(());
        }
        make().creation_flags(base).spawn().map(|_| ())
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        make().process_group(0).spawn().map(|_| ())
    }
    #[cfg(not(any(windows, unix)))]
    {
        make().spawn().map(|_| ())
    }
}

/// Keep the computer awake while a migration runs (Windows). Reset automatically when the process exits.
pub fn keep_awake() {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetThreadExecutionState(flags: u32) -> u32;
        }
        const ES_CONTINUOUS: u32 = 0x8000_0000;
        const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
        // SAFETY: plain Win32 call with constant flags.
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
        }
    }
}
