# Discora

Discora is a Rust CLI for movie/TV directory creation, headless movie ripping, and MKV naming.

Install the CLI from this repository with `cargo install --path .`, then run
`discora` to start the interactive movie ripper. Use `discora --help` to see all commands.

## Library paths and configuration

All commands resolve each required library path in this order:
**command-line argument → config file → interactive setup**. There are no built-in
movie or TV directory defaults. `TMDB_TOKEN` remains environment-only and is never
saved in the config.

Use `--movies-path` and `--tv-shows-path` before or after a subcommand:

```sh
discora --movies-path /path/to/movies
discora rip --movies-path /path/to/movies --disc 0
discora mkdir --movies-path /path/to/movies --tv-shows-path "/path/to/TV Shows"
discora file-cleanup --movies-path /path/to/movies --tv-shows-path "/path/to/TV Shows"
```

Flags override only their corresponding settings for that invocation; they do not
rewrite your config. If all paths required by the command are explicit, Discora
skips config loading entirely. `rip` needs only the movie path; `mkdir` and
`file-cleanup` need both library paths.

Discora reads `discora.toml` from your OS's normal config directory:

| OS | Config file |
| --- | --- |
| Linux | `$XDG_CONFIG_HOME/discora/discora.toml`, or `$HOME/.config/discora/discora.toml` when unset |
| macOS | `$HOME/Library/Application Support/discora/discora.toml` |
| Windows | `%APPDATA%\discora\discora.toml` |

```toml
movies_path = "/absolute/path/to/movies"
tv_shows_path = "/absolute/path/to/tv-shows"
```

Use absolute paths for both flags and config values. TOML values do not expand
`~` or environment variables. Windows paths can use literal TOML strings, such as
`movies_path = 'D:\Movies'`. Library directories need not exist for creation or
ripping, but cleanup requires existing directories. Native config discovery does
not change the current Linux/Unix requirements of the ripping implementation.

If a required path is missing, interactive setup asks for the movie path first,
then the TV path, skipping settings already saved. It offers CLI values as defaults
for missing saved settings. Accepting a value during setup saves it; the explicit
CLI override still controls the current invocation. Setup saves both settings
before continuing, even when triggered by movie-only ripping. Cancelling setup
leaves the existing config unchanged.

Without an interactive terminal, missing paths produce an error explaining which
flags or config settings to provide. Invalid/unreadable config files and save
failures are reported with their filename; malformed files are not overwritten.

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

Logs are retained at `$XDG_STATE_HOME/discora/logs`, or
`~/.local/state/discora/logs` when `XDG_STATE_HOME` is unset. The log path is
shown in the interface and printed on exit. Only a bounded recent log buffer is
kept in memory; the full viewer reads the disk log in pages.

Each rip writes to a unique `.discora-rip-*` directory inside the movie
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
cargo run -- mkdir

# Clean both configured libraries (immediate child directories only):
cargo run -- file-cleanup

# Clean only one explicit directory without config loading or setup:
cargo run -- file-cleanup /path/to/movies
```

The existing `mkdir [movies-path] [tv-path]` positional arguments are also supported;
omitted paths use the same config/setup workflow. Do not combine a positional path
with its corresponding flag. The `file-cleanup [path]` positional argument cannot
be combined with either library flag.

Without a positional path, `file-cleanup` validates both roots before making
changes, cleans movies first and TV shows second, and processes a shared root only
once. Cleanup is not recursive: it skips immediate child directories containing
zero or multiple MKVs and never replaces an existing destination file.

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
