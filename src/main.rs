mod files;
mod makemkv;
mod metadata;
mod rip;
mod tui;

use clap::{Parser, Subcommand};
use files::file_cleanup;
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
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Rename single MKVs in immediate child directories to match their folder names
    FileCleanup { path: PathBuf },
    /// Search TMDB and create a movie or TV directory
    Mkdir {
        #[arg(default_value = "/mnt/storage/Movies")]
        movies_path: PathBuf,
        #[arg(default_value = "/mnt/storage/TV Shows")]
        tv_path: PathBuf,
    },
    /// Interactively select a movie and disc title, then rip with MakeMKV
    Rip {
        #[arg(long)]
        disc: Option<u32>,
        #[arg(long, default_value = "/mnt/storage/Movies")]
        movies_path: PathBuf,
    },
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
    match Cli::parse().command.unwrap_or_else(|| Commands::Rip {
        disc: None,
        movies_path: PathBuf::from("/mnt/storage/Movies"),
    }) {
        Commands::FileCleanup { path } => file_cleanup(&path),
        Commands::Mkdir {
            movies_path,
            tv_path,
        } => mkdir(&movies_path, &tv_path),
        Commands::Rip { disc, movies_path } => tui::run(disc, movies_path),
    }
}
