//! Windows SSHFS mount ownership and server-side, no-overwrite publication.
use crate::{Result, makemkv::check_cancel, process::ChildGuard, rip::RunLog, sftp::Client};
#[cfg(windows)]
use crate::{destination::Remote, platform};
#[cfg(windows)]
use std::{ffi::OsString, process::Stdio};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

fn cygwin_path(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or("SSH paths must be valid Unicode")?
        .replace('\\', "/");
    let path = path
        .strip_prefix("//?/UNC/")
        .map(|p| format!("//{p}"))
        .unwrap_or_else(|| path.strip_prefix("//?/").unwrap_or(&path).to_owned());
    if path
        .chars()
        .any(|c| matches!(c, ',' | '"') || c.is_control())
    {
        return Err("SSH paths cannot contain commas, quotes, or control characters".into());
    }
    let path = path.as_str();
    if path.starts_with("//") {
        return Ok(path.to_owned());
    }
    if path.len() >= 3 && path.as_bytes()[1] == b':' && path.as_bytes()[2] == b'/' {
        return Ok(format!(
            "/cygdrive/{}/{}",
            path[..1].to_ascii_lowercase(),
            &path[3..]
        ));
    }
    Err("SSH identity and known_hosts paths must be absolute Windows paths".into())
}
// Cygwin parses a native parent's command line differently from the MSVC CRT.
// Always enclose options in quotes before escaping their embedded SSH-config
// quotes; Rust's default encoding may leave those escaped quotes unwrapped.
fn cygwin_quoted_argument(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('\"', "\\\""))
}

#[cfg(windows)]
fn ssh_options(remote: &Remote) -> Result<Vec<OsString>> {
    let known_hosts = dirs::home_dir()
        .ok_or("Cannot find user home for SSH known_hosts")?
        .join(".ssh/known_hosts");
    if !known_hosts.is_file() {
        return Err(format!(
            "Verify the SSH server host key and add it to {} first",
            known_hosts.display()
        )
        .into());
    }
    let mut options = vec![
        "BatchMode=yes".into(),
        "StrictHostKeyChecking=yes".into(),
        "ConnectTimeout=10".into(),
        "ServerAliveInterval=15".into(),
        "ServerAliveCountMax=2".into(),
        format!("UserKnownHostsFile=\"{}\"", cygwin_path(&known_hosts)?).into(),
    ];
    if let Some(identity) = &remote.identity_file {
        options.push(format!("IdentityFile=\"{}\"", cygwin_path(identity)?).into());
    }
    Ok(options)
}

/// Exercise the same suspended/job-managed launcher and handles as SFTP.
#[cfg(windows)]
fn probe_ssh_launcher(ssh: &Path, cancel: &AtomicBool, log: &RunLog) -> Result<()> {
    use std::io::Read;

    log.write(
        "ssh-probe",
        "Running bundled ssh -V through Discora's suspended, job-managed launcher",
    )?;
    let mut command = Command::new(ssh);
    command
        .arg("-V")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = ChildGuard::spawn(&mut command)?;
    process.log_stderr(log, "ssh-probe-stderr");
    let started = Instant::now();
    loop {
        check_cancel(cancel)?;
        if let Some(status) = process.child.try_wait()? {
            process.stop();
            let code = status
                .code()
                .map(|code| format!("0x{:08X}", code as u32))
                .unwrap_or_else(|| "unavailable".to_owned());
            log.write(
                "ssh-probe",
                &format!(
                    "ssh -V finished: {status}; Windows exit code {code}; elapsed {:?}",
                    started.elapsed()
                ),
            )?;
            let mut stdout = String::new();
            if let Some(mut output) = process.child.stdout.take() {
                output.read_to_string(&mut stdout)?;
            }
            log.write(
                "ssh-probe",
                &format!(
                    "ssh -V stdout: {stdout:?}; version stderr is captured as ssh-probe-stderr"
                ),
            )?;
            return Ok(());
        }
        if started.elapsed() >= Duration::from_secs(5) {
            log.write(
                "ssh-probe",
                "ssh -V did not exit within 5 seconds; stopping probe",
            )?;
            return Err("Bundled SSH launcher probe timed out; see run log".into());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[derive(Clone)]
pub struct Publisher {
    root: PathBuf,
    remote_root: String,
    client: Arc<Mutex<Client>>,
}
impl Publisher {
    fn remote_path(&self, path: &Path) -> Result<String> {
        let relative = path.strip_prefix(&self.root)?;
        let parts = relative
            .components()
            .map(|c| match c {
                Component::Normal(name) => {
                    name.to_str().ok_or("Remote filename must be valid Unicode")
                }
                _ => Err("Remote path must remain inside the mounted library"),
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(format!(
            "{}/{}",
            self.remote_root.trim_end_matches('/'),
            parts.join("/")
        ))
    }
    pub fn exists(&self, path: &Path, cancel: &AtomicBool) -> Result<bool> {
        self.client
            .lock()
            .map_err(|_| "SFTP lock poisoned")?
            .exists(&self.remote_path(path)?, cancel)
    }
    pub fn publish(&self, source: &Path, target: &Path, cancel: &AtomicBool) -> Result<()> {
        let source_remote = self.remote_path(source)?;
        let target_remote = self.remote_path(target)?;
        let mut client = self.client.lock().map_err(|_| "SFTP lock poisoned")?;
        client.verify_output(&source_remote, cancel)?;
        client.sync(&source_remote, cancel)?;
        check_cancel(cancel)?;
        client.hard_link(&source_remote, &target_remote, cancel).map_err(|error| format!(
            "Remote publication failed or its result is uncertain: {error}. Inspect {target_remote}; staging source: {source_remote}. Existing destinations are never replaced."
        ))?;
        // The final link is now authoritative. Removing staging cannot affect it.
        fs::remove_file(source).map_err(|error| format!("Movie saved at {target_remote}, but could not remove staging file {source_remote}: {error}"))?;
        Ok(())
    }
    fn preflight(&self, cancel: &AtomicBool) -> Result<()> {
        let probe = tempfile::Builder::new()
            .prefix(".discora-check-")
            .tempdir_in(&self.root)?;
        let source = probe.path().join("source");
        let target = probe.path().join("existing");
        let linked = probe.path().join("linked");
        let result = (|| -> Result<()> {
            fs::write(&source, b"discora filesystem check")?;
            fs::write(&target, b"existing destination")?;
            let source = self.remote_path(&source)?;
            let target = self.remote_path(&target)?;
            let linked = self.remote_path(&linked)?;
            let mut client = self.client.lock().map_err(|_| "SFTP lock poisoned")?;
            client.sync(&source, cancel)?;
            client.hard_link(&source, &linked, cancel)?;
            if client.hard_link(&source, &target, cancel).is_ok()
                || client.read_small(&target, cancel)? != b"existing destination"
                || client.read_small(&linked, cancel)? != b"discora filesystem check"
            {
                return Err(
                    "Remote filesystem failed no-overwrite publication verification".into(),
                );
            }
            Ok(())
        })();
        let path = probe.path().to_owned();
        let cleanup = probe.close();
        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (result, Err(error)) => Err(format!(
                "Remote preflight: {result:?}; check directory retained at {}: {error}",
                path.display()
            )
            .into()),
        }
    }
}

pub struct Mount {
    root: PathBuf,
    process: ChildGuard,
    pub publisher: Publisher,
    closed: bool,
    log: RunLog,
}
impl Mount {
    pub fn path(&self) -> &Path {
        &self.root
    }
    #[cfg(windows)]
    pub fn connect(remote: &Remote, cancel: &AtomicBool, log: &RunLog) -> Result<Self> {
        remote.validate()?;
        check_cancel(cancel)?;
        let extra = platform::installation_dirs("SSHFS-Win")
            .into_iter()
            .map(|p| p.join("bin"));
        let wrapper = platform::find_program(&["sshfs-win.exe"], extra)
            .ok_or("Install WinFsp and SSHFS-Win before using SSHFS destinations")?;
        let ssh = wrapper
            .parent()
            .ok_or("Invalid SSHFS-Win installation")?
            .join("ssh.exe");
        if !ssh.is_file() {
            return Err("SSHFS-Win installation is missing bin/ssh.exe".into());
        }
        log.write(
            "sshfs",
            &format!(
                "Using SSHFS-Win executable {}; bundled SSH {}",
                wrapper.display(),
                ssh.display()
            ),
        )?;
        if std::env::var_os("DISCORA_SSH_PROBE").as_deref() == Some(std::ffi::OsStr::new("1")) {
            probe_ssh_launcher(&ssh, cancel, log)?;
        }
        let options = ssh_options(remote)?;
        let mut control = Command::new(ssh);
        control.arg("-vvv");
        for option in &options {
            use std::os::windows::process::CommandExt;
            control.arg("-o").raw_arg(cygwin_quoted_argument(
                option.to_str().ok_or("SSH options must be valid Unicode")?,
            ));
        }
        control.args([
            "-p",
            &remote.port.to_string(),
            "-s",
            &format!(
                "{}@{}",
                remote.username,
                remote.host.trim_matches(['[', ']'])
            ),
            "sftp",
        ]);
        let client = Client::connect(&mut control, cancel, log)?;
        // Probe Windows' drive namespace rather than assuming a particular free letter.
        let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
        if mask == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let letter = (b'D'..=b'Z')
            .rev()
            .find(|letter| mask & (1 << (letter - b'A')) == 0)
            .ok_or("No free drive letter for SSHFS")?;
        let root = PathBuf::from(format!("{}:\\", letter as char));
        let mut command = Command::new(wrapper);
        command.args(["cmd", "-f", "-p", &remote.port.to_string()]);
        // sshfs-win.exe is itself Cygwin-linked and forwards argv to sshfs.
        // It needs the same option encoding as the bundled SSH executable.
        for option in &options {
            use std::os::windows::process::CommandExt;
            command.arg("-o").raw_arg(cygwin_quoted_argument(
                option.to_str().ok_or("SSH options must be valid Unicode")?,
            ));
        }
        // Match SSHFS-Win's default service mapping: present remote entries as
        // owned by the mounting Windows user/group. POSIX mode bits alone do
        // not convey named server ACL grants to WinFsp (e.g. a library owned
        // by someone other than the SFTP account). The server still enforces
        // the authenticated account's permissions on every operation.
        command
            .args([
                "-o",
                "ssh_command=/usr/bin/ssh.exe,idmap=user,uid=-1,gid=-1,direct_io,sshfs_sync,cache=no,attr_timeout=0,entry_timeout=0,negative_timeout=0",
            ])
            .arg(format!(
                "{}@{}:{}",
                remote.username, remote.host, remote.movies_path
            ))
            .arg(format!("{}:", letter as char))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        log.write("sshfs", "Mapping mount ownership to the current Windows user/group (uid=-1,gid=-1); server permissions remain authoritative")?;
        log.write("sshfs", &format!("Starting Windows mount: {command:?}"))?;
        let mut process = ChildGuard::spawn(&mut command).map_err(|error| {
            let _ = log.write("sshfs", &format!("Cannot start Windows mount: {error}"));
            error
        })?;
        process.log_stderr(log, "sshfs-stderr");
        log.write(
            "sshfs",
            &format!(
                "Mount process started (PID {}); waiting up to 20 seconds for {}",
                process.child.id(),
                root.display()
            ),
        )?;
        let publisher = Publisher {
            root: root.clone(),
            remote_root: remote.movies_path.clone(),
            client: Arc::new(Mutex::new(client)),
        };
        let mut mount = Self {
            root,
            process,
            publisher,
            closed: false,
            log: log.clone(),
        };
        let result = (|| -> Result<()> {
            let started = Instant::now();
            loop {
                check_cancel(cancel)?;
                if let Some(status) = mount.process.child.try_wait()? {
                    return Err(format!(
                        "SSHFS-Win exited ({status}); see {} for diagnostics",
                        log.path.display()
                    )
                    .into());
                }
                if mount.root.is_dir() {
                    break;
                }
                if started.elapsed() > Duration::from_secs(20) {
                    return Err("Timed out waiting for SSHFS-Win; check WinFsp installation and SSH settings".into());
                }
                thread::sleep(Duration::from_millis(50));
            }
            log.write(
                "sshfs",
                "Mount is visible; checking remote write, sync, and hard-link publication",
            )?;
            mount.publisher.preflight(cancel)?;
            log.write("sshfs", "Remote publication checks passed")?;
            log.write(
                "sshfs",
                &format!("Mounted remote movie library at {}", mount.root.display()),
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            return match mount.close() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; {cleanup}").into()),
            };
        }
        Ok(mount)
    }
    pub fn close(&mut self) -> Result<()> {
        self.close_with_timeout(Duration::from_secs(5))
    }
    fn close_with_timeout(&mut self, timeout: Duration) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        // WinFsp removes a direct foreground mount when its owning process exits.
        // Never kill by image name: other users' mounts must survive.
        self.process.stop();
        let started = Instant::now();
        while self.root.exists() {
            if started.elapsed() > timeout {
                let message = format!(
                    "SSHFS mount {} remains visible after its process stopped. Close applications using it and disconnect that drive before retrying. No mount contents were deleted.",
                    self.root.display()
                );
                let _ = self.log.write("sshfs", &message);
                return Err(message.into());
            }
            thread::sleep(Duration::from_millis(50));
        }
        self.log
            .write("sshfs", "Disconnected Windows SSHFS mount")?;
        Ok(())
    }
}
impl Drop for Mount {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!("{error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cygwin_options_wrap_embedded_quotes_even_without_spaces() {
        assert_eq!(
            cygwin_quoted_argument(r#"IdentityFile="/cygdrive/c/Users/patpm/.ssh/key""#),
            r#""IdentityFile=\"/cygdrive/c/Users/patpm/.ssh/key\"""#
        );
        assert_eq!(
            cygwin_quoted_argument(
                r#"UserKnownHostsFile="/cygdrive/c/Users/Some User/.ssh/known_hosts""#
            ),
            r#""UserKnownHostsFile=\"/cygdrive/c/Users/Some User/.ssh/known_hosts\"""#
        );
        assert_eq!(cygwin_quoted_argument("BatchMode=yes"), "\"BatchMode=yes\"");
        assert_eq!(cygwin_quoted_argument(r"a\b"), r#""a\\b""#);
    }

    fn publisher() -> (tempfile::TempDir, Publisher) {
        let (root, client) = crate::sftp::process_tests::connect("normal");
        let library = root.path().join("library with spaces");
        fs::create_dir(&library).unwrap();
        let publisher = Publisher {
            remote_root: library.to_string_lossy().replace('\\', "/"),
            root: library,
            client: Arc::new(Mutex::new(client.unwrap())),
        };
        (root, publisher)
    }
    fn sleeping_child() -> ChildGuard {
        ChildGuard::spawn(
            Command::new(crate::test_support::python()).args(["-c", "import time; time.sleep(60)"]),
        )
        .unwrap()
    }
    #[test]
    fn remote_publication_preflights_preserves_existing_files_and_removes_only_staging() {
        let (_root, publisher) = publisher();
        let cancel = AtomicBool::new(false);
        publisher.preflight(&cancel).unwrap();
        assert_eq!(fs::read_dir(&publisher.root).unwrap().count(), 0);
        let source = publisher.root.join("staging.mkv");
        let target = publisher.root.join("final.mkv");
        fs::write(&source, b"new movie").unwrap();
        assert!(!publisher.exists(&target, &cancel).unwrap());
        fs::write(&target, b"existing").unwrap();
        assert!(publisher.publish(&source, &target, &cancel).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"existing");
        assert!(source.exists());
        fs::remove_file(&target).unwrap();
        publisher.publish(&source, &target, &cancel).unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(&target).unwrap(), b"new movie");
        assert!(
            publisher
                .remote_path(&publisher.root.join("../outside"))
                .is_err()
        );
    }
    #[test]
    fn disconnect_stops_only_owned_process_and_retained_mount_contents_survive() {
        let (root, publisher) = publisher();
        let log = RunLog::create(root.path()).unwrap();
        let mut unrelated = sleeping_child();
        let mut mount = Mount {
            root: root.path().join("already-disconnected"),
            process: sleeping_child(),
            publisher,
            closed: false,
            log,
        };
        assert!(
            !mount
                .publisher
                .exists(
                    &mount.publisher.root.join("missing.mkv"),
                    &AtomicBool::new(false)
                )
                .unwrap()
        );
        assert_eq!(mount.path(), root.path().join("already-disconnected"));
        mount.close().unwrap();
        assert!(mount.process.child.try_wait().unwrap().is_some());
        assert!(unrelated.child.try_wait().unwrap().is_none());
        let retained = root.path().join("retained");
        fs::create_dir(&retained).unwrap();
        fs::write(retained.join("keep.mkv"), b"existing").unwrap();
        mount.root = retained.clone();
        mount.process = sleeping_child();
        mount.closed = false;
        let error = mount
            .close_with_timeout(Duration::from_millis(25))
            .unwrap_err()
            .to_string();
        assert!(error.contains(&retained.display().to_string()));
        assert_eq!(fs::read(retained.join("keep.mkv")).unwrap(), b"existing");
        assert!(mount.process.child.try_wait().unwrap().is_some());
        assert!(unrelated.child.try_wait().unwrap().is_none());
    }

    #[test]
    fn cygwin_paths_preserve_spaces_and_convert_drive_and_unc_paths() {
        assert_eq!(
            cygwin_path(Path::new(r"C:\Users\Some User\.ssh\key")).unwrap(),
            "/cygdrive/c/Users/Some User/.ssh/key"
        );
        assert_eq!(
            cygwin_path(Path::new(r"\\server\share\key")).unwrap(),
            "//server/share/key"
        );
        assert!(cygwin_path(Path::new("relative")).is_err());
    }
}
