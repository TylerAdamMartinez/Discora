use std::{
    collections::VecDeque,
    env,
    fs::File,
    io::{self, BufRead, BufReader, IsTerminal},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossterm::event::{
    self, Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, HorizontalAlignment, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
};
use reqwest::blocking::Client;

use crate::{
    Result,
    makemkv::{Drive, Event, MakeMkv, Title},
    metadata::{Media, movie_destination, search_tmdb},
    rip::{self, RunLog},
};

const LOG_CAPACITY: usize = 2000;
const PAGE_SIZE: usize = 100;

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Search,
    Movies,
    Drives,
    Titles,
    Confirm,
    Busy,
    Done,
    Failed,
}

enum Job {
    Search(String),
    CheckDestination {
        movie: Media,
        destination: PathBuf,
    },
    Drives,
    Titles(u32),
    Rip {
        movie: PathBuf,
        disc: u32,
        title: u32,
    },
}
enum Outcome {
    Movies(Vec<Media>),
    Destination {
        movie: Media,
        destination: PathBuf,
        exists: bool,
    },
    Drives(Vec<Drive>),
    Titles(Vec<Title>),
    Saved(PathBuf),
}
enum Update {
    Event(Event),
    Complete(std::result::Result<Outcome, String>),
    Panicked,
}

struct Worker {
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

struct LogPage {
    offset: usize,
    total: usize,
    lines: Vec<String>,
}

impl LogPage {
    fn read(path: PathBuf, requested: Option<usize>) -> io::Result<Self> {
        let mut lines = VecDeque::new();
        let mut total = 0;
        for line in BufReader::new(File::open(path)?).lines() {
            let line = line?;
            match requested {
                Some(offset) if total >= offset && lines.len() < PAGE_SIZE => lines.push_back(line),
                None => {
                    lines.push_back(line);
                    if lines.len() > PAGE_SIZE {
                        lines.pop_front();
                    }
                }
                _ => {}
            }
            total += 1;
        }
        Ok(Self {
            offset: requested.unwrap_or_else(|| total.saturating_sub(PAGE_SIZE)),
            total,
            lines: lines.into_iter().collect(),
        })
    }
}

struct App {
    phase: Phase,
    query: String,
    filter: String,
    selected: usize,
    movies: Vec<Media>,
    drives: Vec<Drive>,
    titles: Vec<Title>,
    movie: Option<Media>,
    poster: crate::poster::Preview,
    disc: Option<u32>,
    title: Option<u32>,
    forced_disc: Option<u32>,
    root: PathBuf,
    root_label: String,
    storage: crate::storage::Monitor,
    remote: Option<crate::destination::Remote>,
    #[cfg(windows)]
    publisher: Option<crate::windows_remote::Publisher>,
    destination: Option<PathBuf>,
    token: String,
    makemkv: MakeMkv,
    log: RunLog,
    logs: VecDeque<String>,
    log_view: bool,
    log_page: Option<LogPage>,
    log_offset: usize,
    log_worker: Option<JoinHandle<io::Result<LogPage>>>,
    worker: Option<Worker>,
    rx: Option<mpsc::Receiver<Update>>,
    operation: String,
    progress: Option<f64>,
    started: Instant,
    elapsed: Duration,
    status: String,
    canceled: bool,
    quit_when_idle: bool,
    quit: bool,
    saved: Option<PathBuf>,
}

impl Drop for App {
    fn drop(&mut self) {
        // Disconnect first: a worker may be blocked sending its final result.
        self.rx.take();
        self.worker.take();
    }
}

pub fn run(
    disc: Option<u32>,
    destination: crate::destination::Destination,
    executable: Option<PathBuf>,
) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("The rip command needs an interactive terminal".into());
    }
    let token = env::var("TMDB_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .ok_or("Set TMDB_TOKEN first")?;
    let makemkv = MakeMkv::discover(executable.as_deref())?;
    let log = RunLog::create(&rip::log_directory()?)?;
    log.write("session", "Starting interactive movie ripping workflow")?;
    let terminated = Arc::new(AtomicBool::new(false));
    let _signals = crate::platform::Signals::register(terminated.clone())?;
    let mounted = crate::destination::MountedDestination::open(destination, &terminated, &log)
        .inspect_err(|error| {
            let _ = log.write("sshfs", &error.to_string());
            eprintln!("Run log: {}", log.path.display());
        })?;
    let result = run_mounted(disc, &mounted, token, makemkv, log, &terminated);
    let cleanup = mounted.close();
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}; {cleanup}").into()),
    }
}

fn run_mounted(
    disc: Option<u32>,
    mounted: &crate::destination::MountedDestination,
    token: String,
    makemkv: MakeMkv,
    log: RunLog,
    terminated: &Arc<AtomicBool>,
) -> Result<()> {
    let mut app = App::new(disc, mounted.root.clone(), token, makemkv, log);
    app.root_label = mounted.label.clone();
    app.remote = mounted.remote.clone();
    #[cfg(windows)]
    {
        app.publisher = mounted.publisher.clone();
    }
    let result = {
        let _restore = TerminalGuard;
        let mut terminal = ratatui::try_init()?;
        let mut picker = ratatui_image::picker::Picker::from_query_stdio()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());

        // Work around a graphics protocol compatibility issue with WezTerm.
        // ratatui-image auto-detects the Kitty protocol, but rendering through it
        // produces corrupted image placeholders instead of displaying the poster.
        // Force iTerm2, which WezTerm supports, while preserving automatic protocol
        // detection for other terminals.
        if std::env::var("TERM_PROGRAM").as_deref() == Ok("WezTerm") {
            picker.set_protocol_type(ratatui_image::picker::ProtocolType::Iterm2);
        }

        app.poster.start(picker);
        app.event_loop(&mut terminal, terminated)
    };
    // If drawing or input failed, finish child termination/cleanup before returning.
    app.rx.take();
    app.worker.take();
    println!("Run log: {}", app.log.path.display());
    result?;
    if let Some(path) = &app.saved {
        println!("Saved: {}", app.display_path(path));
    } else if app.phase == Phase::Failed || app.canceled {
        return Err(app.status.clone().into());
    }
    Ok(())
}

impl App {
    fn new(disc: Option<u32>, root: PathBuf, token: String, makemkv: MakeMkv, log: RunLog) -> Self {
        Self {
            phase: Phase::Search,
            query: String::new(),
            filter: String::new(),
            selected: 0,
            movies: vec![],
            drives: vec![],
            titles: vec![],
            movie: None,
            poster: crate::poster::Preview::default(),
            disc: None,
            title: None,
            forced_disc: disc,
            root_label: root.display().to_string(),
            storage: crate::storage::Monitor::default(),
            remote: None,
            #[cfg(windows)]
            publisher: None,
            root,
            destination: None,
            token,
            makemkv,
            log,
            logs: VecDeque::new(),
            log_view: false,
            log_page: None,
            log_offset: 0,
            log_worker: None,
            worker: None,
            rx: None,
            operation: String::new(),
            progress: None,
            started: Instant::now(),
            elapsed: Duration::ZERO,
            status: "Search for a movie to begin".into(),
            canceled: false,
            quit_when_idle: false,
            quit: false,
            saved: None,
        }
    }

    fn start(&mut self, job: Job, operation: &str) {
        self.phase = Phase::Busy;
        self.operation = operation.into();
        self.status = operation.into();
        self.progress = None;
        self.started = Instant::now();
        self.elapsed = Duration::ZERO;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let makemkv = self.makemkv.clone();
        let log = self.log.clone();
        let token = self.token.clone();
        #[cfg(windows)]
        let publisher = self.publisher.clone();
        let (tx, rx) = mpsc::sync_channel(256);
        let handle = thread::spawn(move || {
            let mut emit = |event| {
                let _ = tx.try_send(Update::Event(event));
            };
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<Outcome> {
                    match job {
                        Job::Search(query) => {
                            let client =
                                Client::builder().timeout(Duration::from_secs(15)).build()?;
                            let movies = search_tmdb(&client, &token, "movie", &query)?;
                            crate::makemkv::check_cancel(&worker_cancel)?;
                            Ok(Outcome::Movies(
                                movies.into_iter().filter(|m| m.year().is_some()).collect(),
                            ))
                        }
                        Job::CheckDestination { movie, destination } => {
                            let path = crate::files::named_mkv(&destination)?;
                            #[cfg(windows)]
                            let exists = if let Some(publisher) = &publisher {
                                publisher.exists(&path, &worker_cancel)?
                            } else {
                                destination_exists(&path)?
                            };
                            #[cfg(unix)]
                            let exists = destination_exists(&path)?;
                            crate::makemkv::check_cancel(&worker_cancel)?;
                            Ok(Outcome::Destination {
                                movie,
                                destination,
                                exists,
                            })
                        }
                        Job::Drives => Ok(Outcome::Drives(makemkv.drives(
                            &worker_cancel,
                            &log,
                            &mut emit,
                        )?)),
                        Job::Titles(disc) => Ok(Outcome::Titles(makemkv.titles(
                            disc,
                            &worker_cancel,
                            &log,
                            &mut emit,
                        )?)),
                        Job::Rip { movie, disc, title } => {
                            let output = rip::Output::local(&movie);
                            #[cfg(windows)]
                            let output = output.with_publisher(publisher.as_ref());
                            Ok(Outcome::Saved(rip::run(
                                &makemkv,
                                output,
                                disc,
                                title,
                                &worker_cancel,
                                &log,
                                &mut emit,
                            )?))
                        }
                    }
                }));
            let result = match result {
                Ok(result) => result,
                Err(_) => {
                    let _ = log.write("error", "Background task panicked");
                    let _ = tx.send(Update::Panicked);
                    return;
                }
            };
            let result = result.map_err(|error| {
                let error = error.to_string();
                let _ = log.write("error", &error);
                error
            });
            let result = match &result {
                Err(error) => match log.write("error", error) {
                    Ok(()) => result,
                    Err(log_error) => Err(format!("{error}; could not write run log: {log_error}")),
                },
                Ok(_) => result,
            };
            let _ = tx.send(Update::Complete(result));
        });
        self.worker = Some(Worker {
            cancel,
            handle: Some(handle),
        });
        self.rx = Some(rx);
    }

    fn cancel(&mut self, quit_when_idle: bool) {
        self.canceled = true;
        self.quit_when_idle |= quit_when_idle;
        self.status =
            "Canceling — waiting for the active task and cleaning up partial output…".into();
        if let Some(worker) = &self.worker {
            worker.cancel.store(true, Ordering::Relaxed);
        } else {
            self.quit = true;
        }
    }

    fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        terminated: &AtomicBool,
    ) -> Result<()> {
        while !self.quit {
            if terminated.load(Ordering::Relaxed) && !self.quit_when_idle {
                self.cancel(true);
            }
            self.poll();
            if self.quit {
                break;
            }
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(50))?
                && let TerminalEvent::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.key(key);
            }
        }
        Ok(())
    }

    fn poll(&mut self) {
        self.storage.poll(&self.root, Instant::now());
        if self.phase == Phase::Busy {
            self.elapsed = self.started.elapsed();
        }
        // Bound each drain to keep input/drawing responsive under heavy output.
        for _ in 0..256 {
            let update = self.rx.as_ref().map(|rx| rx.try_recv());
            match update {
                Some(Ok(Update::Panicked)) => {
                    self.worker.take();
                    self.rx = None;
                    self.complete(Err("Background task panicked; inspect the run log".into()));
                    // Ratatui's panic hook already restored the terminal. Exit
                    // without drawing into the user's ordinary terminal buffer.
                    self.quit = true;
                    break;
                }
                Some(Ok(Update::Event(event))) => match event {
                    Event::Log(line) => {
                        if self.logs.len() == LOG_CAPACITY {
                            self.logs.pop_front();
                        }
                        self.logs.push_back(line);
                    }
                    Event::Progress { total, max } => {
                        self.progress =
                            (max > 0).then(|| (total as f64 / max as f64).clamp(0.0, 1.0));
                    }
                    Event::Operation(text) => self.operation = text,
                },
                Some(Ok(Update::Complete(result))) => {
                    self.worker.take();
                    self.rx = None;
                    self.filter.clear();
                    self.selected = 0;
                    if self.quit_when_idle {
                        self.quit = true;
                    }
                    self.complete(result);
                    break;
                }
                Some(Err(mpsc::TryRecvError::Disconnected)) => {
                    self.worker.take();
                    self.rx = None;
                    self.complete(Err("Background task ended unexpectedly".into()));
                    break;
                }
                _ => break,
            }
        }
        if self
            .log_worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
        {
            match self.log_worker.take().unwrap().join() {
                Ok(Ok(page)) => {
                    self.log_page = Some(page);
                    self.log_offset = 0;
                }
                Ok(Err(error)) => self.status = format!("Cannot read log: {error}"),
                Err(_) => self.status = "Log reader panicked".into(),
            }
        }
    }

    fn complete(&mut self, result: std::result::Result<Outcome, String>) {
        match result {
            Ok(Outcome::Movies(movies)) => {
                self.movies = movies;
                self.phase = Phase::Movies;
                self.status = if self.movies.is_empty() {
                    "No results. Esc returns to search."
                } else {
                    "Select the correct movie; type to filter"
                }
                .into();
            }
            Ok(Outcome::Destination {
                movie,
                destination,
                exists,
            }) => {
                if exists {
                    self.phase = Phase::Movies;
                    self.status = crate::files::named_mkv(&destination)
                        .map(|path| format!("Already exists: {}", self.display_path(&path)))
                        .unwrap_or_else(|error| error.to_string());
                } else if !self.canceled && !self.quit_when_idle {
                    self.destination = Some(destination);
                    self.movie = Some(movie);
                    if let Some(disc) = self.forced_disc {
                        self.disc = Some(disc);
                        self.start(Job::Titles(disc), "Scanning disc titles…");
                    } else {
                        self.start(Job::Drives, "Discovering optical drives…");
                    }
                }
            }
            Ok(Outcome::Drives(drives)) => {
                self.drives = drives;
                self.phase = Phase::Drives;
                self.status = "Select an optical drive with a disc inserted".into();
            }
            Ok(Outcome::Titles(titles)) => {
                self.titles = titles;
                self.phase = Phase::Titles;
                self.status =
                    "Select one title. Duration alone does not identify the correct movie.".into();
            }
            Ok(Outcome::Saved(path)) => {
                self.saved = Some(path);
                self.phase = Phase::Done;
                self.progress = Some(1.0);
                self.status = "Rip complete; movie file named and partial output cleaned up".into();
            }
            Err(error) => {
                self.phase = Phase::Failed;
                self.status = error;
            }
        }
        if self.canceled && self.saved.is_none() {
            self.phase = Phase::Failed;
        }
    }

    fn choices(&self) -> Vec<(usize, String)> {
        let choices: Vec<String> = match self.phase {
            Phase::Movies => self.movies.iter().map(ToString::to_string).collect(),
            Phase::Drives => self
                .drives
                .iter()
                .map(|d| format!("Drive {} — {} — {}", d.index, d.name, d.disc))
                .collect(),
            Phase::Titles => self.titles.iter().map(ToString::to_string).collect(),
            _ => vec![],
        };
        choices
            .into_iter()
            .enumerate()
            .filter(|(_, s)| matches_filter(s, &self.filter))
            .collect()
    }

    fn key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.cancel(true);
            return;
        }
        if key.code == KeyCode::F(2)
            || (key.code == KeyCode::Char('l')
                && !matches!(
                    self.phase,
                    Phase::Search | Phase::Movies | Phase::Drives | Phase::Titles
                ))
        {
            self.log_view = !self.log_view;
            if self.log_view {
                self.load_log(Some(0));
            }
            return;
        }
        if self.log_view && key.code != KeyCode::Esc {
            match key.code {
                KeyCode::Up => self.log_offset = self.log_offset.saturating_sub(1),
                KeyCode::Down => self.log_offset = (self.log_offset + 1).min(PAGE_SIZE - 1),
                KeyCode::PageDown => {
                    if let Some(page) = &self.log_page
                        && page.offset + PAGE_SIZE < page.total
                    {
                        self.load_log(Some(page.offset + PAGE_SIZE));
                    }
                }
                KeyCode::PageUp => {
                    let offset = self
                        .log_page
                        .as_ref()
                        .map_or(0, |p| p.offset.saturating_sub(PAGE_SIZE));
                    self.load_log(Some(offset));
                }
                KeyCode::Home => self.load_log(Some(0)),
                KeyCode::End => self.load_log(None),
                KeyCode::Char('r') => self.load_log(self.log_page.as_ref().map(|p| p.offset)),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => {
                if self.log_view {
                    self.log_view = false;
                } else if self.phase == Phase::Busy {
                    self.cancel(false);
                } else if self.phase == Phase::Movies {
                    self.phase = Phase::Search;
                    self.filter.clear();
                } else if self.phase == Phase::Confirm {
                    self.phase = Phase::Titles;
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Enter if self.phase == Phase::Search => {
                if !self.query.trim().is_empty() {
                    self.poster.reset();
                    self.start(Job::Search(self.query.trim().to_owned()), "Searching TMDB…");
                }
            }
            KeyCode::Enter if self.phase == Phase::Confirm => {
                if let (Some(movie), Some(disc), Some(title)) =
                    (self.destination.clone(), self.disc, self.title)
                {
                    self.start(Job::Rip { movie, disc, title }, "Opening disc for ripping…");
                }
            }
            KeyCode::Enter if matches!(self.phase, Phase::Done | Phase::Failed) => self.quit = true,
            KeyCode::Enter => self.choose(),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.choices().len().saturating_sub(1))
            }
            KeyCode::Backspace => {
                if self.phase == Phase::Search {
                    self.query.pop();
                } else {
                    self.filter.pop();
                    self.selected = 0;
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if self.phase == Phase::Search {
                    self.query.push(c);
                } else if matches!(self.phase, Phase::Movies | Phase::Drives | Phase::Titles) {
                    self.filter.push(c);
                    self.selected = 0;
                }
            }
            _ => {}
        }
    }

    fn display_path(&self, path: &std::path::Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(relative) if relative.as_os_str().is_empty() => self.root_label.clone(),
            Ok(relative) if self.root_label != self.root.display().to_string() => format!(
                "{}/{}",
                self.root_label.trim_end_matches('/'),
                relative.to_string_lossy().replace('\\', "/")
            ),
            _ => path.display().to_string(),
        }
    }

    fn choose(&mut self) {
        let Some((index, _)) = self.choices().get(self.selected).cloned() else {
            return;
        };
        match self.phase {
            Phase::Movies => {
                let movie = self.movies[index].clone();
                match movie_destination(&self.root, &movie) {
                    Ok(destination) => {
                        self.start(
                            Job::CheckDestination { movie, destination },
                            "Checking destination…",
                        );
                    }
                    Err(error) => {
                        self.status = error.to_string();
                    }
                }
            }
            Phase::Drives => {
                let disc = self.drives[index].index;
                self.disc = Some(disc);
                self.start(Job::Titles(disc), "Scanning disc titles…");
            }
            Phase::Titles => {
                self.title = Some(self.titles[index].id);
                self.phase = Phase::Confirm;
                self.status = "Enter starts ripping with MakeMKV's current audio/subtitle preferences. Esc chooses another title.".into();
            }
            _ => {}
        }
    }

    fn load_log(&mut self, offset: Option<usize>) {
        if self.log_worker.is_none() {
            let path = self.log.path.clone();
            self.log_worker = Some(thread::spawn(move || LogPage::read(path, offset)));
        }
    }

    fn draw_movie_preview(
        &mut self,
        frame: &mut Frame,
        area: ratatui::layout::Rect,
        movie: Option<&Media>,
    ) {
        let block = Block::bordered().title("Movie preview");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(movie) = movie else {
            frame.render_widget(Paragraph::new("No matching movies"), inner);
            return;
        };
        let [title, poster, synopsis] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Percentage(65),
            Constraint::Min(3),
        ])
        .areas(inner);
        frame.render_widget(
            Paragraph::new(safe_text(&movie.to_string())).wrap(Wrap { trim: true }),
            title,
        );
        self.poster.draw(frame, poster, movie.poster_path());
        frame.render_widget(
            Paragraph::new(safe_text(movie.overview())).wrap(Wrap { trim: true }),
            synopsis,
        );
    }

    fn draw(&mut self, frame: &mut Frame) {
        let browsing_movies = self.phase == Phase::Movies && !self.log_view;
        let [header, context, content, progress, logs, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(if self.log_view { 0 } else { 5 }),
            Constraint::Length(if browsing_movies { 0 } else { 3 }),
            if self.log_view {
                Constraint::Percentage(65)
            } else {
                Constraint::Length(if browsing_movies { 0 } else { 8 })
            },
            Constraint::Length(3),
        ])
        .areas(frame.area());
        frame.render_widget(
            Paragraph::new("Discora")
                .alignment(HorizontalAlignment::Center)
                .style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .block(Block::bordered()),
            header,
        );
        let version = concat!("v", env!("CARGO_PKG_VERSION"));
        let header_inner = Block::bordered().inner(header);
        let version_width = version.len() as u16;
        // Reserve the centered title plus a one-column gap on either side.
        let storage_width = header_inner.width.saturating_sub(7) / 2;
        let storage_width = storage_width.saturating_sub(1);
        if header_inner.height > 0
            && let Some(text) = self.storage.label(storage_width)
        {
            frame.render_widget(
                Paragraph::new(text).style(Style::default().fg(Color::Cyan)),
                ratatui::layout::Rect::new(header_inner.x, header_inner.y, storage_width, 1),
            );
        }

        // Keep the title centered, hiding the version if the two would overlap.
        if header_inner.height > 0 && header_inner.width >= version_width * 2 + 9 {
            frame.render_widget(
                Paragraph::new(version).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                ratatui::layout::Rect::new(
                    header_inner.right() - version_width,
                    header_inner.y,
                    version_width,
                    1,
                ),
            );
        }
        let movie = self
            .movie
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "No movie selected".into());
        let path = self.destination.as_ref().unwrap_or(&self.root);
        let (mode, destination) = match &self.remote {
            Some(remote) => (
                format!(
                    "SSHFS ({}@{}, port {})",
                    remote.username, remote.host, remote.port
                ),
                path.strip_prefix(&self.root)
                    .map(|relative| {
                        if relative.as_os_str().is_empty() {
                            remote.movies_path.clone()
                        } else {
                            format!(
                                "{}/{}",
                                remote.movies_path.trim_end_matches('/'),
                                relative.to_string_lossy().replace('\\', "/")
                            )
                        }
                    })
                    .unwrap_or_else(|_| path.display().to_string()),
            ),
            None => ("Local".into(), self.display_path(path)),
        };
        frame.render_widget(
            Paragraph::new(safe_text(&format!("{movie} -> {mode} {destination}"))),
            context,
        );
        if self.log_view {
            frame.render_widget(
                Paragraph::new("Full log view — F2 returns to workflow"),
                content,
            );
        } else if self.phase == Phase::Search {
            frame.render_widget(
                Paragraph::new(safe_text(&format!("{}▏", self.query)))
                    .block(Block::bordered().title("Search TMDB — Enter to search")),
                content,
            );
        } else if matches!(self.phase, Phase::Movies | Phase::Drives | Phase::Titles) {
            let choices = self.choices();
            let list_area = if self.phase == Phase::Movies
                && content.width >= 100
                && content.height >= 12
            {
                let [list, preview] =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .areas(content);
                let movie = choices
                    .get(self.selected)
                    .map(|(index, _)| self.movies[*index].clone());
                self.draw_movie_preview(frame, preview, movie.as_ref());
                list
            } else {
                content
            };
            let items: Vec<ListItem> = choices
                .iter()
                .map(|(_, s)| {
                    let text = safe_text(s);
                    if self.phase == Phase::Titles {
                        // Account for borders and the selection marker.
                        let width = content.width.saturating_sub(4).max(1) as usize;
                        let lines = text
                            .lines()
                            .flat_map(|line| {
                                line.chars()
                                    .collect::<Vec<_>>()
                                    .chunks(width)
                                    .map(|chunk| chunk.iter().collect::<String>())
                                    .collect::<Vec<_>>()
                            })
                            .collect::<Vec<_>>();
                        ListItem::new(lines.join("\n"))
                    } else {
                        ListItem::new(text)
                    }
                })
                .collect();
            let title = match self.phase {
                Phase::Movies => "Movie",
                Phase::Drives => "Drive",
                _ => "Title (longest first)",
            };
            let list = List::new(items)
                .block(
                    Block::bordered()
                        .title(format!("{title} | Filter: {}", safe_text(&self.filter))),
                )
                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
                .highlight_symbol("› ");
            let mut state =
                ListState::default().with_selected((!choices.is_empty()).then_some(self.selected));
            frame.render_stateful_widget(list, list_area, &mut state);
        } else {
            let text = match self.phase {
                Phase::Confirm => {
                    let title = self
                        .titles
                        .iter()
                        .find(|t| Some(t.id) == self.title)
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    format!(
                        "Ready to rip\nDisc: {}\n{title}\nOutput: {}\nOne movie title; MakeMKV default audio/subtitle tracks.\nExisting files will not be replaced.",
                        self.disc.unwrap_or(0),
                        self.destination
                            .as_ref()
                            .and_then(|d| crate::files::named_mkv(d).ok())
                            .map(|p| self.display_path(&p))
                            .unwrap_or_default()
                    )
                }
                Phase::Busy => format!(
                    "{} {}\n{}",
                    ["◐", "◓", "◑", "◒"][(self.started.elapsed().as_millis() / 150 % 4) as usize],
                    safe_text(&self.operation),
                    self.status
                ),
                Phase::Done => format!(
                    "Saved: {}",
                    self.saved
                        .as_ref()
                        .map(|p| self.display_path(p))
                        .unwrap_or_default()
                ),
                _ => self.status.clone(),
            };
            frame.render_widget(
                Paragraph::new(safe_text(&text))
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().title("Workflow")),
                content,
            );
        }
        let elapsed = self.elapsed.as_secs();
        let remaining = self
            .progress
            .filter(|p| *p > 0.0 && *p <= 1.0)
            .filter(|p| elapsed >= 2 || *p == 1.0)
            .map(|p| duration((elapsed as f64 * (1.0 - p) / p) as u64))
            .unwrap_or_else(|| "-:--:--".to_string());
        let percentage = self
            .progress
            .map(|p| format!("{:>6.2}%", p * 100.0))
            .unwrap_or_else(|| " --.--%".to_string());
        let progress_label = format!(
            "{percentage} | Elapsed {} | Remaining {remaining}",
            duration(elapsed)
        );
        let progress_block = Block::bordered().title(format!("Progress {progress_label}"));
        let bar = progress_block.inner(progress);
        frame.render_widget(progress_block, progress);
        let filled_width =
            (f64::from(bar.width) * self.progress.unwrap_or(0.0).clamp(0.0, 1.0)).round() as u16;
        // Draw every filled cell alike, without the gauge's special center-label cell.
        for y in bar.top()..bar.bottom() {
            for x in bar.left()..bar.left() + filled_width {
                frame.buffer_mut()[(x, y)]
                    .set_symbol("█")
                    .set_style(Style::default().fg(Color::Cyan));
            }
        }
        let (title, text) = if self.log_view {
            match &self.log_page {
                Some(page) => (
                    format!(
                        "Full log | lines {}–{} of {} | ↑↓ PgUp/PgDn Home/End R refresh",
                        page.offset + 1,
                        page.offset + page.lines.len(),
                        page.total
                    ),
                    page.lines
                        .iter()
                        .skip(self.log_offset)
                        .map(|s| safe_text(s))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                None => ("Loading full log…".into(), String::new()),
            }
        } else {
            let height = logs.height.saturating_sub(2) as usize;
            (
                "Live logs | F2 full log".into(),
                self.logs
                    .iter()
                    .skip(self.logs.len().saturating_sub(height))
                    .map(|s| safe_text(s))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        };
        frame.render_widget(
            Paragraph::new(text).block(Block::bordered().title(title)),
            logs,
        );
        let help = if self.log_view {
            "F2/Esc: close logs · Ctrl+C: cancel and exit"
        } else if self.phase == Phase::Busy {
            "Esc: cancel · L/F2: full log · Ctrl+C: cancel and exit"
        } else {
            "↑↓: select · Enter: continue · Esc: back/exit · F2: full log · Ctrl+C: exit"
        };
        frame.render_widget(
            Paragraph::new(safe_text(&format!(
                "{help}\n{} | Log: {}",
                self.status,
                self.log.path.display()
            ))),
            footer,
        );
    }
}

fn duration(seconds: u64) -> String {
    format!(
        "{}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}
fn safe_text(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect()
}

fn destination_exists(path: &std::path::Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn matches_filter(text: &str, filter: &str) -> bool {
    let lower = text.to_lowercase();
    let mut chars = lower.chars();
    filter
        .to_lowercase()
        .chars()
        .all(|wanted| chars.by_ref().any(|c| c == wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movie_preview_tracks_filtered_selection_and_handles_small_windows() {
        let temp = tempfile::tempdir().unwrap();
        let log = RunLog::create(temp.path()).unwrap();
        let mut app = App::new(
            None,
            temp.path().into(),
            String::new(),
            MakeMkv::new("/nonexistent".into()),
            log,
        );
        app.phase = Phase::Movies;
        app.movies = serde_json::from_str(
            r#"[
            {"id":1,"title":"Alpha","release_date":"2000-01-01","overview":"First synopsis"},
            {"id":2,"title":"Beta","release_date":"2001-01-01","overview":"Second synopsis"}
        ]"#,
        )
        .unwrap();
        fn screen(app: &mut App, width: u16, height: u16) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect()
        }
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("Movie preview"));
        assert!(text.contains("First synopsis"));
        assert!(text.contains("No poster available"));
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("Second synopsis"));
        assert!(!text.contains("First synopsis"));
        app.filter = "Beta".into();
        app.selected = 0;
        assert!(screen(&mut app, 120, 30).contains("Second synopsis"));
        app.filter = "No match".into();
        assert!(screen(&mut app, 120, 30).contains("No matching movies"));
        app.filter.clear();
        for (width, height) in [(99, 30), (120, 18), (10, 5), (1, 1)] {
            assert!(!screen(&mut app, width, height).contains("Movie preview"));
        }
        app.log_view = true;
        assert!(!screen(&mut app, 120, 30).contains("Movie preview"));
        app.log_view = false;
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!screen(&mut app, 120, 30).contains("Movie preview"));
    }

    #[test]
    fn storage_header_keeps_title_centered_and_compacts_without_overlap() {
        let temp = tempfile::tempdir().unwrap();
        let log = RunLog::create(temp.path()).unwrap();
        let mut app = App::new(
            None,
            temp.path().to_owned(),
            String::new(),
            MakeMkv::new("/nonexistent".into()),
            log,
        );
        app.storage = crate::storage::Monitor::with_usage(2_000_000_000_000, 12_000_000_000_000);
        for log_view in [false, true] {
            app.log_view = log_view;
            for (width, expected) in [
                (100, Some("Used 2 TB out of 12 TB (16.67%)")),
                (60, Some("2 / 12 TB (16.67%)")),
                (28, None),
            ] {
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
                terminal.draw(|frame| app.draw(frame)).unwrap();
                let buffer = terminal.backend().buffer();
                let header: String = (0..width).map(|x| buffer[(x, 1)].symbol()).collect();
                let title_column = (0..width).find(|x| buffer[(*x, 1)].symbol() == "D");
                assert_eq!(title_column, Some(1 + (width - 9).div_ceil(2)));
                assert!(header.contains(concat!("v", env!("CARGO_PKG_VERSION"))));
                if let Some(expected) = expected {
                    assert!(header.starts_with(&format!("│{expected}")), "{header}");
                } else {
                    assert!(!header.contains("TB"));
                }
            }
        }
        app.storage = crate::storage::Monitor::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let header: String = (0..100)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect();
        assert!(header.contains("Storage: checking…"));
    }

    #[test]
    fn movie_header_shows_destination_on_one_row_without_a_gap() {
        let temp = tempfile::tempdir().unwrap();
        let log = RunLog::create(temp.path()).unwrap();
        let mut app = App::new(
            None,
            PathBuf::from("/Movies"),
            String::new(),
            MakeMkv::new("/nonexistent".into()),
            log,
        );
        app.phase = Phase::Busy;
        for port in [None, Some(22), Some(2222)] {
            app.remote = port.map(|port| crate::destination::Remote {
                host: "server".into(),
                port,
                username: "friend".into(),
                movies_path: "/media/Movies/".into(),
                identity_file: Some("/home/friend/.ssh/id_ed25519".into()),
            });
            for selected in [false, true] {
                app.movie = selected.then(|| {
                    serde_json::from_str(
                        r#"{"id":436270,"title":"Black Adam","release_date":"2022-01-01"}"#,
                    )
                    .unwrap()
                });
                app.destination = selected.then(|| app.root.join("Black Adam (2022)"));
                let movie = if selected {
                    "Black Adam (2022) [TMDB: 436270]"
                } else {
                    "No movie selected"
                };
                let mode = port
                    .map(|port| format!("SSHFS (friend@server, port {port})"))
                    .unwrap_or_else(|| "Local".into());
                let root = if port.is_some() {
                    "/media/Movies"
                } else {
                    "/Movies"
                };
                let path = if selected {
                    format!("{root}/Black Adam (2022)")
                } else if port.is_some() {
                    format!("{root}/")
                } else {
                    root.into()
                };
                let path = if port.is_none() {
                    app.display_path(app.destination.as_ref().unwrap_or(&app.root))
                } else {
                    path
                };
                let expected = format!("{movie} -> {mode} {path}");
                for width in [220, 40] {
                    let mut terminal =
                        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24))
                            .unwrap();
                    terminal.draw(|frame| app.draw(frame)).unwrap();
                    let buffer = terminal.backend().buffer();
                    let row = |y| {
                        (0..width)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                    };
                    assert_eq!(
                        row(3).trim_end(),
                        expected.chars().take(width as usize).collect::<String>()
                    );
                    assert!(row(4).contains("Workflow"));
                }
            }
        }
    }

    fn wait_for_task(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.worker.is_some() {
            assert!(
                Instant::now() < deadline,
                "task did not finish: {}",
                app.status
            );
            app.poll();
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn existing_remote_movie_is_rejected_before_drive_discovery() {
        let root = tempfile::tempdir().unwrap();
        let log = RunLog::create(root.path()).unwrap();
        let mut app = App::new(
            None,
            root.path().join("mounted"),
            "unused-test-token".into(),
            MakeMkv::new("/nonexistent".into()),
            log,
        );
        app.root_label = "friend@server:/Movies".into();
        let movie: Media =
            serde_json::from_str(r#"{"id":1,"title":"Example","release_date":"2005-01-01"}"#)
                .unwrap();
        let folder = movie_destination(&app.root, &movie).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(crate::files::named_mkv(&folder).unwrap(), "existing movie").unwrap();
        app.complete(Ok(Outcome::Movies(vec![movie])));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        wait_for_task(&mut app);
        assert!(app.phase == Phase::Movies);
        assert!(app.worker.is_none());
        assert!(
            app.status.contains(
                "Already exists: friend@server:/Movies/Example (2005)/Example (2005).mkv"
            )
        );
        assert_eq!(app.display_path(&app.root), "friend@server:/Movies");
    }

    #[test]
    fn workflow_requires_confirmation_and_rips_the_explicitly_selected_title() {
        use std::fs;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("makemkvcon");
        fs::write(
            &executable,
            r#"import sys
from pathlib import Path
if 'disc:9999' in sys.argv:
    print('DRV:2,2,999,1,"Drive","DISC","/dev/sr2"')
elif 'info' in sys.argv:
    print('TINFO:7,9,0,"2:00:00"')
    print('TINFO:1,9,0,"1:30:00"')
else:
    assert sys.argv[-2] == '1'
    assert 'disc:2' in sys.argv
    Path(sys.argv[-1], 'selected.mkv').write_text('selected movie')
    print('PRGV:1,1,0')
    print('PRGV:65536,65536,65536')
"#,
        )
        .unwrap();
        let log = RunLog::create(root.path()).unwrap();
        let mut app = App::new(
            None,
            root.path().join("Movies"),
            "unused-test-token".into(),
            MakeMkv::with_arguments(
                crate::test_support::python(),
                vec![executable.into_os_string()],
            ),
            log,
        );
        let movie: Media =
            serde_json::from_str(r#"{"id":1,"title":"Example","release_date":"2005-01-01"}"#)
                .unwrap();
        app.complete(Ok(Outcome::Movies(vec![movie])));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.key(enter);
        wait_for_task(&mut app);
        assert!(app.phase == Phase::Drives, "{}", app.status);
        app.key(enter);
        wait_for_task(&mut app);
        assert!(app.phase == Phase::Titles, "{}", app.status);
        assert_eq!(app.titles[0].id, 7);
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.key(enter);
        assert!(app.phase == Phase::Confirm);
        assert_eq!(app.title, Some(1));
        assert!(!app.destination.as_ref().unwrap().exists());
        // Confirm and log screens must also render in small/resized terminals.
        for (width, height) in [(110, 30), (80, 24), (40, 12)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
        }
        app.key(enter);
        wait_for_task(&mut app);
        assert!(app.phase == Phase::Done, "{}", app.status);
        assert_eq!(
            fs::read_to_string(app.saved.as_ref().unwrap()).unwrap(),
            "selected movie"
        );
        assert!(
            app.saved
                .as_ref()
                .unwrap()
                .ends_with("Example (2005)/Example (2005).mkv")
        );
    }

    #[test]
    fn teardown_disconnects_a_full_event_queue_before_joining_worker() {
        let root = tempfile::tempdir().unwrap();
        let log = RunLog::create(root.path()).unwrap();
        let mut app = App::new(
            None,
            root.path().to_owned(),
            String::new(),
            MakeMkv::new("/nonexistent".into()),
            log,
        );
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Update::Event(Event::Log("fills queue".into())))
            .unwrap();
        let handle = thread::spawn(move || {
            let _ = tx.send(Update::Complete(Err("canceled".into())));
        });
        app.rx = Some(rx);
        app.worker = Some(Worker {
            cancel: Arc::new(AtomicBool::new(false)),
            handle: Some(handle),
        });
        drop(app);
    }

    #[test]
    fn log_pages_cover_full_file_with_bounded_memory() {
        let root = tempfile::tempdir().unwrap();
        let log = RunLog::create(root.path()).unwrap();
        for i in 0..250 {
            log.write("test", &format!("line {i}")).unwrap();
        }
        let first = LogPage::read(log.path.clone(), Some(0)).unwrap();
        let next = LogPage::read(log.path.clone(), Some(100)).unwrap();
        let tail = LogPage::read(log.path.clone(), None).unwrap();
        assert_eq!(first.lines.len(), PAGE_SIZE);
        assert!(first.lines[0].ends_with("line 0"));
        assert!(next.lines[0].ends_with("line 100"));
        assert_eq!(tail.offset, 150);
        assert!(tail.lines.last().unwrap().ends_with("line 249"));
    }
}
