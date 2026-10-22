//! Minimal SFTP v3 control channel over an existing OpenSSH executable.
//! Movie bytes go through SSHFS; this channel handles server-side publication.
use crate::{Result, makemkv::check_cancel, process::ChildGuard, rip::RunLog};
use std::{
    io::{self, Read, Write},
    process::{Command, Stdio},
    sync::{atomic::AtomicBool, mpsc},
    thread,
    time::{Duration, Instant},
};

const MAX_PACKET: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);

fn number(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn string(out: &mut Vec<u8>, value: &[u8]) {
    number(out, value.len() as u32);
    out.extend_from_slice(value);
}
struct Packet<'a>(&'a [u8]);
impl<'a> Packet<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        if len > self.0.len() {
            return Err("Truncated SFTP packet".into());
        }
        let (value, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(value)
    }
    fn number(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn string(&mut self) -> Result<&'a [u8]> {
        let len = self.number()? as usize;
        self.take(len)
    }
}
fn exchange(input: &mut impl Write, output: &mut impl Read, packet: &[u8]) -> io::Result<Vec<u8>> {
    input.write_all(&(packet.len() as u32).to_be_bytes())?;
    input.write_all(packet)?;
    input.flush()?;
    let mut size = [0; 4];
    output.read_exact(&mut size).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("Reading SFTP response length: {error}"),
        )
    })?;
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > MAX_PACKET {
        return Err(io::Error::other("Invalid SFTP packet size"));
    }
    let mut response = vec![0; size];
    output.read_exact(&mut response).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("Reading SFTP response body ({size} bytes): {error}"),
        )
    })?;
    Ok(response)
}

pub struct Client {
    process: ChildGuard,
    requests: Option<mpsc::Sender<Vec<u8>>>,
    responses: mpsc::Receiver<io::Result<Vec<u8>>>,
    worker: Option<thread::JoinHandle<()>>,
    next_id: u32,
    failed: bool,
    timeout: Duration,
    log: RunLog,
}
impl Client {
    pub fn connect(command: &mut Command, cancel: &AtomicBool, log: &RunLog) -> Result<Self> {
        let errors = std::fs::OpenOptions::new().append(true).open(&log.path)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(errors);
        log.write(
            "sftp",
            &format!("Starting SSH control process: {command:?}"),
        )?;
        let mut process = ChildGuard::spawn(command).map_err(|error| {
            let _ = log.write(
                "sftp",
                &format!("Cannot start SSH control process: {error}"),
            );
            error
        })?;
        log.write(
            "sftp",
            &format!("SSH control process started (PID {})", process.child.id()),
        )?;
        let mut input = process.child.stdin.take().ok_or("No SSH stdin")?;
        let mut output = process.child.stdout.take().ok_or("No SSH stdout")?;
        let (tx, requests) = mpsc::channel::<Vec<u8>>();
        let (responses, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            while let Ok(packet) = requests.recv() {
                let result = exchange(&mut input, &mut output, &packet);
                let failed = result.is_err();
                if responses.send(result).is_err() || failed {
                    break;
                }
            }
        });
        let mut client = Self {
            process,
            requests: Some(tx),
            responses: rx,
            worker: Some(worker),
            next_id: 0,
            failed: false,
            timeout: TIMEOUT,
            log: log.clone(),
        };
        log.write(
            "sftp",
            "Sending SFTP v3 initialization; waiting for server greeting",
        )?;
        let hello = client
            .exchange(vec![1, 0, 0, 0, 3], cancel)
            .map_err(|error| {
                format!(
                    "SFTP initialization failed: {error}; see {} for SSH diagnostics",
                    log.path.display()
                )
            })?;
        validate_hello(&hello).map_err(|error| {
            let _ = log.write("sftp", &format!("SFTP greeting rejected: {error}"));
            error
        })?;
        log.write(
            "sftp",
            "SFTP v3 connected; server supports hardlink and fsync extensions",
        )?;
        Ok(client)
    }
    fn exchange(&mut self, packet: Vec<u8>, cancel: &AtomicBool) -> Result<Vec<u8>> {
        if self.failed {
            return Err("SFTP connection is unavailable; start a new rip".into());
        }
        check_cancel(cancel)?;
        if packet.len() > MAX_PACKET {
            return Err("SFTP request too large".into());
        }
        self.requests
            .as_ref()
            .ok_or("SFTP connection closed")?
            .send(packet)?;
        let started = Instant::now();
        let result = loop {
            if let Err(error) = check_cancel(cancel) {
                break Err(error);
            }
            if started.elapsed() >= self.timeout {
                break Err("SFTP operation timed out".into());
            }
            match self.responses.recv_timeout(Duration::from_millis(25)) {
                Ok(Ok(packet)) => break Ok(packet),
                Ok(Err(error)) => break Err(error.into()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break Err("SFTP connection closed".into()),
            }
        };
        if let Err(error) = &result {
            let status = match self.process.child.try_wait() {
                Ok(Some(status)) => format!("exited with {status}"),
                Ok(None) => "still running before cleanup".to_owned(),
                Err(error) => format!("status unavailable: {error}"),
            };
            let _ = self.log.write(
                "sftp",
                &format!(
                    "SFTP exchange failed after {:?}: {error}; SSH PID {} {status}",
                    started.elapsed(),
                    self.process.child.id()
                ),
            );
            self.failed = true;
            self.process.stop();
        }
        result
    }
    fn request(&mut self, kind: u8, body: &[u8], cancel: &AtomicBool) -> Result<Vec<u8>> {
        self.next_id = self.next_id.wrapping_add(1);
        let mut request = vec![kind];
        number(&mut request, self.next_id);
        request.extend_from_slice(body);
        let response = self.exchange(request, cancel)?;
        let mut packet = Packet(&response);
        packet.take(1)?;
        if packet.number()? != self.next_id {
            self.failed = true;
            self.process.stop();
            return Err("SFTP response ID mismatch".into());
        }
        Ok(response)
    }
    fn status(packet: &[u8]) -> Result<u32> {
        let mut packet = Packet(packet);
        if packet.take(1)? != [101] {
            return Err("Expected SFTP status".into());
        }
        packet.number()?;
        let code = packet.number()?;
        Ok(code)
    }
    fn success(packet: &[u8]) -> Result<()> {
        let code = Self::status(packet)?;
        if code != 0 {
            let message = Packet(&packet[9..])
                .string()
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .unwrap_or_default();
            return Err(format!("SFTP operation failed (status {code}): {message}").into());
        }
        Ok(())
    }
    pub fn exists(&mut self, path: &str, cancel: &AtomicBool) -> Result<bool> {
        let mut body = Vec::new();
        string(&mut body, path.as_bytes());
        let response = self.request(7, &body, cancel)?; // LSTAT, never follow a symlink.
        if response[0] == 105 {
            return Ok(true);
        }
        match Self::status(&response)? {
            2 => Ok(false),
            code => Err(format!("Cannot inspect remote path {path} (SFTP status {code})").into()),
        }
    }
    pub fn verify_output(&mut self, path: &str, cancel: &AtomicBool) -> Result<()> {
        let mut body = Vec::new();
        string(&mut body, path.as_bytes());
        let response = self.request(7, &body, cancel)?;
        let mut packet = Packet(&response);
        if packet.take(1)? != [105] {
            Self::success(&response)?;
            return Err("Expected SFTP attributes".into());
        }
        packet.number()?;
        let flags = packet.number()?;
        if flags & 1 == 0 {
            return Err("Server did not report output size".into());
        }
        let size = u64::from_be_bytes(packet.take(8)?.try_into()?);
        if flags & 2 != 0 {
            packet.take(8)?;
        } // uid/gid precede permissions.
        if flags & 4 == 0 || packet.number()? & 0o170000 != 0o100000 || size == 0 {
            return Err(format!("Remote output is not a nonempty regular file: {path}").into());
        }
        Ok(())
    }
    fn open(&mut self, path: &str, flags: u32, cancel: &AtomicBool) -> Result<Vec<u8>> {
        let mut body = Vec::new();
        string(&mut body, path.as_bytes());
        number(&mut body, flags);
        number(&mut body, 0);
        let response = self.request(3, &body, cancel)?;
        let mut packet = Packet(&response);
        if packet.take(1)? != [102] {
            Self::success(&response)?;
            return Err("Expected SFTP handle".into());
        }
        packet.number()?;
        Ok(packet.string()?.to_vec())
    }
    fn close(&mut self, handle: &[u8], cancel: &AtomicBool) -> Result<()> {
        let mut body = Vec::new();
        string(&mut body, handle);
        Self::success(&self.request(4, &body, cancel)?)
    }
    pub fn hard_link(&mut self, source: &str, target: &str, cancel: &AtomicBool) -> Result<()> {
        let mut body = Vec::new();
        string(&mut body, b"hardlink@openssh.com");
        string(&mut body, source.as_bytes());
        string(&mut body, target.as_bytes());
        Self::success(&self.request(200, &body, cancel)?)
    }
    pub fn sync(&mut self, path: &str, cancel: &AtomicBool) -> Result<()> {
        let handle = self.open(path, 3, cancel)?; // READ | WRITE; fsync needs a writable handle on some servers.
        let mut body = Vec::new();
        string(&mut body, b"fsync@openssh.com");
        string(&mut body, &handle);
        let result = Self::success(&self.request(200, &body, cancel)?);
        let closed = self.close(&handle, cancel);
        result.and(closed)
    }
    pub fn read_small(&mut self, path: &str, cancel: &AtomicBool) -> Result<Vec<u8>> {
        let handle = self.open(path, 1, cancel)?;
        let mut body = Vec::new();
        string(&mut body, &handle);
        body.extend_from_slice(&0u64.to_be_bytes());
        number(&mut body, 1024);
        let response = self.request(5, &body, cancel)?;
        let closed = self.close(&handle, cancel);
        let mut packet = Packet(&response);
        if packet.take(1)? != [103] {
            Self::success(&response)?;
            return Err("Expected SFTP data".into());
        }
        packet.number()?;
        let data = packet.string()?.to_vec();
        closed?;
        Ok(data)
    }
}
fn validate_hello(hello: &[u8]) -> Result<()> {
    let mut packet = Packet(hello);
    if packet.take(1)? != [2] || packet.number()? != 3 {
        return Err("Server must support SFTP v3".into());
    }
    let mut hardlink = false;
    let mut fsync = false;
    while !packet.0.is_empty() {
        let name = packet.string()?;
        let version = packet.string()?;
        hardlink |= name == b"hardlink@openssh.com" && version == b"1";
        fsync |= name == b"fsync@openssh.com" && version == b"1";
    }
    if !hardlink || !fsync {
        return Err("Remote server must support hardlink@openssh.com and fsync@openssh.com".into());
    }
    Ok(())
}
impl Drop for Client {
    fn drop(&mut self) {
        self.requests.take();
        self.process.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hello_requires_both_extensions_and_rejects_truncation() {
        let mut hello = vec![2, 0, 0, 0, 3];
        assert!(validate_hello(&hello).is_err());
        for name in [b"hardlink@openssh.com".as_slice(), b"fsync@openssh.com"] {
            string(&mut hello, name);
            string(&mut hello, b"1");
        }
        validate_hello(&hello).unwrap();
        for length in 0..hello.len() {
            assert!(validate_hello(&hello[..length]).is_err());
        }
    }
}

#[cfg(test)]
pub(crate) mod process_tests {
    use super::*;
    use std::{
        fs,
        sync::{Arc, atomic::Ordering},
    };
    const SERVER: &str = r#"
import os, sys, struct, time
from pathlib import Path
mode = sys.argv[1]
if mode == 'auth-fail':
    print('Permission denied', file=sys.stderr)
    sys.exit(1)
handles = {}
next_handle = 0
def u32(n): return struct.pack('>I', n)
def string(s): return u32(len(s)) + s
def send(s):
    sys.stdout.buffer.write(u32(len(s)) + s)
    sys.stdout.buffer.flush()
def status(i, n): send(bytes([101]) + i + u32(n) + string(b'fixture status') + string(b''))
while True:
    size = sys.stdin.buffer.read(4)
    if not size: break
    data = sys.stdin.buffer.read(struct.unpack('>I', size)[0])
    kind, payload = data[0], data[5:]
    if kind == 1:
        ext = b'' if mode == 'no-extensions' else string(b'hardlink@openssh.com') + string(b'1') + string(b'fsync@openssh.com') + string(b'1')
        send(bytes([2]) + u32(3) + ext)
        continue
    if mode == 'stall':
        time.sleep(60)
        continue
    if mode == 'disconnect': sys.exit(1)
    if mode == 'oversized':
        sys.stdout.buffer.write(u32(1024 * 1024))
        sys.stdout.buffer.flush()
        continue
    i = data[1:5]
    def take_string():
        global payload
        n = struct.unpack('>I', payload[:4])[0]
        value, payload = payload[4:4+n], payload[4+n:]
        return value
    try:
        if kind == 7:
            p = os.fsdecode(take_string())
            st = os.lstat(p)
            send(bytes([105]) + i + u32(5) + struct.pack('>Q', st.st_size) + u32(st.st_mode))
        elif kind == 3:
            p = os.fsdecode(take_string())
            flags = struct.unpack('>I', payload[:4])[0]
            f = open(p, 'r+b' if flags == 3 else 'rb')
            next_handle += 1
            h = str(next_handle).encode()
            handles[h] = f
            send(bytes([102]) + i + string(h))
        elif kind == 4:
            handles.pop(take_string()).close()
            status(i, 0)
        elif kind == 5:
            f = handles[take_string()]
            offset, length = struct.unpack('>QI', payload[:12])
            f.seek(offset)
            send(bytes([103]) + i + string(f.read(length)))
        elif kind == 200:
            ext = take_string()
            if ext == b'hardlink@openssh.com':
                source, target = os.fsdecode(take_string()), os.fsdecode(take_string())
                os.link(source, target)
            elif ext == b'fsync@openssh.com':
                os.fsync(handles[take_string()].fileno())
            else: raise RuntimeError('unknown extension')
            status(i, 0)
        else: status(i, 8)
    except FileNotFoundError: status(i, 2)
    except Exception: status(i, 4)
"#;
    pub(crate) fn connect(mode: &str) -> (tempfile::TempDir, Result<Client>) {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("fake sftp server.py");
        fs::write(&script, SERVER).unwrap();
        let log = RunLog::create(root.path()).unwrap();
        let client = Client::connect(
            Command::new(crate::test_support::python())
                .arg(script)
                .arg(mode),
            &AtomicBool::new(false),
            &log,
        );
        (root, client)
    }
    #[test]
    fn server_publication_syncs_and_refuses_racing_destination() {
        let (root, client) = connect("normal");
        let mut client = client.unwrap();
        let source = root.path().join("movie with spaces.mkv");
        let target = root.path().join("final.mkv");
        let source = source.to_str().unwrap();
        let target = target.to_str().unwrap();
        let cancel = AtomicBool::new(false);
        fs::write(source, b"movie").unwrap();
        assert!(client.exists(source, &cancel).unwrap());
        assert!(!client.exists(target, &cancel).unwrap());
        client.verify_output(source, &cancel).unwrap();
        client.sync(source, &cancel).unwrap();
        fs::write(target, b"existing").unwrap();
        assert!(client.hard_link(source, target, &cancel).is_err());
        assert_eq!(client.read_small(target, &cancel).unwrap(), b"existing");
        fs::remove_file(target).unwrap();
        client.hard_link(source, target, &cancel).unwrap();
        fs::remove_file(source).unwrap();
        assert_eq!(client.read_small(target, &cancel).unwrap(), b"movie");
        let empty = root.path().join("empty.mkv");
        fs::write(&empty, b"").unwrap();
        assert!(
            client
                .verify_output(empty.to_str().unwrap(), &cancel)
                .is_err()
        );
        assert!(
            client
                .verify_output(root.path().to_str().unwrap(), &cancel)
                .is_err()
        );
    }
    #[test]
    fn authentication_failure_and_missing_extensions_fail_connection() {
        for mode in ["auth-fail", "no-extensions"] {
            let (root, result) = connect(mode);
            let error = match result {
                Ok(_) => panic!("connection should fail"),
                Err(error) => error.to_string(),
            };
            let log_path = fs::read_dir(root.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.extension().is_some_and(|ext| ext == "log"))
                .unwrap();
            let text = fs::read_to_string(log_path).unwrap();
            assert!(text.contains("Starting SSH control process"));
            if mode == "auth-fail" {
                assert!(text.contains("Permission denied"));
                assert!(text.contains("SFTP exchange failed"));
                assert!(text.contains("SSH PID"));
                assert!(error.contains("SFTP initialization failed"));
                assert!(error.contains("Reading SFTP response length"));
            } else {
                assert!(text.contains("SFTP greeting rejected"));
            }
        }
    }
    #[test]
    fn cancellation_timeout_disconnect_and_invalid_packets_reap_control_process() {
        for mode in ["cancel", "timeout", "disconnect", "oversized"] {
            let (_root, client) = connect(if matches!(mode, "cancel" | "timeout") {
                "stall"
            } else {
                mode
            });
            let mut client = client.unwrap();
            let cancel = Arc::new(AtomicBool::new(false));
            let worker_cancel = cancel.clone();
            client.timeout = Duration::from_millis(150);
            let thread = thread::spawn(move || {
                if mode == "cancel" {
                    thread::sleep(Duration::from_millis(25));
                    worker_cancel.store(true, Ordering::Relaxed);
                }
            });
            assert!(client.exists("unused", &cancel).is_err());
            thread.join().unwrap();
            assert!(client.process.child.try_wait().unwrap().is_some());
            assert!(client.failed);
        }
    }
}
