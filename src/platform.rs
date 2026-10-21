use crate::Result;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

pub fn executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(windows)]
    {
        path.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    }
}

pub fn find_program(names: &[&str], extra: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    find_in(names, paths.into_iter().chain(extra))
}
fn find_in(names: &[&str], paths: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    paths
        .into_iter()
        .flat_map(|p| names.iter().map(move |n| p.join(n)))
        .find(|p| executable(p))
}

#[cfg(windows)]
pub fn installation_dirs(product: &str) -> Vec<PathBuf> {
    ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|p| PathBuf::from(p).join(product))
        .collect()
}

pub fn makemkv(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if executable(path) {
            return Ok(std::fs::canonicalize(path)?);
        }
        return Err(format!("Invalid --makemkv-path executable: {}", path.display()).into());
    }
    #[cfg(windows)]
    let (names, extra) = (
        &["makemkvcon64.exe", "makemkvcon.exe"][..],
        installation_dirs("MakeMKV"),
    );
    #[cfg(unix)]
    let (names, extra) = (&["makemkvcon"][..], Vec::<PathBuf>::new());
    find_program(names, extra).ok_or_else(|| "MakeMKV console was not found. Install MakeMKV, add it to PATH, or supply --makemkv-path.".into())
}

#[cfg(unix)]
pub struct Signals(Vec<signal_hook::SigId>);
#[cfg(unix)]
impl Signals {
    pub fn register(cancel: Arc<AtomicBool>) -> Result<Self> {
        let mut signals = Self(Vec::new());
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            signals
                .0
                .push(signal_hook::flag::register(signal, cancel.clone())?);
        }
        Ok(signals)
    }
}
#[cfg(unix)]
impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.0.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

#[cfg(windows)]
mod console {
    use super::*;
    use std::sync::{Mutex, atomic::Ordering};
    use windows_sys::Win32::System::Console::*;
    static CANCEL: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);
    unsafe extern "system" fn handler(event: u32) -> i32 {
        if matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT)
            && let Ok(cancel) = CANCEL.lock()
            && let Some(cancel) = cancel.as_ref()
        {
            cancel.store(true, Ordering::Relaxed);
            return 1;
        }
        0
    }
    pub struct Signals;
    impl Signals {
        pub fn register(cancel: Arc<AtomicBool>) -> Result<Self> {
            *CANCEL.lock().map_err(|_| "Console handler lock poisoned")? = Some(cancel);
            if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
                *CANCEL.lock().map_err(|_| "Console handler lock poisoned")? = None;
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self)
        }
    }
    impl Drop for Signals {
        fn drop(&mut self) {
            unsafe {
                SetConsoleCtrlHandler(Some(handler), 0);
            }
            if let Ok(mut cancel) = CANCEL.lock() {
                *cancel = None;
            }
        }
    }
}
#[cfg(windows)]
pub use console::Signals;

/// Honor the documented Windows app-data variables, with Known Folder fallback.
pub fn config_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(path) = app_data("APPDATA") {
        return Some(path);
    }
    dirs::config_dir()
}
#[cfg(windows)]
pub fn app_data(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_preserves_directory_order_and_checks_all_console_names() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("PATH with spaces");
        let install = root.path().join("Program Files");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::create_dir_all(&install).unwrap();
        let first = path.join("makemkvcon.exe");
        let second = install.join("makemkvcon64.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &first).unwrap();
        std::fs::copy(std::env::current_exe().unwrap(), &second).unwrap();
        let names = &["makemkvcon64.exe", "makemkvcon.exe"];
        assert_eq!(
            find_in(names, [path.clone(), install.clone()]).unwrap(),
            first
        );
        std::fs::remove_file(first).unwrap();
        assert_eq!(find_in(names, [path, install]).unwrap(), second);
    }

    #[test]
    fn explicit_executable_overrides_discovery_and_invalid_override_fails() {
        let current = std::env::current_exe().unwrap();
        assert_eq!(makemkv(Some(&current)).unwrap(), current);
        let root = tempfile::tempdir().unwrap();
        assert!(makemkv(Some(root.path())).is_err());
        assert!(makemkv(Some(&root.path().join("missing.exe"))).is_err());
    }
}
