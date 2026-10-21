//! Explicit Python discovery shared by portable process fixtures.
use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
};
pub fn python() -> PathBuf {
    let configured = std::env::var_os("DISCORA_TEST_PYTHON");
    configured
        .into_iter()
        .chain([OsString::from("python3"), OsString::from("python")])
        .map(PathBuf::from)
        .find(|path| {
            Command::new(path)
                .args(["-c", "import sys; sys.exit(sys.version_info.major != 3)"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
        .expect("Tests need Python 3 on PATH, or DISCORA_TEST_PYTHON set to its executable")
}
pub fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::*};
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut code = 0;
        let alive = GetExitCodeProcess(process, &mut code) != 0 && code == 259;
        CloseHandle(process);
        alive
    }
}
