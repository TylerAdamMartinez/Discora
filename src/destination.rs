//! Config-managed SSHFS output. The mount stays alive until all rip workers have
//! stopped. No reconnect: lost remote handles must fail instead of silently
//! continuing a partially written movie on a new connection.
#[cfg(target_os = "linux")]
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::ffi::OsString;
use std::{path::PathBuf, sync::atomic::AtomicBool};

use crate::{Result, rip::RunLog};
#[cfg(not(windows))]
use std::path::Path;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct RipSettings {
    #[serde(default)]
    pub destination: DestinationSettings,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, toml::Value>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum DestinationSettings {
    #[default]
    Local,
    Sshfs(Remote),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    pub movies_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<PathBuf>,
}

fn default_port() -> u16 {
    22
}

impl DestinationSettings {
    pub fn remote(self) -> Result<Option<Remote>> {
        match self {
            Self::Local => Ok(None),
            Self::Sshfs(remote) => {
                remote.validate()?;
                Ok(Some(remote))
            }
        }
    }
}

impl Remote {
    pub(crate) fn validate(&self) -> Result<()> {
        // Restrict connection identifiers to avoid ambiguity in SSHFS's
        // [user@]host:path syntax and in OpenSSH option parsing.
        if self.host.is_empty()
            || self.host.starts_with('-')
            || !self
                .host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_:[]".contains(&b))
        {
            return Err("rip.destination.host must be a hostname or IP address".into());
        }
        if self.host.contains(':') && !(self.host.starts_with('[') && self.host.ends_with(']')) {
            return Err("Use brackets around an IPv6 host, for example [::1]".into());
        }
        if self.username.is_empty()
            || self.username.starts_with('-')
            || !self
                .username
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err("rip.destination.username must be a valid SSH account name".into());
        }
        if self.port == 0 {
            return Err("rip.destination.port must be between 1 and 65535".into());
        }
        if !self.movies_path.starts_with('/') || self.movies_path.chars().any(char::is_control) {
            return Err("rip.destination.movies_path must be an absolute remote directory".into());
        }
        if let Some(identity) = &self.identity_file
            && (!identity.is_absolute()
                || identity.as_os_str().as_encoded_bytes().iter().any(|b| {
                    (b",\"".contains(b) || (cfg!(unix) && *b == b'\\')) || b.is_ascii_control()
                }))
        {
            return Err("rip.destination.identity_file must be an absolute path without commas, quotes, or control characters (backslashes are also disallowed on Unix)".into());
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn arguments(&self, mountpoint: &Path) -> Vec<OsString> {
        let mut args = vec![
            "-f".into(),
            "-p".into(),
            self.port.to_string().into(),
            "-o".into(),
            // direct_io bounds client caching; synchronous writes surface remote
            // write failures. BatchMode avoids hidden password prompts in the TUI.
            "BatchMode=yes,StrictHostKeyChecking=yes,ConnectTimeout=10,ServerAliveInterval=15,ServerAliveCountMax=2,direct_io,sshfs_sync".into(),
        ];
        if let Some(identity) = &self.identity_file {
            args.push("-o".into());
            // SSH parses -o values as config syntax, so quote spaces within the
            // value as well as preserving the entire argument for SSHFS.
            let mut option = OsString::from("IdentityFile=\"");
            option.push(identity);
            option.push("\"");
            args.push(option);
        }
        args.push(format!("{}@{}:{}", self.username, self.host, self.movies_path).into());
        args.push(mountpoint.as_os_str().to_owned());
        args
    }
}

pub enum Destination {
    Local(PathBuf),
    Sshfs(Remote),
}

pub struct MountedDestination {
    pub root: PathBuf,
    pub label: String,
    pub remote: Option<Remote>,
    mount: Option<Mount>,
    #[cfg(windows)]
    pub publisher: Option<crate::windows_remote::Publisher>,
}

impl MountedDestination {
    pub fn open(destination: Destination, cancel: &AtomicBool, log: &RunLog) -> Result<Self> {
        match destination {
            Destination::Local(root) => Ok(Self {
                label: root.display().to_string(),
                root,
                remote: None,
                mount: None,
                #[cfg(windows)]
                publisher: None,
            }),
            Destination::Sshfs(remote) => {
                println!(
                    "Connecting to {}@{}:{} over SSHFS…",
                    remote.username, remote.host, remote.movies_path
                );
                let mount = Mount::connect(&remote, cancel, log)?;
                Ok(Self {
                    #[cfg(windows)]
                    publisher: Some(mount.publisher.clone()),
                    root: mount.path().to_owned(),
                    label: format!(
                        "{}@{}:{}",
                        remote.username,
                        remote.host,
                        remote.movies_path.trim_end_matches('/')
                    ),
                    remote: Some(remote),
                    mount: Some(mount),
                })
            }
        }
    }

    pub fn close(mut self) -> Result<()> {
        if let Some(mut mount) = self.mount.take() {
            mount.close()?;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
struct Mount {
    directory: Option<PathBuf>,
    process: Child,
    errors: tempfile::NamedTempFile,
    active: bool,
    unmount: Vec<OsString>,
    log: RunLog,
}

#[cfg(target_os = "linux")]
impl Mount {
    fn path(&self) -> &Path {
        self.directory.as_deref().expect("mount directory")
    }

    fn connect(remote: &Remote, cancel: &AtomicBool, log: &RunLog) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        if !cfg!(target_os = "linux") {
            return Err("Config-managed SSHFS ripping currently requires Linux".into());
        }
        let available = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|directory| {
                ["fusermount3", "fusermount"].iter().any(|name| {
                    fs::metadata(directory.join(name)).is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                })
            })
        });
        if !available {
            return Err("Install fusermount3 (or fusermount) before using SSHFS".into());
        }
        Self::connect_using(
            remote,
            cancel,
            log,
            Path::new("sshfs"),
            &[],
            vec!["fusermount3".into(), "fusermount".into()],
            is_mounted,
        )
    }

    fn connect_using(
        remote: &Remote,
        cancel: &AtomicBool,
        log: &RunLog,
        executable: &Path,
        prefix: &[OsString],
        unmount: Vec<OsString>,
        mounted: impl Fn(&Path) -> Result<bool>,
    ) -> Result<Self> {
        remote.validate()?;
        crate::makemkv::check_cancel(cancel)?;
        let directory = tempfile::Builder::new()
            .prefix("discora-sshfs-")
            .tempdir()?;
        let errors = tempfile::NamedTempFile::new()?;
        let process = Command::new(executable)
            .args(prefix)
            .args(remote.arguments(directory.path()))
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(errors.reopen()?)
            .spawn()
            .map_err(|error| format!("Cannot start sshfs; install SSHFS and FUSE: {error}"))?;
        let mut mount = Self {
            directory: Some(directory.keep()),
            process,
            errors,
            active: false,
            unmount,
            log: log.clone(),
        };
        let result = (|| {
            let started = Instant::now();
            loop {
                crate::makemkv::check_cancel(cancel)?;
                if let Some(status) = mount.process.try_wait()? {
                    return Err(format!("SSHFS connection failed ({status}): {}. Configure SSH key/agent authentication and verify the server in known_hosts first.", mount.diagnostics()).into());
                }
                if mounted(mount.path())? {
                    mount.active = true;
                    break;
                }
                if started.elapsed() > Duration::from_secs(20) {
                    return Err("Timed out waiting for SSHFS to mount".into());
                }
                thread::sleep(Duration::from_millis(50));
            }
            log.write(
                "sshfs",
                &format!(
                    "Mounted {}@{}:{} at {}",
                    remote.username,
                    remote.host,
                    remote.movies_path,
                    mount.path().display()
                ),
            )?;
            crate::makemkv::check_cancel(cancel)?;
            preflight(mount.path()).map_err(|error| format!("SSHFS destination must support writing, syncing, and no-overwrite hard-link publication: {error}"))?;
            crate::makemkv::check_cancel(cancel)?;
            Ok(())
        })();
        if let Err(error) = result {
            let cleanup = mount.close();
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; {cleanup}").into()),
            };
        }
        Ok(mount)
    }

    fn diagnostics(&mut self) -> String {
        let mut contents = String::new();
        if let Ok(mut file) = self.errors.reopen() {
            let _ = file
                .seek(SeekFrom::End(-8192))
                .or_else(|_| file.seek(SeekFrom::Start(0)));
            let _ = file.read_to_string(&mut contents);
        }
        contents.trim().to_owned()
    }

    fn close(&mut self) -> Result<()> {
        if self.directory.is_none() {
            return Ok(());
        }
        let path = self.path().to_owned();
        let mounted = match is_mounted(&path) {
            Ok(mounted) => mounted,
            Err(error) if !self.active => {
                self.directory.take();
                self.stop();
                return Err(format!(
                    "Cannot inspect SSHFS mount at {}; mountpoint retained: {error}",
                    path.display()
                )
                .into());
            }
            Err(_) => true,
        };
        if self.active || mounted {
            let result = unmount(&self.unmount, &path);
            if let Err(error) = result {
                // Never recursively remove a mountpoint that may still be live.
                let remaining = self.directory.take().unwrap();
                self.stop();
                let message = format!(
                    "Could not unmount SSHFS at {}; run fusermount3 -u on that path: {error}",
                    remaining.display()
                );
                let _ = self.log.write("sshfs", &message);
                return Err(message.into());
            }
            self.active = false;
            let _ = self.log.write("sshfs", "Unmounted remote movie library");
        }
        self.stop();
        // remove_dir never follows the mounted filesystem or recursively removes
        // remote data, including when mount state unexpectedly changes.
        let path = self.directory.take().unwrap();
        fs::remove_dir(&path).map_err(|error| {
            format!(
                "Could not remove SSHFS mountpoint {}: {error}",
                path.display()
            )
        })?;
        Ok(())
    }

    fn stop(&mut self) {
        if self.process.try_wait().ok().flatten().is_none() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(self.process.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = self.process.kill();
        }
        let _ = self.process.wait();
    }
}

#[cfg(target_os = "linux")]
impl Drop for Mount {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!("{error}");
        }
    }
}

#[cfg(target_os = "linux")]
fn is_mounted(path: &Path) -> Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let mut escaped = Vec::new();
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b' ' | b'\t' | b'\n' | b'\\' => {
                escaped.extend_from_slice(format!("\\{byte:03o}").as_bytes())
            }
            _ => escaped.push(*byte),
        }
    }
    let mounts = fs::read("/proc/self/mountinfo")?;
    Ok(mounts
        .split(|byte| *byte == b'\n')
        .any(|line| line.split(|byte| *byte == b' ').nth(4) == Some(escaped.as_slice())))
}

#[cfg(target_os = "linux")]
fn unmount(executables: &[OsString], path: &Path) -> Result<()> {
    for executable in executables {
        let mut child = match Command::new(executable)
            .arg("-u")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("fusermount exited with {status}").into())
                };
            }
            if started.elapsed() > Duration::from_secs(5) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Unmount timed out".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    Err("Install fusermount3 (or fusermount) to unmount SSHFS".into())
}

#[cfg(target_os = "linux")]
fn preflight(root: &Path) -> Result<()> {
    let directory = tempfile::Builder::new()
        .prefix(".discora-check-")
        .tempdir_in(root)?;
    let result = (|| -> Result<()> {
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        let linked = directory.path().join("linked");
        let mut file = File::create(&source)?;
        file.write_all(b"discora filesystem check")?;
        file.sync_all()?;
        fs::hard_link(&source, &linked)?;
        fs::write(&target, b"existing destination")?;
        // SFTP v3 may report a generic EPERM instead of EEXIST. Require
        // refusal and unchanged contents, rather than a specific errno.
        if fs::hard_link(&source, &target).is_ok() || fs::read(&target)? != b"existing destination"
        {
            return Err("Filesystem does not refuse an existing hard-link destination".into());
        }
        if fs::read(linked)? != b"discora filesystem check" {
            return Err("Filesystem verification failed".into());
        }
        Ok(())
    })();
    let cleanup = directory.close();
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (result, Err(error)) => Err(format!(
            "Filesystem check: {result:?}; could not remove check directory: {error}"
        )
        .into()),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, sync::atomic::Ordering};

    fn remote() -> Remote {
        Remote {
            host: "192.168.1.50".into(),
            port: 22,
            username: "movie-upload".into(),
            movies_path: "/media/Movies with spaces".into(),
            identity_file: None,
        }
    }

    fn executable(directory: &Path, name: &str, script: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, format!("#!/usr/bin/python3\n{script}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn connection_arguments_preserve_spaces_and_require_verified_key_auth() {
        let mut remote = remote();
        remote.identity_file = Some("/home/friend/SSH Keys/movie key".into());
        remote.validate().unwrap();
        let args = remote.arguments(Path::new("/tmp/mount point"));
        assert_eq!(
            args[args.len() - 2],
            "movie-upload@192.168.1.50:/media/Movies with spaces"
        );
        assert_eq!(args.last().unwrap(), "/tmp/mount point");
        assert!(
            args.iter()
                .any(|arg| arg == "IdentityFile=\"/home/friend/SSH Keys/movie key\"")
        );
        let options = args[4].to_str().unwrap();
        for expected in [
            "StrictHostKeyChecking=yes",
            "BatchMode=yes",
            "direct_io",
            "sshfs_sync",
        ] {
            assert!(options.contains(expected));
        }
        assert!(!options.contains("reconnect"));
    }

    #[test]
    fn invalid_remote_identifiers_and_paths_are_rejected() {
        for host in [
            "",
            "-option",
            "user@host",
            "host,option",
            "host/path",
            "::1",
        ] {
            let mut remote = remote();
            remote.host = host.into();
            assert!(remote.validate().is_err(), "accepted {host}");
        }
        let mut remote = remote();
        remote.host = "[::1]".into();
        remote.validate().unwrap();
        remote.username = "user@host".into();
        assert!(remote.validate().is_err());
        remote.username = "movie-upload".into();
        remote.port = 0;
        assert!(remote.validate().is_err());
        remote.port = 22;
        remote.movies_path = "relative".into();
        assert!(remote.validate().is_err());
        remote.movies_path = "/media/Movies".into();
        remote.identity_file = Some("/keys/key,option".into());
        assert!(remote.validate().is_err());
    }

    #[test]
    fn filesystem_check_cleans_up_without_touching_neighbors() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("existing.mkv"), "existing").unwrap();
        preflight(root.path()).unwrap();
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        assert_eq!(
            fs::read_to_string(root.path().join("existing.mkv")).unwrap(),
            "existing"
        );
        assert!(preflight(&root.path().join("missing")).is_err());
    }

    #[test]
    fn local_output_requires_no_mount_and_preserves_library() {
        let root = tempfile::tempdir().unwrap();
        let log = RunLog::create(root.path()).unwrap();
        let mounted = MountedDestination::open(
            Destination::Local(root.path().into()),
            &AtomicBool::new(false),
            &log,
        )
        .unwrap();
        assert_eq!(mounted.root, root.path());
        mounted.close().unwrap();
        assert!(root.path().exists());
    }

    #[test]
    fn mount_lifecycle_preflights_unmounts_and_reaps_foreground_process() {
        let root = tempfile::tempdir().unwrap();
        let sshfs = executable(root.path(), "sshfs", "import time\ntime.sleep(60)\n");
        let unmount = PathBuf::from("/usr/bin/true");
        let log = RunLog::create(root.path()).unwrap();
        let mut mount = Mount::connect_using(
            &remote(),
            &AtomicBool::new(false),
            &log,
            Path::new("/usr/bin/python3"),
            &[sshfs.into_os_string()],
            vec![unmount.into_os_string()],
            |_| Ok(true),
        )
        .unwrap();
        let path = mount.path().to_owned();
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        assert!(mount.process.try_wait().unwrap().is_none());
        mount.close().unwrap();
        assert!(!path.exists());
        assert!(mount.process.try_wait().unwrap().is_some());
        let contents = fs::read_to_string(log.path).unwrap();
        assert!(contents.contains("Mounted"));
        assert!(contents.contains("Unmounted"));
    }

    #[test]
    fn failed_connection_reports_diagnostics_and_removes_mountpoint() {
        let root = tempfile::tempdir().unwrap();
        let sshfs = executable(
            root.path(),
            "sshfs",
            "import sys\nprint('Permission denied (publickey)', file=sys.stderr)\nsys.exit(1)\n",
        );
        let log = RunLog::create(root.path()).unwrap();
        let captured = std::cell::RefCell::new(None);
        let result = Mount::connect_using(
            &remote(),
            &AtomicBool::new(false),
            &log,
            Path::new("/usr/bin/python3"),
            &[sshfs.into_os_string()],
            vec![],
            |path| {
                *captured.borrow_mut() = Some(path.to_owned());
                Ok(false)
            },
        );
        let error = result.err().unwrap().to_string();
        assert!(error.contains("Permission denied (publickey)"), "{error}");
        assert!(!captured.borrow().as_ref().unwrap().exists());
    }

    #[test]
    fn cancellation_during_connection_removes_mountpoint_and_stops_process() {
        let root = tempfile::tempdir().unwrap();
        let sshfs = executable(root.path(), "sshfs", "import time\ntime.sleep(60)\n");
        let log = RunLog::create(root.path()).unwrap();
        let cancel = AtomicBool::new(false);
        let captured = std::cell::RefCell::new(None);
        let result = Mount::connect_using(
            &remote(),
            &cancel,
            &log,
            Path::new("/usr/bin/python3"),
            &[sshfs.into_os_string()],
            vec![],
            |path| {
                *captured.borrow_mut() = Some(path.to_owned());
                cancel.store(true, Ordering::Relaxed);
                Ok(false)
            },
        );
        assert!(result.is_err());
        assert!(!captured.borrow().as_ref().unwrap().exists());
    }

    #[test]
    fn unmount_failure_retains_mountpoint_without_deleting_its_contents() {
        let root = tempfile::tempdir().unwrap();
        let sshfs = executable(root.path(), "sshfs", "import time\ntime.sleep(60)\n");
        let unmount = PathBuf::from("/usr/bin/false");
        let log = RunLog::create(root.path()).unwrap();
        let mut mount = Mount::connect_using(
            &remote(),
            &AtomicBool::new(false),
            &log,
            Path::new("/usr/bin/python3"),
            &[sshfs.into_os_string()],
            vec![unmount.into_os_string()],
            |_| Ok(true),
        )
        .unwrap();
        let path = mount.path().to_owned();
        fs::write(path.join("keep.mkv"), "movie").unwrap();
        let error = mount.close().unwrap_err().to_string();
        assert!(error.contains(&path.display().to_string()));
        assert_eq!(fs::read_to_string(path.join("keep.mkv")).unwrap(), "movie");
        assert!(mount.process.try_wait().unwrap().is_some());
        // This is a simulated mount backed by a disposable local test directory.
        fs::remove_file(path.join("keep.mkv")).unwrap();
        fs::remove_dir(path).unwrap();
    }
}

#[cfg(windows)]
use crate::windows_remote::Mount;

#[cfg(not(any(windows, target_os = "linux")))]
struct Mount;
#[cfg(not(any(windows, target_os = "linux")))]
impl Mount {
    fn connect(_: &Remote, _: &AtomicBool, _: &RunLog) -> Result<Self> {
        Err("Managed SSHFS requires Linux or Windows".into())
    }
    fn path(&self) -> &Path {
        unreachable!()
    }
    fn close(&mut self) -> Result<()> {
        Ok(())
    }
}
