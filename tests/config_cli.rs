//! Exercise command dispatch with isolated config and no TMDB/network access.
#![cfg(target_os = "linux")]

use std::{fs, path::Path, process::Command};

fn command(config_home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_discora"));
    command
        .env("XDG_CONFIG_HOME", config_home)
        .env("HOME", config_home)
        .env_remove("TMDB_TOKEN");
    command
}

fn write_config(config_home: &Path, contents: &str) {
    fs::create_dir_all(config_home.join("discora")).unwrap();
    fs::write(config_home.join("discora/discora.toml"), contents).unwrap();
}

fn library(path: &Path) {
    let title = path.join("Title (2000)");
    fs::create_dir_all(&title).unwrap();
    fs::write(title.join("original.mkv"), "video").unwrap();
}

#[test]
fn cleanup_uses_config_and_independent_overrides_without_tmdb() {
    let dir = tempfile::tempdir().unwrap();
    let movies = dir.path().join("movies");
    let tv = dir.path().join("tv");
    let override_movies = dir.path().join("override");
    for path in [&movies, &tv, &override_movies] {
        library(path);
    }
    write_config(
        dir.path(),
        &format!(
            "movies_path = '{}'\ntv_shows_path = '{}'\n",
            movies.display(),
            tv.display()
        ),
    );
    let config = dir.path().join("discora/discora.toml");
    let original_config = fs::read(&config).unwrap();
    let output = command(dir.path())
        .arg("--movies-path")
        .arg(&override_movies)
        .arg("file-cleanup")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(movies.join("Title (2000)/original.mkv").exists());
    for path in [&tv, &override_movies] {
        assert!(path.join("Title (2000)/Title (2000).mkv").exists());
    }
    assert_eq!(fs::read(config).unwrap(), original_config);
    assert!(
        command(dir.path())
            .arg("file-cleanup")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(movies.join("Title (2000)/Title (2000).mkv").exists());
}

#[test]
fn explicit_cleanup_bypasses_bad_config_but_rejects_conflicting_global_flags() {
    let dir = tempfile::tempdir().unwrap();
    let movies = dir.path().join("movies");
    library(&movies);
    write_config(dir.path(), "invalid = [");
    assert!(
        command(dir.path())
            .arg("file-cleanup")
            .arg(&movies)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        command(dir.path())
            .arg("file-cleanup")
            .arg("--movies-path")
            .arg(&movies)
            .arg("--tv-shows-path")
            .arg(&movies)
            .output()
            .unwrap()
            .status
            .success()
    );
    let output = command(dir.path())
        .arg("--tv-shows-path")
        .arg(&movies)
        .arg("file-cleanup")
        .arg(&movies)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be combined"));
}

#[test]
fn every_command_resolves_missing_paths_before_starting_work() {
    let dir = tempfile::tempdir().unwrap();
    for subcommand in [None, Some("rip"), Some("mkdir"), Some("file-cleanup")] {
        let mut cmd = command(dir.path());
        if let Some(subcommand) = subcommand {
            cmd.arg(subcommand);
        }
        let output = cmd.output().unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("--movies-path"), "{error}");
        assert!(error.contains("discora.toml"), "{error}");
    }
    assert!(!dir.path().join("discora").exists());
    for flag in ["--help", "--version"] {
        assert!(
            command(dir.path())
                .arg(flag)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}

#[test]
fn mkdir_resolves_config_and_cli_paths_before_requesting_token() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        &format!(
            "movies_path = '{}'\ntv_shows_path = '{}'\n",
            dir.path().display(),
            dir.path().display()
        ),
    );
    let output = command(dir.path()).arg("mkdir").output().unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("Set TMDB_TOKEN first"));
    write_config(dir.path(), "invalid = [");
    let output = command(dir.path())
        .arg("mkdir")
        .arg("--movies-path")
        .arg(dir.path())
        .arg("--tv-shows-path")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("Set TMDB_TOKEN first"));
}

#[test]
fn remote_rip_uses_its_destination_without_local_setup_and_cli_path_selects_local() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        "[rip.destination]\ntype = 'sshfs'\nhost = 'server'\nusername = 'friend'\nmovies_path = '/media/Movies'\n",
    );
    for subcommand in [None, Some("rip")] {
        let mut cmd = command(dir.path());
        if let Some(subcommand) = subcommand {
            cmd.arg(subcommand);
        }
        let output = cmd.output().unwrap();
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("interactive terminal"), "{error}");
        assert!(!error.contains("Missing library paths"));
    }
    // An explicit local path bypasses even an invalid remote configuration.
    write_config(
        dir.path(),
        "[rip.destination]\ntype = 'sshfs'\nhost = '-bad'\n",
    );
    let output = command(dir.path())
        .arg("rip")
        .arg("--movies-path")
        .arg(dir.path())
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("interactive terminal"), "{error}");
    assert!(!error.contains("Invalid config"));
}
