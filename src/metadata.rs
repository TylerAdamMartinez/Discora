use crate::Result;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::{
    fmt,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize)]
struct SearchResponse {
    results: Vec<Media>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Media {
    id: u64,
    title: Option<String>,
    name: Option<String>,
    release_date: Option<String>,
    first_air_date: Option<String>,
}

impl Media {
    pub fn title(&self) -> &str {
        self.title
            .as_deref()
            .or(self.name.as_deref())
            .unwrap_or("Unknown")
    }

    pub fn year(&self) -> Option<&str> {
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

pub fn search_tmdb(
    client: &Client,
    token: &str,
    media_type: &str,
    query: &str,
) -> Result<Vec<Media>> {
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

pub fn sanitize_folder_name(name: &str) -> String {
    sanitize_name(name, cfg!(windows))
}

fn sanitize_name(name: &str, windows: bool) -> String {
    let clean = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\n' | '\r' | '\0' => ' ',
            ':' => '-',
            '<' | '>' | '"' | '|' | '?' | '*' if windows => ' ',
            c if windows && c.is_control() => ' ',
            _ => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !windows {
        return clean;
    }
    let mut clean = clean.trim_end_matches(['.', ' ']).to_owned();
    let stem = clean
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|n| {
                matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
    if reserved {
        clean.insert(0, '_');
    }
    clean
}

pub fn movie_destination(root: &Path, movie: &Media) -> Result<PathBuf> {
    let title = sanitize_folder_name(movie.title());
    if title.is_empty() {
        return Err("Invalid movie title".into());
    }
    let year = movie.year().ok_or("Selected title has no release year")?;
    Ok(root.join(format!("{title} ({year})")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_names_are_safe_and_valid_names_are_preserved() {
        assert_eq!(sanitize_name("A <Movie>?*|\"\u{1}. ", true), "A Movie");
        for name in ["CON", "nul.txt", "COM1", "LPT²"] {
            assert!(sanitize_name(name, true).starts_with('_'));
        }
        assert_eq!(sanitize_name("Good Movie", true), "Good Movie");
        assert_eq!(sanitize_name("A?", false), "A?");
    }

    #[test]
    fn movie_and_tv_results_keep_existing_naming_and_reject_missing_years() {
        let movie: Media = serde_json::from_str(
            r#"{"id":1,"title":"Movie: / Title","release_date":"2005-06-01"}"#,
        )
        .unwrap();
        assert_eq!(
            movie_destination(Path::new("/movies"), &movie).unwrap(),
            Path::new("/movies/Movie- Title (2005)")
        );
        let tv: Media =
            serde_json::from_str(r#"{"id":2,"name":"A Show","first_air_date":"2006-01-01"}"#)
                .unwrap();
        assert_eq!(
            movie_destination(Path::new("/tv"), &tv).unwrap(),
            Path::new("/tv/A Show (2006)")
        );
        let undated: Media =
            serde_json::from_str(r#"{"id":3,"title":"Unknown","release_date":""}"#).unwrap();
        assert!(movie_destination(Path::new("/movies"), &undated).is_err());
    }
}
