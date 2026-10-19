use std::{
    env,
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use inquire::{Select, Text};
use reqwest::blocking::Client;
use serde::Deserialize;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug, Deserialize)]
struct SearchResponse {
    results: Vec<Media>,
}

#[derive(Debug, Clone, Deserialize)]
struct Media {
    id: u64,
    title: Option<String>,
    name: Option<String>,
    release_date: Option<String>,
    first_air_date: Option<String>,
}

impl Media {
    fn title(&self) -> &str {
        self.title
            .as_deref()
            .or(self.name.as_deref())
            .unwrap_or("Unknown")
    }

    fn year(&self) -> Option<&str> {
        let date = self
            .release_date
            .as_deref()
            .or(self.first_air_date.as_deref())?;

        let year = date.get(..4)?;

        if year.chars().all(|c| c.is_ascii_digit()) {
            Some(year)
        } else {
            None
        }
    }
}

impl fmt::Display for Media {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}) [TMDB: {}]",
            self.title(),
            self.year().unwrap_or("Unknown"),
            self.id
        )
    }
}

fn clean_name(name: &str) -> String {
    name.replace(['\n', '\r'], " ")
}

fn file_cleanup(root: &Path) -> Result<()> {
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", root.display()).into());
    }

    for entry in fs::read_dir(root)? {
        let folder = entry?.path();

        if !folder.is_dir() {
            continue;
        }

        let mkvs: Vec<_> = fs::read_dir(&folder)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("mkv"))
            })
            .collect();

        if mkvs.len() != 1 {
            continue;
        }

        let Some(folder_name) = folder.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        let new_file = folder.join(format!("{}.mkv", clean_name(folder_name)));

        let old_file = &mkvs[0];

        if *old_file == new_file {
            continue;
        }

        if new_file.exists() {
            eprintln!("Skipping existing file: {}", new_file.display());
            continue;
        }

        fs::rename(old_file, &new_file)?;

        println!("{} -> {}", old_file.display(), new_file.display());
    }

    Ok(())
}

fn search_tmdb(client: &Client, token: &str, media_type: &str, query: &str) -> Result<Vec<Media>> {
    let url = format!("https://api.themoviedb.org/3/search/{media_type}");

    let response = client
        .get(url)
        .bearer_auth(token)
        .query(&[("query", query), ("language", "en-US")])
        .send()?
        .error_for_status()?
        .json::<SearchResponse>()?;

    Ok(response.results)
}

fn sanitize_folder_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | '\n' | '\r' | '\0' => ' ',
            ':' => '-',
            _ => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn mk_movie_dir(movies_root: &Path, tv_root: &Path) -> Result<()> {
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

        let title = sanitize_folder_name(selected.title());

        if title.is_empty() {
            return Err("Invalid movie title".into());
        }

        let year = selected
            .year()
            .ok_or("Selected title has no release year")?;

        let folder_name = format!("{title} ({year})");
        let destination = root.join(folder_name);

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
    let args: Vec<String> = env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("file-cleanup") => {
            let root = args
                .get(2)
                .ok_or("Usage: storage-utils file-cleanup <path>")?;

            file_cleanup(Path::new(root))
        }

        Some("mkdir") => {
            let movies = args
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/mnt/storage/Movies"));

            let tv = args
                .get(3)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/mnt/storage/TV Shows"));

            mk_movie_dir(&movies, &tv)
        }

        _ => {
            println!("Storage Utils\n");
            println!("Commands:");
            println!("  file-cleanup <path>");
            println!("  mkdir [movies-path] [tv-path]");
            Ok(())
        }
    }
}
