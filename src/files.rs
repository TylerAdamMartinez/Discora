use crate::Result;
use std::{
    fs,
    path::{Path, PathBuf},
};

fn clean_name(name: &str) -> String {
    name.replace(['\n', '\r'], " ")
}

/// Validate both libraries before changing either, and process shared roots once.
pub fn library_cleanup(movies: &Path, tv: &Path) -> Result<()> {
    for root in [movies, tv] {
        if !root.is_dir() {
            return Err(format!("Not a directory: {}", root.display()).into());
        }
    }
    let same_root = fs::canonicalize(movies)? == fs::canonicalize(tv)?;
    file_cleanup(movies)?;
    if !same_root {
        file_cleanup(tv)?;
    }
    Ok(())
}

pub fn file_cleanup(root: &Path) -> Result<()> {
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

        let Some(_) = folder.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        let new_file = named_mkv(&folder)?;

        let old_file = &mkvs[0];

        if *old_file == new_file {
            continue;
        }

        if new_file.exists() {
            eprintln!("Skipping existing file: {}", new_file.display());
            continue;
        }

        publish_mkv(old_file, &new_file)?;

        println!("{} -> {}", old_file.display(), new_file.display());
    }

    Ok(())
}

/// Hard-link publication is atomic and refuses an existing destination, even in a race.
/// Staging lives inside the destination directory, so both paths share a filesystem.
pub fn publish_mkv(source: &Path, destination: &Path) -> Result<()> {
    fs::hard_link(source, destination)?;
    fs::remove_file(source)?;
    Ok(())
}

pub fn named_mkv(folder: &Path) -> Result<PathBuf> {
    let name = folder
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("Invalid movie folder")?;
    Ok(folder.join(format!("{}.mkv", clean_name(name))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_both_libraries_and_validates_before_changing_any_files() {
        let root = tempfile::tempdir().unwrap();
        let movies = root.path().join("movies");
        let tv = root.path().join("tv");
        for library in [&movies, &tv] {
            let folder = library.join("Title (2000)");
            fs::create_dir_all(&folder).unwrap();
            fs::write(folder.join("title00.mkv"), "video").unwrap();
        }
        assert!(library_cleanup(&movies, &root.path().join("missing")).is_err());
        assert!(movies.join("Title (2000)/title00.mkv").exists());
        library_cleanup(&movies, &tv).unwrap();
        for library in [&movies, &tv] {
            assert!(library.join("Title (2000)/Title (2000).mkv").exists());
        }
        library_cleanup(&movies, &movies).unwrap();
    }

    #[test]
    fn legacy_cleanup_renames_one_mkv_and_skips_multiple_mkvs() {
        let root = tempfile::tempdir().unwrap();
        let single = root.path().join("One Movie (2005)");
        let multiple = root.path().join("Two Titles (2006)");
        fs::create_dir(&single).unwrap();
        fs::create_dir(&multiple).unwrap();
        fs::write(single.join("title00.MKV"), "movie").unwrap();
        fs::write(multiple.join("main.mkv"), "main").unwrap();
        fs::write(multiple.join("extra.mkv"), "extra").unwrap();
        file_cleanup(root.path()).unwrap();
        file_cleanup(root.path()).unwrap();
        assert_eq!(
            fs::read_to_string(named_mkv(&single).unwrap()).unwrap(),
            "movie"
        );
        assert!(!single.join("title00.MKV").exists());
        assert!(multiple.join("main.mkv").exists());
        assert!(multiple.join("extra.mkv").exists());
    }
}
