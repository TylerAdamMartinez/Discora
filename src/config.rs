use crate::Result;
use crate::destination::{Destination, RipSettings};
use inquire::Text;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct LibraryPaths {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub movies_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tv_shows_path: Option<PathBuf>,
    #[serde(default)]
    pub rip: RipSettings,
    // Preserve unrelated settings when completing a partial config.
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

/// An explicit movie path always selects local output, including when the saved
/// destination is remote. Utilities continue to resolve only local library paths.
pub fn resolve_rip(overrides: LibraryPaths) -> Result<Destination> {
    if let Some(path) = &overrides.movies_path {
        validate_path(path)?;
        return Ok(Destination::Local(path.clone()));
    }
    let config_path = dirs::config_dir()
        .ok_or("Cannot locate the user config directory; supply --movies-path")?
        .join("discora/discora.toml");
    if let Some(remote) = remote_at(&config_path)? {
        return Ok(Destination::Sshfs(remote));
    }
    let paths = resolve(overrides, false)?;
    Ok(Destination::Local(
        paths.movies_path.expect("resolved movie path"),
    ))
}

fn remote_at(path: &Path) -> Result<Option<crate::destination::Remote>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Cannot read config {}: {error}", path.display()).into()),
    };
    let saved: LibraryPaths = toml::from_str(&contents)
        .map_err(|error| format!("Invalid config {}: {error}", path.display()))?;
    saved
        .rip
        .destination
        .remote()
        .map_err(|error| format!("Config {}: {error}", path.display()).into())
}

pub fn parse_path(value: &str) -> std::result::Result<PathBuf, String> {
    let path = PathBuf::from(value);
    validate_path(&path)?;
    Ok(path)
}

fn validate_path(path: &Path) -> std::result::Result<(), String> {
    if !path.is_absolute() {
        return Err(format!(
            "Expected a nonempty absolute directory path: {}",
            path.display()
        ));
    }
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_dir() => Err(format!("Not a directory: {}", path.display())),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Cannot access {}: {error}", path.display())),
    }
}

fn validate(paths: &LibraryPaths, need_tv: bool) -> Result<()> {
    if let Some(path) = &paths.movies_path {
        validate_path(path).map_err(|error| format!("movies_path: {error}"))?;
    }
    if need_tv && let Some(path) = &paths.tv_shows_path {
        validate_path(path).map_err(|error| format!("tv_shows_path: {error}"))?;
    }
    Ok(())
}

fn complete(paths: &LibraryPaths, need_tv: bool) -> bool {
    paths.movies_path.is_some() && (!need_tv || paths.tv_shows_path.is_some())
}

pub fn resolve(overrides: LibraryPaths, need_tv: bool) -> Result<LibraryPaths> {
    // Fully explicit invocations must not depend on config discovery or its contents.
    if complete(&overrides, need_tv) {
        validate(&overrides, need_tv)?;
        return Ok(overrides);
    }
    let path = dirs::config_dir()
        .ok_or("Cannot locate the user config directory; supply the required library path flags")?
        .join("discora")
        .join("discora.toml");
    resolve_at(
        overrides,
        need_tv,
        &path,
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        |label, default| {
            loop {
                let mut prompt = Text::new(label);
                if let Some(default) = default {
                    prompt = prompt.with_default(default);
                }
                let input = prompt.prompt()?;
                match parse_path(&input) {
                    Ok(path) => return Ok(path),
                    Err(error) => eprintln!("{error}"),
                }
            }
        },
    )
}

fn resolve_at(
    overrides: LibraryPaths,
    need_tv: bool,
    config_path: &Path,
    interactive: bool,
    mut prompt: impl FnMut(&str, Option<&str>) -> Result<PathBuf>,
) -> Result<LibraryPaths> {
    validate(&overrides, need_tv)?;
    if complete(&overrides, need_tv) {
        return Ok(overrides);
    }
    let mut saved: LibraryPaths = match fs::read_to_string(config_path) {
        Ok(contents) => toml::from_str(&contents)
            .map_err(|error| format!("Invalid config {}: {error}", config_path.display()))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => LibraryPaths::default(),
        Err(error) => {
            return Err(format!("Cannot read config {}: {error}", config_path.display()).into());
        }
    };
    let effective = LibraryPaths {
        movies_path: overrides
            .movies_path
            .clone()
            .or_else(|| saved.movies_path.clone()),
        tv_shows_path: overrides
            .tv_shows_path
            .clone()
            .or_else(|| saved.tv_shows_path.clone()),
        ..Default::default()
    };
    validate(&effective, need_tv)
        .map_err(|error| format!("Config {}: {error}", config_path.display()))?;
    if complete(&effective, need_tv) {
        return Ok(effective);
    }
    if !interactive {
        let missing =
            if effective.movies_path.is_none() && need_tv && effective.tv_shows_path.is_none() {
                "--movies-path and --tv-shows-path"
            } else if effective.movies_path.is_none() {
                "--movies-path"
            } else {
                "--tv-shows-path"
            };
        return Err(format!(
            "Missing library paths: supply {missing}, configure {}, or run interactively to create the config",
            config_path.display()
        ).into());
    }
    println!(
        "Library setup will save settings to {}",
        config_path.display()
    );
    if saved.movies_path.is_none() {
        let default = overrides.movies_path.as_deref().and_then(Path::to_str);
        saved.movies_path = Some(prompt("Absolute path to movies:", default)?);
    }
    if saved.tv_shows_path.is_none() {
        let default = overrides.tv_shows_path.as_deref().and_then(Path::to_str);
        saved.tv_shows_path = Some(prompt("Absolute path to TV shows:", default)?);
    }
    validate(&saved, true).map_err(|error| format!("Config {}: {error}", config_path.display()))?;
    save(config_path, &saved)?;
    println!("Saved config: {}", config_path.display());
    Ok(LibraryPaths {
        movies_path: overrides.movies_path.or(saved.movies_path),
        tv_shows_path: overrides.tv_shows_path.or(saved.tv_shows_path),
        ..Default::default()
    })
}

fn save(path: &Path, settings: &LibraryPaths) -> Result<()> {
    let write = || -> Result<()> {
        let parent = path.parent().ok_or("Config path has no parent directory")?;
        fs::create_dir_all(parent)?;
        let contents = toml::to_string_pretty(settings)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(contents.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        Ok(())
    };
    write().map_err(|error| format!("Cannot save config {}: {error}", path.display()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(movie: Option<&Path>, tv: Option<&Path>) -> LibraryPaths {
        LibraryPaths {
            movies_path: movie.map(Path::to_path_buf),
            tv_shows_path: tv.map(Path::to_path_buf),
            ..Default::default()
        }
    }

    fn no_prompt(_: &str, _: Option<&str>) -> Result<PathBuf> {
        panic!("unexpected setup prompt")
    }

    #[test]
    fn remote_config_needs_no_local_paths_and_does_not_rewrite_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("discora.toml");
        let contents = "[rip.destination]\ntype = 'sshfs'\nhost = 'server'\nusername = 'friend'\nmovies_path = '/media/Movies'\n";
        fs::write(&config, contents).unwrap();
        let remote = remote_at(&config).unwrap().unwrap();
        assert_eq!(remote.port, 22);
        assert_eq!(remote.movies_path, "/media/Movies");
        assert_eq!(fs::read_to_string(config).unwrap(), contents);
    }

    #[test]
    fn local_default_is_saved_and_existing_configs_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("discora.toml");
        fs::write(
            &config,
            format!("movies_path = '{}'\n", dir.path().display()),
        )
        .unwrap();
        assert!(remote_at(&config).unwrap().is_none());
        save(&config, &paths(Some(dir.path()), None)).unwrap();
        let contents = fs::read_to_string(&config).unwrap();
        assert!(contents.contains("[rip.destination]"));
        assert!(contents.contains("type = \"local\""));
        assert!(remote_at(&config).unwrap().is_none());
    }

    #[test]
    fn invalid_remote_config_reports_filename_and_preserves_contents() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("discora.toml");
        for contents in [
            "[rip.destination]\ntype = 'other'",
            "[rip.destination]\ntype = 'sshfs'",
            "[rip.destination]\ntype = 'sshfs'\nhost = '-bad'\nusername = 'friend'\nmovies_path = '/Movies'",
            "[rip.destination]\ntype = 'sshfs'\nhost = 'server'\nusername = 'friend'\nmovies_path = 'relative'",
        ] {
            fs::write(&config, contents).unwrap();
            let error = remote_at(&config).unwrap_err().to_string();
            assert!(error.contains(config.to_str().unwrap()), "{error}");
            assert_eq!(fs::read_to_string(&config).unwrap(), contents);
        }
    }

    #[test]
    fn cli_paths_bypass_even_malformed_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("discora.toml");
        fs::write(&config, "not toml = [").unwrap();
        let result = resolve_at(
            paths(Some(dir.path()), Some(dir.path())),
            true,
            &config,
            false,
            no_prompt,
        )
        .unwrap();
        assert_eq!(result.movies_path.as_deref(), Some(dir.path()));
        assert_eq!(fs::read_to_string(config).unwrap(), "not toml = [");
    }

    #[test]
    fn overrides_merge_independently_without_changing_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("discora.toml");
        save(&config, &paths(Some(dir.path()), Some(dir.path()))).unwrap();
        let original = fs::read(&config).unwrap();
        for movie_override in [true, false] {
            let different = dir.path().join("other");
            let overrides = if movie_override {
                paths(Some(&different), None)
            } else {
                paths(None, Some(&different))
            };
            let result = resolve_at(overrides, true, &config, false, no_prompt).unwrap();
            assert_eq!(
                result.movies_path.as_deref(),
                Some(if movie_override {
                    different.as_path()
                } else {
                    dir.path()
                })
            );
            assert_eq!(
                result.tv_shows_path.as_deref(),
                Some(if movie_override {
                    dir.path()
                } else {
                    different.as_path()
                })
            );
            assert_eq!(fs::read(&config).unwrap(), original);
        }
    }

    #[test]
    fn movie_only_workflow_does_not_require_tv() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("discora.toml");
        save(&config, &paths(Some(dir.path()), None)).unwrap();
        let result = resolve_at(LibraryPaths::default(), false, &config, false, no_prompt).unwrap();
        assert_eq!(result.movies_path.as_deref(), Some(dir.path()));
        assert!(result.tv_shows_path.is_none());
    }

    #[test]
    fn first_run_prompts_in_order_saves_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("nested/discora.toml");
        let mut labels = Vec::new();
        let result = resolve_at(
            LibraryPaths::default(),
            false,
            &config,
            true,
            |label, default| {
                labels.push(label.to_owned());
                assert!(default.is_none());
                Ok(dir.path().join(if labels.len() == 1 {
                    "Movies"
                } else {
                    "TV Shows"
                }))
            },
        )
        .unwrap();
        assert_eq!(
            labels,
            ["Absolute path to movies:", "Absolute path to TV shows:"]
        );
        let loaded = resolve_at(LibraryPaths::default(), true, &config, false, no_prompt).unwrap();
        assert_eq!(loaded.movies_path, result.movies_path);
        assert_eq!(loaded.tv_shows_path, result.tv_shows_path);
        assert!(!result.movies_path.unwrap().exists());
        assert!(!fs::read_to_string(config).unwrap().contains("TMDB"));
    }

    #[test]
    fn setup_offers_cli_default_but_override_still_wins() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("discora.toml");
        let explicit = dir.path().join("explicit");
        let saved_movie = dir.path().join("saved");
        let mut count = 0;
        let result = resolve_at(
            paths(Some(&explicit), None),
            true,
            &config,
            true,
            |_, default| {
                count += 1;
                if count == 1 {
                    assert_eq!(default, explicit.to_str());
                    Ok(saved_movie.clone())
                } else {
                    assert!(default.is_none());
                    Ok(dir.path().join("tv"))
                }
            },
        )
        .unwrap();
        assert_eq!(result.movies_path, Some(explicit));
        let saved: LibraryPaths = toml::from_str(&fs::read_to_string(config).unwrap()).unwrap();
        assert_eq!(saved.movies_path, Some(saved_movie));
    }

    #[test]
    fn partial_setup_preserves_existing_and_unrelated_values() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("discora.toml");
        let mut initial = paths(None, Some(dir.path()));
        initial
            .extra
            .insert("future_setting".into(), toml::Value::Boolean(true));
        save(&config, &initial).unwrap();
        let mut count = 0;
        resolve_at(LibraryPaths::default(), true, &config, true, |label, _| {
            count += 1;
            assert_eq!(label, "Absolute path to movies:");
            Ok(dir.path().join("movies"))
        })
        .unwrap();
        assert_eq!(count, 1);
        let saved: LibraryPaths = toml::from_str(&fs::read_to_string(config).unwrap()).unwrap();
        assert_eq!(saved.tv_shows_path, initial.tv_shows_path);
        assert_eq!(saved.extra, initial.extra);
    }

    #[test]
    fn cancellation_never_writes_partial_setup() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("discora.toml");
        for existing in [false, true] {
            if existing {
                fs::write(&config, "# original\n").unwrap();
            }
            let original = fs::read(&config).ok();
            let mut count = 0;
            let result = resolve_at(LibraryPaths::default(), true, &config, true, |_, _| {
                count += 1;
                if count == 1 {
                    Ok(dir.path().to_path_buf())
                } else {
                    Err("cancelled".into())
                }
            });
            assert!(result.is_err());
            assert_eq!(fs::read(&config).ok(), original);
        }
    }

    #[test]
    fn missing_paths_noninteractive_error_names_flags_and_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("discora.toml");
        let error = resolve_at(LibraryPaths::default(), true, &config, false, no_prompt)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--movies-path and --tv-shows-path"));
        assert!(error.contains(config.to_str().unwrap()));
        assert!(!config.exists());
    }

    #[test]
    fn invalid_config_and_read_errors_include_filename() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("discora.toml");
        for content in [
            "invalid = [",
            "movies_path = 'relative'",
            "movies_path = 42",
        ] {
            fs::write(&config, content).unwrap();
            let error = resolve_at(LibraryPaths::default(), true, &config, true, no_prompt)
                .unwrap_err()
                .to_string();
            assert!(error.contains(config.to_str().unwrap()));
            assert_eq!(fs::read_to_string(&config).unwrap(), content);
        }
        let error = resolve_at(LibraryPaths::default(), true, dir.path(), true, no_prompt)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Cannot read config"));
    }

    #[test]
    fn paths_must_be_absolute_directories_but_need_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        for invalid in ["", "relative", "~/Movies", "$HOME/Movies"] {
            assert!(parse_path(invalid).is_err());
        }
        let file = dir.path().join("file");
        fs::write(&file, "data").unwrap();
        assert!(validate_path(&file).is_err());
        assert!(validate_path(&dir.path().join("new library")).is_ok());
    }

    #[test]
    fn failed_save_reports_path_without_overwriting_blocker() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        fs::write(&blocker, "unchanged").unwrap();
        let config = blocker.join("discora.toml");
        let error = save(&config, &paths(Some(dir.path()), Some(dir.path())))
            .unwrap_err()
            .to_string();
        assert!(error.contains(config.to_str().unwrap()));
        assert_eq!(fs::read_to_string(blocker).unwrap(), "unchanged");
    }
}
