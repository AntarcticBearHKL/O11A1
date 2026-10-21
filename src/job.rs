//! Process-lifetime binding through a single Windows Job Object.
//!
//! The app spawns `ffmpeg.exe` / `ffplay.exe` children for playback and for
//! 离线下载. Those children are reaped on a *graceful* shutdown, but an abrupt
//! death (crash, panic, Task Manager "End task", `Stop-Process -Force`) runs no
//! Rust cleanup and the children would keep running with no UI.
//!
//! One job is created at startup and configured with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: when the last handle to the job closes
//! the OS terminates every process assigned to it. The kernel closes the handle
//! when this process exits *for any reason*, including a force-kill, so the
//! children die with the app at the OS level.
//!
//! Everything here is best-effort and panic-free: a failure to create the job,
//! or to assign a process, is logged and playback continues unaffected. Nested
//! jobs (Windows 8+) mean a child that is already in a job can still be assigned;
//! when a debugger/CI already placed *us* in a non-nestable job the assign can
//! fail with `ERROR_ACCESS_DENIED`, which is logged and ignored.

use std::os::windows::io::AsRawHandle;
use std::process::Child;
use std::sync::OnceLock;

/// A Win32 `HANDLE` as an integer; `0` means "no handle" / failure.
type Handle = isize;

#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(
        attributes: *const std::os::raw::c_void,
        name: *const u16,
    ) -> Handle;

    fn SetInformationJobObject(
        job: Handle,
        info_class: i32,
        info: *const std::os::raw::c_void,
        info_len: u32,
    ) -> i32;

    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
}

/// `JobObjectExtendedLimitInformation` (winnt.h).
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
/// Terminate every process in the job once its last handle closes (winnt.h).
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
/// `ERROR_ACCESS_DENIED` (winerror.h).
const ERROR_ACCESS_DENIED: i32 = 5;

/// `JOBOBJECT_BASIC_LIMIT_INFORMATION` (winnt.h).
#[repr(C)]
#[allow(dead_code)]
struct JobObjectBasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

/// `IO_COUNTERS` (winnt.h).
#[repr(C)]
#[allow(dead_code)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

/// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` (winnt.h).
#[repr(C)]
#[allow(dead_code)]
struct JobObjectExtendedLimitInformation {
    basic_limit_information: JobObjectBasicLimitInformation,
    io_info: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

/// The one job for the whole process lifetime. Deliberately never closed: the
/// handle must outlive every child, and the kernel closes it on exit (which is
/// what fires `KILL_ON_JOB_CLOSE`).
static JOB: OnceLock<Handle> = OnceLock::new();

/// Create the job on first use (idempotent, thread-safe). Returns the handle, or
/// `None` if creation failed.
fn ensure_job() -> Option<Handle> {
    let handle = *JOB.get_or_init(create_job);
    if handle != 0 {
        Some(handle)
    } else {
        None
    }
}

/// Build the job with `KILL_ON_JOB_CLOSE`; `0` on failure (logged).
fn create_job() -> Handle {
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job == 0 {
        crate::debug_log(&format!(
            "job: CreateJobObjectW failed err={}",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        ));
        return 0;
    }

    let mut info: JobObjectExtendedLimitInformation = unsafe { std::mem::zeroed() };
    info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
            (&info as *const JobObjectExtendedLimitInformation).cast(),
            std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
        )
    };
    if ok == 0 {
        crate::debug_log(&format!(
            "job: SetInformationJobObject(KILL_ON_JOB_CLOSE) failed err={}",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        ));
    } else {
        crate::debug_log("job: created with KILL_ON_JOB_CLOSE");
    }
    job
}

/// Create the job at startup. Safe to call more than once; later calls are no-ops.
pub fn init() {
    let _ = ensure_job();
}

/// Bind `child` to the app's job so the OS kills it when this process dies.
///
/// Best-effort: if the job is unavailable, or the child is already in a
/// non-nestable job (`ERROR_ACCESS_DENIED`), the failure is logged and playback
/// continues. Returning early here never aborts the caller's spawn path.
pub fn assign(child: &Child) {
    let Some(job) = ensure_job() else {
        return;
    };
    let process = child.as_raw_handle() as Handle;
    if process == 0 {
        return;
    }
    let ok = unsafe { AssignProcessToJobObject(job, process) };
    if ok != 0 {
        return;
    }
    let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    if code == ERROR_ACCESS_DENIED {
        crate::debug_log(
            "job: AssignProcessToJobObject ERROR_ACCESS_DENIED (process already in a \
             non-nestable job); continuing",
        );
    } else {
        crate::debug_log(&format!(
            "job: AssignProcessToJobObject failed err={code}"
        ));
    }
}
