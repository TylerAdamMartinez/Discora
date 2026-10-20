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
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};
use reqwest::blocking::Client;
use signal_hook::{
    SigId,
    consts::{SIGINT, SIGTERM},
};

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

struct Signals(Vec<SigId>);
impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.0.drain(..) {
            signal_hook::low_level::unregister(id);
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
    disc: Option<u32>,
    title: Option<u32>,
    forced_disc: Option<u32>,
    root: PathBuf,
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

pub fn run(disc: Option<u32>, root: PathBuf) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("The rip command needs an interactive terminal".into());
    }
    let token = env::var("TMDB_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .ok_or("Set TMDB_TOKEN first")?;
    let makemkv = MakeMkv::installed()?;
    let log = RunLog::create(&rip::log_directory()?)?;
    log.write("session", "Starting interactive movie ripping workflow")?;
    let terminated = Arc::new(AtomicBool::new(false));
    let mut signals = Signals(Vec::new());
    for signal in [SIGINT, SIGTERM] {
        signals
            .0
            .push(signal_hook::flag::register(signal, terminated.clone())?);
    }
    let mut app = App::new(disc, root, token, makemkv, log);
    let result = {
        let _restore = TerminalGuard;
        let mut terminal = ratatui::try_init()?;
        app.event_loop(&mut terminal, &terminated)
    };
    // If drawing or input failed, finish child termination/cleanup before returning.
    app.rx.take();
    app.worker.take();
    println!("Run log: {}", app.log.path.display());
    result?;
    if let Some(path) = &app.saved {
        println!("Saved: {}", path.display());
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
            disc: None,
            title: None,
            forced_disc: disc,
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
                        Job::Rip { movie, disc, title } => Ok(Outcome::Saved(rip::run(
                            &makemkv,
                            &movie,
                            disc,
                            title,
                            &worker_cancel,
                            &log,
                            &mut emit,
                        )?)),
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
            let result = result.map_err(|e| e.to_string());
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

    fn choose(&mut self) {
        let Some((index, _)) = self.choices().get(self.selected).cloned() else {
            return;
        };
        match self.phase {
            Phase::Movies => {
                let movie = self.movies[index].clone();
                match movie_destination(&self.root, &movie) {
                    Ok(destination) => {
                        self.destination = Some(destination);
                        self.movie = Some(movie);
                    }
                    Err(error) => {
                        self.status = error.to_string();
                        return;
                    }
                }
                if let Some(disc) = self.forced_disc {
                    self.disc = Some(disc);
                    self.start(Job::Titles(disc), "Scanning disc titles…");
                } else {
                    self.start(Job::Drives, "Discovering optical drives…");
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

    fn draw(&self, frame: &mut Frame) {
        let [header, context, content, progress, logs, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(if self.log_view { 0 } else { 5 }),
            Constraint::Length(3),
            if self.log_view {
                Constraint::Percentage(65)
            } else {
                Constraint::Length(8)
            },
            Constraint::Length(3),
        ])
        .areas(frame.area());
        frame.render_widget(
            Paragraph::new("Storage Utils — Movie Ripper")
                .style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .block(Block::bordered()),
            header,
        );
        let movie = self
            .movie
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "No movie selected".into());
        let destination = self
            .destination
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| self.root.display().to_string());
        frame.render_widget(
            Paragraph::new(safe_text(&format!("{movie}\n{destination}"))),
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
            frame.render_stateful_widget(list, content, &mut state);
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
                            .map(|p| p.display().to_string())
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
                        .map(|p| p.display().to_string())
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
        let eta = self
            .progress
            .filter(|p| *p > 0.0 && *p <= 1.0)
            .filter(|p| elapsed >= 2 || *p == 1.0)
            .map(|p| duration((elapsed as f64 * (1.0 - p) / p) as u64))
            .unwrap_or_else(|| "—".into());
        let label = self
            .progress
            .map(|p| {
                format!(
                    "{:.1}% | Elapsed {} | Remaining {eta}",
                    p * 100.0,
                    duration(elapsed)
                )
            })
            .unwrap_or_else(|| format!("—% | Elapsed {} | Remaining —", duration(elapsed)));
        frame.render_widget(
            Gauge::default()
                .block(Block::bordered().title(format!("Overall progress | {label}")))
                .gauge_style(Style::default().fg(Color::Cyan))
                .ratio(self.progress.unwrap_or(0.0)),
            progress,
        );
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
    fn workflow_requires_confirmation_and_rips_the_explicitly_selected_title() {
        use std::fs;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("makemkvcon");
        fs::write(
            &executable,
            r#"#!/usr/bin/python3
import sys
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
            MakeMkv::with_arguments("/usr/bin/python3".into(), vec![executable.into_os_string()]),
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
