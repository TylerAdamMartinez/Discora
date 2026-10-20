//! MakeMKV robot protocol and a cancellable, headless process runner (Linux/Unix).
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{BufRead, BufReader, Read},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{Result, rip::RunLog};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};

#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Drive {
        index: u32,
        accessible: bool,
        name: String,
        disc: String,
    },
    Title {
        id: u32,
        attribute: u32,
        value: String,
    },
    Progress {
        total: u64,
        max: u64,
    },
    Operation(String),
    Message {
        code: u32,
        text: String,
    },
    Unknown,
}

/// Robot strings use backslash escaping, not CSV's doubled-quote convention.
fn fields(input: &str) -> Option<Vec<String>> {
    let mut result = Vec::new();
    let mut chars = input.chars().peekable();
    loop {
        let mut field = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next()? {
                    '"' => break,
                    '\\' => field.push(match chars.next()? {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '\\' => '\\',
                        '"' => '"',
                        _ => return None,
                    }),
                    c => field.push(c),
                }
            }
            if chars.peek().is_some_and(|c| *c != ',') {
                return None;
            }
        } else {
            while chars.peek().is_some_and(|c| *c != ',') {
                let c = chars.next()?;
                if matches!(c, '"' | '\\') {
                    return None;
                }
                field.push(c);
            }
        }
        result.push(field);
        match chars.next() {
            Some(',') => {}
            None => return Some(result),
            _ => return None,
        }
    }
}

pub fn parse(line: &str) -> Record {
    let parse_inner = || -> Option<Record> {
        let (kind, input) = line.split_once(':')?;
        let f = fields(input)?;
        Some(match kind {
            "DRV" if f.len() >= 6 => Record::Drive {
                index: f[0].parse().ok()?,
                // Current builds emit drive-state,999,media-flags rather than the
                // visible/enabled booleans in usage.txt. AP_DriveState 0..3 are
                // present drives; 256/257 mean missing/unmounting (apdefs.h).
                accessible: !f[4].is_empty()
                    && if f[2] == "999" {
                        f[1].parse::<u32>().ok().is_some_and(|state| state <= 3)
                    } else {
                        f[1] == "1" && f[2] == "1"
                    },
                name: f[4].clone(),
                disc: f[5].clone(),
            },
            // Actual title records are TINFO:title-id,attribute-id,message-code,value.
            // Attribute IDs verified against MakeMKV OSS 1.18.4 apdefs.h.
            "TINFO" if f.len() == 4 => Record::Title {
                id: f[0].parse().ok()?,
                attribute: f[1].parse().ok()?,
                value: f[3].clone(),
            },
            "PRGV" if f.len() == 3 => {
                let _: u64 = f[0].parse().ok()?;
                Record::Progress {
                    total: f[1].parse().ok()?,
                    max: f[2].parse().ok()?,
                }
            }
            "PRGC" | "PRGT" if f.len() == 3 => Record::Operation(f[2].clone()),
            "MSG" if f.len() >= 5 => Record::Message {
                code: f[0].parse().ok()?,
                text: f[3].clone(),
            },
            _ => Record::Unknown,
        })
    };
    parse_inner().unwrap_or(Record::Unknown)
}

#[derive(Debug, Clone)]
pub struct Drive {
    pub index: u32,
    pub name: String,
    pub disc: String,
}

#[derive(Debug, Clone, Default)]
pub struct Title {
    pub id: u32,
    pub name: String,
    pub duration: String,
    pub size: String,
    pub source: String,
    pub chapters: String,
    pub segment_map: String,
}

impl Title {
    pub fn seconds(&self) -> u64 {
        let parts: Option<Vec<u64>> = self.duration.split(':').map(|s| s.parse().ok()).collect();
        match parts.as_deref() {
            Some([h, m, s]) if *m < 60 && *s < 60 => h
                .saturating_mul(3600)
                .saturating_add(m * 60)
                .saturating_add(*s),
            _ => 0,
        }
    }
}

impl std::fmt::Display for Title {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Title {} | {} | {} | Chapters: {} | {} | {}\nSegments: {}",
            self.id,
            self.duration,
            self.size,
            if self.chapters.is_empty() {
                "—"
            } else {
                &self.chapters
            },
            self.source,
            self.name,
            if self.segment_map.is_empty() {
                "—"
            } else {
                &self.segment_map
            }
        )
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Log(String),
    Progress { total: u64, max: u64 },
    Operation(String),
}

#[derive(Clone)]
pub struct MakeMkv {
    executable: PathBuf,
    arguments: Vec<OsString>,
}

impl MakeMkv {
    pub fn new(executable: PathBuf) -> Self {
        Self::with_arguments(executable, Vec::new())
    }

    /// Allow an explicit interpreter/wrapper for process fixtures without changing PATH.
    pub fn with_arguments(executable: PathBuf, arguments: Vec<OsString>) -> Self {
        Self {
            executable,
            arguments,
        }
    }

    pub fn installed() -> Result<Self> {
        let executable = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .map(|path| path.join("makemkvcon"))
            .find(|path| {
                path.is_file()
                    && path
                        .metadata()
                        .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
            })
            .ok_or("makemkvcon was not found on PATH. Install MakeMKV first.")?;
        Ok(Self::new(executable))
    }

    pub fn drives(
        &self,
        cancel: &AtomicBool,
        log: &RunLog,
        emit: &mut impl FnMut(Event),
    ) -> Result<Vec<Drive>> {
        let mut drives = BTreeMap::new();
        // disc:9999 is the documented drive enumeration source. Some versions exit
        // nonzero because it isn't a real disc; parsed accessible drives are authoritative.
        let status = self.run(
            &["--cache=1".into(), "info".into(), "disc:9999".into()],
            cancel,
            log,
            emit,
            &mut |record| {
                if let Record::Drive {
                    index,
                    accessible: true,
                    name,
                    disc,
                } = record
                {
                    drives.insert(index, Drive { index, name, disc });
                }
            },
        )?;
        if drives.is_empty() {
            return Err(format!("No accessible optical drives found (MakeMKV: {status}). Check drive permissions and the run log.").into());
        }
        Ok(drives.into_values().collect())
    }

    pub fn titles(
        &self,
        disc: u32,
        cancel: &AtomicBool,
        log: &RunLog,
        emit: &mut impl FnMut(Event),
    ) -> Result<Vec<Title>> {
        let mut titles = BTreeMap::new();
        let status = self.run(
            &[
                "--minlength=0".into(),
                "info".into(),
                format!("disc:{disc}").into(),
            ],
            cancel,
            log,
            emit,
            &mut |record| {
                if let Record::Title {
                    id,
                    attribute,
                    value,
                } = record
                {
                    let title = titles.entry(id).or_insert_with(|| Title {
                        id,
                        ..Title::default()
                    });
                    match attribute {
                        2 => title.name = value,
                        8 => title.chapters = value,
                        9 => title.duration = value,
                        10 => title.size = value,
                        16 => title.source = value,
                        26 => title.segment_map = value,
                        _ => {}
                    }
                }
            },
        )?;
        if !status.success() {
            return Err(format!("Disc scan failed ({status}). See run log.").into());
        }
        let mut titles: Vec<Title> = titles.into_values().collect();
        titles.sort_by_key(|t| (std::cmp::Reverse(t.seconds()), t.id));
        if titles.is_empty() {
            return Err(
                "No titles found. Check the disc, MakeMKV registration, and the run log.".into(),
            );
        }
        Ok(titles)
    }

    pub fn rip(
        &self,
        disc: u32,
        title: u32,
        destination: &Path,
        cancel: &AtomicBool,
        log: &RunLog,
        emit: &mut impl FnMut(Event),
    ) -> Result<()> {
        let mut reported_failure = false;
        let status = self.run(
            &[
                "--minlength=0".into(),
                "mkv".into(),
                format!("disc:{disc}").into(),
                title.to_string().into(),
                destination.as_os_str().into(),
            ],
            cancel,
            log,
            emit,
            &mut |record| {
                // APP_DUMP_DONE_PARTIAL: MakeMKV can report failed titles even with exit 0.
                if let Record::Message { code: 5004, .. } = record {
                    reported_failure = true;
                }
            },
        )?;
        if !status.success() || reported_failure {
            return Err(
                format!("MakeMKV did not complete the rip ({status}). See run log.").into(),
            );
        }
        Ok(())
    }

    fn run(
        &self,
        args: &[OsString],
        cancel: &AtomicBool,
        log: &RunLog,
        emit: &mut impl FnMut(Event),
        record: &mut impl FnMut(Record),
    ) -> Result<std::process::ExitStatus> {
        check_cancel(cancel)?;
        log.write(
            "command",
            &format!(
                "{} {:?} -r --messages=-stdout --progress=-same {:?}",
                self.executable.display(),
                self.arguments,
                args
            ),
        )?;
        let child = Command::new(&self.executable)
            .args(&self.arguments)
            .args(["-r", "--messages=-stdout", "--progress=-same"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?;
        let mut child = ChildGuard {
            child,
            stopped: false,
        };
        let stdout = child
            .child
            .stdout
            .take()
            .ok_or("Cannot capture MakeMKV stdout")?;
        let stderr = child
            .child
            .stderr
            .take()
            .ok_or("Cannot capture MakeMKV stderr")?;
        let (tx, rx) = mpsc::sync_channel(128);
        let out = reader(stdout, "stdout", tx.clone());
        let err = reader(stderr, "stderr", tx);
        let result: Result<std::process::ExitStatus> = (|| {
            loop {
                check_cancel(cancel)?;
                if !child.stopped && child.child.try_wait()?.is_some() {
                    // A helper may have inherited the pipes and outlived the main
                    // process. Stop it so EOF cannot leave the UI waiting forever.
                    child.stop();
                }
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok((stream, Ok(line))) => {
                        log.write(stream, &line)?;
                        let parsed = if stream == "stdout" {
                            parse(&line)
                        } else {
                            Record::Unknown
                        };
                        match &parsed {
                            Record::Progress { total, max } => emit(Event::Progress {
                                total: *total,
                                max: *max,
                            }),
                            Record::Operation(text) => emit(Event::Operation(text.clone())),
                            Record::Message { text, .. } => emit(Event::Log(text.clone())),
                            _ => emit(Event::Log(line)),
                        }
                        record(parsed);
                    }
                    Ok((_, Err(error))) => {
                        return Err(error.into());
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            loop {
                check_cancel(cancel)?;
                if let Some(status) = child.child.try_wait()? {
                    return Ok(status);
                }
                thread::sleep(Duration::from_millis(25));
            }
        })();
        // Stop descendants before closing the queue or deleting output. Dropping the
        // receiver unblocks readers even when a child produced more than one queueful.
        child.stop();
        drop(rx);
        let out_result = out.join();
        let err_result = err.join();
        let status = result?;
        if out_result.is_err() || err_result.is_err() {
            return Err("MakeMKV output reader panicked".into());
        }
        check_cancel(cancel)?;
        log.write("exit", &status.to_string())?;
        Ok(status)
    }
}

pub fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err("Canceled".into())
    } else {
        Ok(())
    }
}

type OutputLine = (&'static str, std::io::Result<String>);

fn reader(
    input: impl Read + Send + 'static,
    stream: &'static str,
    tx: mpsc::SyncSender<OutputLine>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(input).split(b'\n') {
            let line = line.map(|bytes| {
                String::from_utf8_lossy(&bytes)
                    .trim_end_matches('\r')
                    .to_owned()
            });
            let failed = line.is_err();
            if tx.send((stream, line)).is_err() || failed {
                break;
            }
        }
    })
}

struct ChildGuard {
    child: Child,
    stopped: bool,
}

impl ChildGuard {
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let pgid = Pid::from_raw(self.child.id() as i32);
        let _ = killpg(pgid, Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.child.try_wait().is_ok_and(|status| status.is_some()) {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        // Also terminate helpers which outlived the parent.
        let _ = killpg(pgid, Signal::SIGKILL);
        let _ = self.child.wait();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
