use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    Result,
    files::{named_mkv, publish_mkv},
    makemkv::{self, Event, MakeMkv},
};

#[derive(Clone)]
pub struct RunLog {
    pub path: PathBuf,
    file: Arc<Mutex<File>>,
}

impl RunLog {
    pub fn create(directory: &Path) -> Result<Self> {
        fs::create_dir_all(directory)?;
        let (file, path) = tempfile::Builder::new()
            .prefix("rip-")
            .suffix(".log")
            .tempfile_in(directory)?
            .keep()?;
        Ok(Self {
            path,
            file: Arc::new(Mutex::new(file)),
        })
    }

    pub fn write(&self, stream: &str, text: &str) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let mut file = self.file.lock().map_err(|_| "Run log lock poisoned")?;
        writeln!(file, "[{now}ms UTC] [{stream}] {text}")?;
        file.flush()?;
        Ok(())
    }
}

pub fn log_directory() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        Ok(crate::platform::app_data("LOCALAPPDATA")
            .or_else(dirs::data_local_dir)
            .ok_or("Cannot locate LOCALAPPDATA for logs")?
            .join("discora/logs"))
    }
    #[cfg(unix)]
    {
        let state = std::env::var_os("XDG_STATE_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| dirs::home_dir().map(|p| p.join(".local/state")))
            .ok_or("Cannot locate the user state directory for logs")?;
        Ok(state.join("discora/logs"))
    }
}

/// Owns only the unique directory created for this attempt. Explicit close reports
/// cleanup errors; TempDir's Drop remains a fallback during unwinding.
pub struct Attempt {
    directory: tempfile::TempDir,
    destination: PathBuf,
}

impl Attempt {
    pub fn create(movie: &Path) -> Result<Self> {
        fs::create_dir_all(movie)?;
        let destination = named_mkv(movie)?;
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                return Err(format!(
                    "Refusing to replace existing file: {}",
                    destination.display()
                )
                .into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let directory = tempfile::Builder::new()
            .prefix(".discora-rip-")
            .tempdir_in(movie)?;
        Ok(Self {
            directory,
            destination,
        })
    }

    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    fn publish(
        &self,
        cancel: &AtomicBool,
        #[cfg(windows)] publisher: Option<&crate::windows_remote::Publisher>,
    ) -> Result<PathBuf> {
        makemkv::check_cancel(cancel)?;
        let mut mkvs = Vec::new();
        for entry in fs::read_dir(self.path())? {
            let entry = entry?;
            if entry
                .path()
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("mkv"))
            {
                if !entry.file_type()?.is_file() {
                    return Err("MakeMKV output is not a regular MKV file".into());
                }
                mkvs.push(entry.path());
            }
        }
        if mkvs.len() != 1 {
            return Err(format!(
                "Expected exactly one MKV from this rip; found {}",
                mkvs.len()
            )
            .into());
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&mkvs[0])?;
        if file.metadata()?.len() == 0 {
            return Err("MakeMKV produced an empty MKV".into());
        }
        #[cfg(windows)]
        if publisher.is_none() {
            file.sync_all()?;
        }
        #[cfg(unix)]
        file.sync_all()?;
        drop(file);
        makemkv::check_cancel(cancel)?;
        #[cfg(windows)]
        if let Some(publisher) = publisher {
            publisher.publish(&mkvs[0], &self.destination, cancel)?;
        } else {
            publish_mkv(&mkvs[0], &self.destination)?;
        }
        #[cfg(unix)]
        publish_mkv(&mkvs[0], &self.destination)?;
        Ok(self.destination.clone())
    }

    pub fn close(self) -> Result<()> {
        let path = self.directory.path().to_path_buf();
        self.directory.close().map_err(|error| {
            format!(
                "Could not delete partial output at {}: {error}",
                path.display()
            )
            .into()
        })
    }
}

pub struct Output<'a> {
    movie: &'a Path,
    #[cfg(windows)]
    publisher: Option<&'a crate::windows_remote::Publisher>,
}
impl<'a> Output<'a> {
    pub fn local(movie: &'a Path) -> Self {
        Self {
            movie,
            #[cfg(windows)]
            publisher: None,
        }
    }
    #[cfg(windows)]
    pub fn with_publisher(
        mut self,
        publisher: Option<&'a crate::windows_remote::Publisher>,
    ) -> Self {
        self.publisher = publisher;
        self
    }
}

pub fn run(
    makemkv: &MakeMkv,
    output: Output<'_>,
    disc: u32,
    title: u32,
    cancel: &AtomicBool,
    log: &RunLog,
    emit: &mut impl FnMut(Event),
) -> Result<PathBuf> {
    makemkv::check_cancel(cancel)?;
    let attempt = Attempt::create(output.movie)?;
    let result = (|| {
        log.write("staging", &attempt.path().display().to_string())?;
        emit(Event::Log(format!(
            "Ripping title {title} from disc {disc}"
        )));
        makemkv.rip(disc, title, attempt.path(), cancel, log, emit)?;
        emit(Event::Operation("Finalizing movie file".into()));
        let destination = attempt.publish(
            cancel,
            #[cfg(windows)]
            output.publisher,
        )?;
        log.write("completed", &destination.display().to_string())
            .map_err(|error| {
                format!(
                    "Movie saved at {}, but could not update the run log: {error}",
                    destination.display()
                )
            })?;
        Ok(destination)
    })();
    // MakeMkv::rip stopped the process group and reaped its child before returning.
    let cleanup = attempt.close();
    match (result, cleanup) {
        (Ok(path), Ok(())) => Ok(path),
        (Err(error), Ok(())) => Err(error),
        (Ok(path), Err(error)) => Err(format!("Movie saved at {}; {error}", path.display()).into()),
        (Err(error), Err(cleanup)) => Err(format!("{error}; {cleanup}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_refuses_racing_destination_and_preserves_neighbors() {
        let root = tempfile::tempdir().unwrap();
        let movie = root.path().join("Example (2005)");
        let attempt = Attempt::create(&movie).unwrap();
        let staging = attempt.path().to_owned();
        fs::write(staging.join("title00.mkv"), "new movie").unwrap();
        let final_path = named_mkv(&movie).unwrap();
        fs::write(&final_path, "existing movie").unwrap();
        let extra = movie.join("extras.mkv");
        fs::write(&extra, "extra").unwrap();
        assert!(
            attempt
                .publish(
                    &AtomicBool::new(false),
                    #[cfg(windows)]
                    None
                )
                .is_err()
        );
        attempt.close().unwrap();
        assert!(!staging.exists());
        assert_eq!(fs::read_to_string(final_path).unwrap(), "existing movie");
        assert_eq!(fs::read_to_string(extra).unwrap(), "extra");
    }

    #[test]
    fn only_one_nonempty_regular_mkv_can_be_published() {
        for count in 0..=2 {
            let root = tempfile::tempdir().unwrap();
            let movie = root.path().join("Example (2005)");
            let attempt = Attempt::create(&movie).unwrap();
            for id in 0..count {
                fs::write(attempt.path().join(format!("{id}.MKV")), "movie").unwrap();
            }
            let result = attempt.publish(
                &AtomicBool::new(false),
                #[cfg(windows)]
                None,
            );
            assert_eq!(result.is_ok(), count == 1);
            attempt.close().unwrap();
        }
        let root = tempfile::tempdir().unwrap();
        let movie = root.path().join("Example (2005)");
        let attempt = Attempt::create(&movie).unwrap();
        fs::write(attempt.path().join("empty.mkv"), "").unwrap();
        assert!(
            attempt
                .publish(
                    &AtomicBool::new(false),
                    #[cfg(windows)]
                    None
                )
                .is_err()
        );
        attempt.close().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_and_symlink_output_are_not_published() {
        let root = tempfile::tempdir().unwrap();
        let movie = root.path().join("Example (2005)");
        let attempt = Attempt::create(&movie).unwrap();
        let external = root.path().join("original.mkv");
        fs::write(&external, "original").unwrap();
        std::os::unix::fs::symlink(&external, attempt.path().join("link.mkv")).unwrap();
        assert!(
            attempt
                .publish(
                    &AtomicBool::new(false),
                    #[cfg(windows)]
                    None
                )
                .is_err()
        );
        assert!(
            attempt
                .publish(
                    &AtomicBool::new(true),
                    #[cfg(windows)]
                    None
                )
                .is_err()
        );
        attempt.close().unwrap();
        assert_eq!(fs::read_to_string(external).unwrap(), "original");
    }
}
