use std::{env, error::Error, fs, path::Path};

fn clean_name(name: &str) -> String {
    name.replace(['\n', '\r'], " ")
}

fn main() -> Result<(), Box<dyn Error>> {
    let root = env::args()
        .nth(1)
        .ok_or("Usage: rename_movies <directory>")?;

    let root = Path::new(&root);

    if !root.is_dir() {
        return Err(format!("Not a directory: {}", root.display()).into());
    }

    for entry in fs::read_dir(root)? {
        let folder = entry?.path();

        if !folder.is_dir() {
            continue;
        }

        let mkvs: Vec<_> = fs::read_dir(&folder)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("mkv"))
            })
            .collect();

        if mkvs.len() != 1 {
            continue;
        }

        let Some(folder_name) = folder.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        let cleaned_name = clean_name(folder_name);
        let old_file = &mkvs[0];
        let new_file = folder.join(format!("{cleaned_name}.mkv"));

        if *old_file == new_file {
            continue;
        }

        if new_file.exists() {
            eprintln!("Skipping, destination exists: {}", new_file.display());
            continue;
        }

        fs::rename(old_file, &new_file)?;

        println!("{} -> {}", old_file.display(), new_file.display());
    }

    Ok(())
}
