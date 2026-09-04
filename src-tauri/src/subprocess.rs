//! Bounded supervision for short-lived database metadata/helper commands.
//! SQL backup/restore streams use the separate streaming pipeline, not these limits.
use anyhow::{anyhow, Context, Result};
use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub timeout: Duration,
    pub output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            output_bytes: 4 * 1024 * 1024,
        }
    }
}

struct OwnedChild {
    child: Child,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl OwnedChild {
    fn spawn(command: &mut Command) -> Result<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
            // Assignment must precede execution: otherwise a fast helper could
            // create a pipe-holding descendant before it enters our job.
            command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command.spawn().context("Could not start database helper")?;
        let mut owned = Self {
            child,
            #[cfg(windows)]
            job: std::ptr::null_mut(),
        };
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::*;
            // A private, unnamed job owns only this helper and its descendants.
            // Closing it also closes inherited pipe writers before readers join.
            unsafe {
                owned.job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if owned.job.is_null() {
                    return Err(std::io::Error::last_os_error())
                        .context("Could not supervise database helper");
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    owned.job,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&info) as u32,
                ) == 0
                    || AssignProcessToJobObject(owned.job, owned.child.as_raw_handle()) == 0
                {
                    return Err(std::io::Error::last_os_error())
                        .context("Could not supervise database helper");
                }
            }
            resume_owned_child(&owned.child)?;
        }
        Ok(owned)
    }
}

#[cfg(windows)]
fn resume_owned_child(child: &Child) -> Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    use windows_sys::Win32::System::Threading::*;
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    // std::process keeps the process handle, not the initial thread handle.
    // The child is suspended, so it cannot create more threads or exit itself.
    // Find and validate only its initial thread, never resume unrelated threads.
    unsafe {
        let raw = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if raw == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error())
                .context("Could not locate database helper thread");
        }
        let snapshot = Handle(raw);
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut present = Thread32First(snapshot.0, &mut entry);
        while present != 0 {
            if entry.th32OwnerProcessID == child.id() {
                let raw = OpenThread(
                    THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                    0,
                    entry.th32ThreadID,
                );
                if raw.is_null() {
                    return Err(std::io::Error::last_os_error())
                        .context("Could not open database helper thread");
                }
                let thread = Handle(raw);
                if GetProcessIdOfThread(thread.0) != child.id() {
                    return Err(anyhow!("Database helper thread ownership changed"));
                }
                if ResumeThread(thread.0) != 1 {
                    return Err(anyhow!("Could not resume the suspended database helper"));
                }
                return Ok(());
            }
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            present = Thread32Next(snapshot.0, &mut entry);
        }
    }
    Err(anyhow!("Suspended database helper thread was not found"))
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            if !self.job.is_null() {
                windows_sys::Win32::Foundation::CloseHandle(self.job);
            }
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn collect_bounded(
    stream: impl Read,
    limit: usize,
    exceeded: &AtomicBool,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    stream
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut output)?;
    if output.len() > limit {
        exceeded.store(true, Ordering::Release);
        output.truncate(limit);
    }
    Ok(output)
}

pub(crate) fn run(
    mut command: Command,
    limits: Limits,
    cancelled: &dyn Fn() -> bool,
) -> Result<Output> {
    if cancelled() {
        return Err(crate::backup_operations::cancelled_error());
    }
    let started = Instant::now();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut owned = OwnedChild::spawn(&mut command)?;
    let stdout = owned
        .child
        .stdout
        .take()
        .context("Database helper stdout unavailable")?;
    let stderr = owned
        .child
        .stderr
        .take()
        .context("Database helper stderr unavailable")?;
    let exceeded = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&exceeded);
    let out = std::thread::spawn(move || collect_bounded(stdout, limits.output_bytes, &flag));
    let flag = Arc::clone(&exceeded);
    let err = std::thread::spawn(move || collect_bounded(stderr, limits.output_bytes, &flag));
    let result = loop {
        if cancelled() {
            break Err(crate::backup_operations::cancelled_error());
        }
        if exceeded.load(Ordering::Acquire) {
            break Err(anyhow!(
                "DATABASE_TOOL_OUTPUT_LIMIT: Helper output exceeded the safe limit"
            ));
        }
        if started.elapsed() >= limits.timeout {
            break Err(anyhow!("DATABASE_TOOL_TIMEOUT: Database helper timed out"));
        }
        match owned.child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => break Err(error).context("Could not poll database helper"),
        }
    };
    // Always terminate/reap before joining readers, even on polling failures or
    // a parent that exits successfully while a descendant holds its pipes open.
    drop(owned);
    let stdout = out
        .join()
        .map_err(|_| anyhow!("Database output reader failed"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow!("Database error reader failed"))??;
    let status = result?;
    if exceeded.load(Ordering::Acquire) {
        return Err(anyhow!(
            "DATABASE_TOOL_OUTPUT_LIMIT: Helper output exceeded the safe limit"
        ));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "subprocess::tests::helper_fixture",
            "--nocapture",
        ]);
        command.env("SAVESTATE_TEST_HELPER", mode);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        command
    }

    #[test]
    fn helper_fixture() {
        use std::io::Write;
        let Ok(mode) = std::env::var("SAVESTATE_TEST_HELPER") else {
            return;
        };
        match mode.as_str() {
            "sleep" => std::thread::sleep(Duration::from_secs(20)),
            "descendant" | "descendant-sleep" => {
                let _child = fixture("sleep").spawn().unwrap();
                println!("descendant spawned");
                if mode == "descendant-sleep" {
                    std::thread::sleep(Duration::from_secs(20));
                }
            }
            "flood" => {
                let block = [b'x'; 8192];
                for _ in 0..256 {
                    if std::io::stdout().write_all(&block).is_err() {
                        break;
                    }
                }
            }
            "fail" => {
                eprintln!("fixture rejected");
                std::process::exit(7);
            }
            _ => println!("fixture completed"),
        }
    }

    #[test]
    fn captures_success_and_nonzero_exit_without_hiding_failure() {
        let result = run(fixture("ok"), Limits::default(), &|| false).unwrap();
        assert!(result.status.success());
        assert!(String::from_utf8_lossy(&result.stdout).contains("fixture completed"));
        let result = run(fixture("fail"), Limits::default(), &|| false).unwrap();
        assert_eq!(result.status.code(), Some(7));
        assert!(String::from_utf8_lossy(&result.stderr).contains("fixture rejected"));
    }

    #[test]
    fn times_out_and_reaps_helper() {
        let start = Instant::now();
        let error = run(
            fixture("sleep"),
            Limits {
                timeout: Duration::from_millis(150),
                ..Limits::default()
            },
            &|| false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("DATABASE_TOOL_TIMEOUT"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn cancellation_stops_running_helper_and_prevents_new_spawn() {
        let start = Instant::now();
        let error = run(fixture("sleep"), Limits::default(), &|| {
            start.elapsed() >= Duration::from_millis(150)
        })
        .unwrap_err();
        assert!(crate::backup_operations::is_cancelled(&error));
        assert!(start.elapsed() < Duration::from_secs(5));
        let error = run(Command::new("does-not-exist"), Limits::default(), &|| true).unwrap_err();
        assert!(crate::backup_operations::is_cancelled(&error));
    }

    #[test]
    fn output_limit_fails_closed() {
        let error = run(
            fixture("flood"),
            Limits {
                output_bytes: 4096,
                ..Limits::default()
            },
            &|| false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("DATABASE_TOOL_OUTPUT_LIMIT"));
    }

    #[test]
    fn successful_parent_cannot_leave_pipe_holding_descendants() {
        let start = Instant::now();
        let result = run(fixture("descendant"), Limits::default(), &|| false).unwrap();
        assert!(result.status.success());
        assert!(String::from_utf8_lossy(&result.stdout).contains("descendant spawned"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn timeout_cleans_up_an_immediately_spawned_descendant() {
        let start = Instant::now();
        let error = run(
            fixture("descendant-sleep"),
            Limits {
                timeout: Duration::from_millis(200),
                ..Limits::default()
            },
            &|| false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("DATABASE_TOOL_TIMEOUT"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
