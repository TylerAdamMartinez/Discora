# Storage Utils

Rust utilities for movie/TV directory creation, headless movie ripping, and MKV naming.

## Rip a movie

On Linux, install MakeMKV with `makemkvcon` on `PATH`, configure its registration
and audio/subtitle preferences, and ensure your user can access the optical drive.
Set `TMDB_TOKEN` to your TMDB API read access token, then run:

```sh
cargo run
# The explicit `rip` command is also supported.
# Choose a particular MakeMKV drive index or a different library root:
cargo run -- rip --disc 0 --movies-path "/mnt/storage/Movies"
```

The full-screen interface walks through TMDB movie search, drive discovery, title
scanning, selection, and confirmation. Titles are shown longest first with their
IDs, duration, size, source playlist/file, and name when available. Choose the
correct title explicitly; the longest title may not be the main movie.

MakeMKV runs headlessly, using its current audio/subtitle preferences. Progress,
elapsed time, an approximate ETA, and live logs are displayed during the rip.
The resulting file is named `Movie Title (Year)/Movie Title (Year).mkv`.
This workflow handles one movie title per run; TV episodes and multiple-title
ripping are not included.

| Key | Action |
| --- | --- |
| Type | Search TMDB or filter the current selection list |
| Up/Down, Enter | Select and continue; Enter on confirmation starts ripping |
| Esc | Back/exit; during a task, cancel and wait for cleanup |
| Ctrl+C | Cancel and exit after cleanup |
| F2 | Open/close the full log viewer |
| L | Open/close logs while ripping or on the result screen |
| Up/Down in logs | Scroll within the current page |
| PageUp/PageDown, Home/End | Navigate the full on-disk log |
| R in logs | Refresh the current page |

Logs are retained at `$XDG_STATE_HOME/storage-utils/logs`, or
`~/.local/state/storage-utils/logs` when `XDG_STATE_HOME` is unset. The log path is
shown in the interface and printed on exit. Only a bounded recent log buffer is
kept in memory; the full viewer reads the disk log in pages.

Each rip writes to a unique `.storage-utils-rip-*` directory inside the movie
directory. After MakeMKV succeeds, exactly one nonempty, regular MKV must exist
before it is published. Existing final filenames are never replaced, including
when another process creates one during the rip. Publication uses a hard link
and requires a filesystem that supports hard links. Existing extras are left alone.

On failure, Esc/Ctrl+C cancellation, or SIGTERM, the application stops and reaps
MakeMKV, deletes only that attempt's temporary output, and retains the log. Cleanup
failures report the remaining path. Forced termination or power loss can leave
staging directories behind; these are not automatically deleted on later runs.
An empty movie directory may remain after a failed attempt.

The application orchestrates MakeMKV through its
[robot interface](https://www.makemkv.com/developers/usage.txt); it does not decode discs itself.

## Existing utilities

```sh
# Search TMDB and create a movie or TV show directory:
cargo run -- mkdir [movies-path] [tv-path]

# Rename the single MKV in each immediate child directory to match that directory:
cargo run -- file-cleanup /mnt/storage/Movies
```

`mkdir` defaults to `/mnt/storage/Movies` and `/mnt/storage/TV Shows`.
`mk-movie-dir` is also accepted as an alias. `file-cleanup` skips directories
containing zero or multiple MKVs and does not replace existing destination files.

## Development

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Tests use a fake MakeMKV executable and temporary directories; no disc, TMDB token,
or MakeMKV installation is needed. The process tests require Python 3 and Linux/Unix.
A real-disc acceptance check still requires selecting and ripping a movie on a
machine with an accessible optical drive.
