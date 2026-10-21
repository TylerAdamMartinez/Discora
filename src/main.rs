mod config;
mod destination;
mod files;
mod makemkv;
mod metadata;
mod platform;
mod process;
mod rip;
#[cfg(any(windows, test))]
mod sftp;
#[cfg(test)]
mod test_support;
mod tui;
#[cfg(any(windows, test))]
mod windows_remote;

use clap::{Parser, Subcommand};
use files::{file_cleanup, library_cleanup};
use inquire::{Select, Text};
use metadata::{movie_destination, search_tmdb};
use reqwest::blocking::Client;
use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Parser)]
#[command(
    name = "discora",
    about = "Movie directories, headless disc ripping, and file cleanup",
    version
)]
struct Cli {
    /// Movie library root (overrides discora.toml)
    #[arg(long, global = true, value_parser = config::parse_path)]
    movies_path: Option<PathBuf>,
    /// TV show library root (overrides discora.toml)
    #[arg(long, global = true, value_parser = config::parse_path)]
    tv_shows_path: Option<PathBuf>,
    /// Explicit MakeMKV console executable (otherwise discovered automatically)
    #[arg(long, global = true)]
    makemkv_path: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Rename single MKVs in immediate child directories to match their folder names
    FileCleanup {
        #[arg(value_parser = config::parse_path, conflicts_with_all = ["movies_path", "tv_shows_path"])]
        path: Option<PathBuf>,
    },
    /// Search TMDB and create a movie or TV directory
    Mkdir {
        #[arg(value_parser = config::parse_path, conflicts_with = "movies_path")]
        movies_root: Option<PathBuf>,
        #[arg(value_parser = config::parse_path, conflicts_with = "tv_shows_path")]
        tv_root: Option<PathBuf>,
    },
    /// Interactively select a movie and disc title, then rip with MakeMKV
    Rip {
        #[arg(long)]
        disc: Option<u32>,
    },
}

impl Cli {
    fn checked(self) -> std::result::Result<Self, clap::Error> {
        // Clap does not check subcommand conflicts against global flags placed
        // before the subcommand. Check again after global values propagate.
        let conflict = match &self.command {
            Some(Commands::FileCleanup { path: Some(_) }) => {
                self.movies_path.is_some() || self.tv_shows_path.is_some()
            }
            Some(Commands::Mkdir {
                movies_root,
                tv_root,
            }) => {
                (movies_root.is_some() && self.movies_path.is_some())
                    || (tv_root.is_some() && self.tv_shows_path.is_some())
            }
            _ => false,
        };
        if conflict {
            return Err(clap::Error::raw(
                clap::error::ErrorKind::ArgumentConflict,
                "positional library paths cannot be combined with corresponding library flags",
            ));
        }
        Ok(self)
    }
}

fn mkdir(movies_root: &Path, tv_root: &Path) -> Result<()> {
    let token = env::var("TMDB_TOKEN").map_err(|_| "Set TMDB_TOKEN first")?;

    let media_type = Select::new("Choose media type:", vec!["Movie", "TV Show"]).prompt()?;

    let endpoint = if media_type == "Movie" { "movie" } else { "tv" };

    let root = if media_type == "Movie" {
        movies_root
    } else {
        tv_root
    };

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    loop {
        let query = Text::new("Search title:").prompt()?;

        if query.trim().is_empty() {
            println!("Please enter a title.");
            continue;
        }

        let results = search_tmdb(&client, &token, endpoint, query.trim())?;

        // Only show entries with a valid year.
        let results: Vec<_> = results.into_iter().filter(|m| m.year().is_some()).collect();

        if results.is_empty() {
            println!("No results. Try another search.");
            continue;
        }

        let selected = Select::new("Select the correct title (type to filter):", results)
            .with_page_size(10)
            .prompt()?;

        let destination = movie_destination(root, &selected)?;

        if destination.exists() {
            println!("Already exists: {}", destination.display());
            return Ok(());
        }

        fs::create_dir_all(&destination)?;

        println!("\nCreated: {}\n", destination.display());

        println!("Select this directory as your MakeMKV output.");

        return Ok(());
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse().checked().unwrap_or_else(|error| error.exit());
    let mut overrides = config::LibraryPaths {
        movies_path: cli.movies_path,
        tv_shows_path: cli.tv_shows_path,
        ..Default::default()
    };
    match cli.command.unwrap_or(Commands::Rip { disc: None }) {
        Commands::FileCleanup { path: Some(path) } => file_cleanup(&path),
        Commands::FileCleanup { path: None } => {
            let paths = config::resolve(overrides, true)?;
            let movies = paths.movies_path.as_deref().expect("resolved movie path");
            let tv = paths.tv_shows_path.as_deref().expect("resolved TV path");
            library_cleanup(movies, tv)
        }
        Commands::Mkdir {
            movies_root,
            tv_root,
        } => {
            overrides.movies_path = overrides.movies_path.or(movies_root);
            overrides.tv_shows_path = overrides.tv_shows_path.or(tv_root);
            let paths = config::resolve(overrides, true)?;
            mkdir(
                paths.movies_path.as_deref().expect("resolved movie path"),
                paths.tv_shows_path.as_deref().expect("resolved TV path"),
            )
        }
        Commands::Rip { disc } => tui::run(disc, config::resolve_rip(overrides)?, cli.makemkv_path),
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn flags_are_global_and_positional_paths_remain_supported() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_str().unwrap();
        for args in [
            vec!["discora", "--movies-path", path],
            vec!["discora", "--movies-path", path, "rip"],
            vec!["discora", "rip", "--movies-path", path],
            vec![
                "discora",
                "mkdir",
                "--movies-path",
                path,
                "--tv-shows-path",
                path,
            ],
            vec![
                "discora",
                "file-cleanup",
                "--movies-path",
                path,
                "--tv-shows-path",
                path,
            ],
            vec!["discora", "mkdir", path, path],
            vec!["discora", "file-cleanup", path],
        ] {
            Cli::try_parse_from(args).and_then(Cli::checked).unwrap();
        }
    }

    #[test]
    fn makemkv_override_is_global_and_preserves_spaces() {
        for args in [
            vec![
                "discora",
                "--makemkv-path",
                "custom folder/makemkvcon.exe",
                "rip",
            ],
            vec![
                "discora",
                "rip",
                "--makemkv-path",
                "custom folder/makemkvcon.exe",
            ],
            vec!["discora", "--makemkv-path", "custom folder/makemkvcon.exe"],
        ] {
            assert_eq!(
                Cli::try_parse_from(args).unwrap().makemkv_path,
                Some(PathBuf::from("custom folder/makemkvcon.exe"))
            );
        }
    }

    #[test]
    fn conflicting_and_relative_paths_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_str().unwrap();
        for args in [
            vec!["discora", "mkdir", path, "--movies-path", path],
            vec!["discora", "mkdir", path, path, "--tv-shows-path", path],
            vec!["discora", "file-cleanup", path, "--movies-path", path],
            vec!["discora", "--tv-shows-path", path, "file-cleanup", path],
            vec!["discora", "--movies-path", "relative"],
        ] {
            assert!(
                Cli::try_parse_from(&args).and_then(Cli::checked).is_err(),
                "accepted conflicting arguments: {args:?}"
            );
        }
    }
}
