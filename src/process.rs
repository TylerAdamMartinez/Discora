//! Own an entire child tree, including helpers that inherit output pipes.
use std::process::{Child, Command};
#[cfg(unix)]
use std::time::Instant;
use std::{io, thread, time::Duration};

pub struct ChildGuard {
    pub child: Child,
    pub stopped: bool,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl ChildGuard {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            Ok(Self {
                child: command.process_group(0).spawn()?,
                stopped: false,
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::{
                io::{AsRawHandle, FromRawHandle, OwnedHandle},
                process::CommandExt,
            };
            use windows_sys::Win32::{
                Foundation::CloseHandle,
                System::{Diagnostics::ToolHelp::*, JobObjects::*, Threading::*},
            };
            // The child starts suspended: no helper can escape before assignment.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let job = OwnedHandle::from_raw_handle(handle);
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as _,
                    std::mem::size_of_val(&limits) as u32,
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                let child = command
                    .creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW)
                    .spawn()?;
                let mut guard = Self {
                    child,
                    stopped: false,
                    job,
                };
                let setup = (|| {
                    if AssignProcessToJobObject(handle, guard.child.as_raw_handle()) == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
                    if snapshot == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                        return Err(io::Error::last_os_error());
                    }
                    let snapshot = OwnedHandle::from_raw_handle(snapshot);
                    let mut entry: THREADENTRY32 = std::mem::zeroed();
                    entry.dwSize = std::mem::size_of_val(&entry) as u32;
                    let mut more = Thread32First(snapshot.as_raw_handle(), &mut entry);
                    while more != 0 {
                        if entry.th32OwnerProcessID == guard.child.id() {
                            let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                            if thread.is_null() {
                                return Err(io::Error::last_os_error());
                            }
                            let resumed = ResumeThread(thread);
                            let error = io::Error::last_os_error();
                            CloseHandle(thread);
                            if resumed == u32::MAX {
                                return Err(error);
                            }
                            return Ok(());
                        }
                        more = Thread32Next(snapshot.as_raw_handle(), &mut entry);
                    }
                    Err(io::Error::other("Cannot find suspended child thread"))
                })();
                if let Err(error) = setup {
                    // Also kill explicitly if assignment itself failed.
                    let _ = guard.child.kill();
                    guard.stop();
                    return Err(error);
                }
                Ok(guard)
            }
        }
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        #[cfg(unix)]
        {
            use nix::{
                sys::signal::{Signal, killpg},
                unistd::Pid,
            };
            let group = Pid::from_raw(self.child.id() as i32);
            let _ = killpg(group, Signal::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if self.child.try_wait().is_ok_and(|s| s.is_some()) {
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
            let _ = killpg(group, Signal::SIGKILL);
        }
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::*;
            TerminateJobObject(self.job.as_raw_handle(), 1);
            // Termination is asynchronous. Wait until helpers have released files.
            loop {
                let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
                if QueryInformationJobObject(
                    self.job.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    &mut info as *mut _ as _,
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                ) == 0
                    || info.ActiveProcesses == 0
                {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = self.child.wait();
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.stop();
    }
}
