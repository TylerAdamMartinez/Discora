# Discora

Discora is a Rust CLI for movie/TV directory creation, headless movie ripping, and MKV naming on Linux and Windows 10/11 x64.

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
skips library-path config loading. The ripper still reads saved storage display
and drive-selection preferences when the config is readable and valid; otherwise
it uses their defaults. `rip` needs only the movie path; `mkdir` and
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
storage_units = "decimal" # "decimal" (GB/TB) or "binary" (GiB/TiB)

[rip]
auto_select_single_drive = true
auto_select_runtime_match = true

[rip.destination]
type = "local"
```

Omitting `[rip.destination]` also selects local ripping. Setup writes an explicit
`type = "local"` when creating a config.

Use absolute paths for both flags and config values. TOML values do not expand
`~` or environment variables. Windows paths can use literal TOML strings, such as
`movies_path = 'D:\Movies'`. Library directories need not exist for creation or
ripping, but cleanup requires existing directories. Windows drive paths and UNC
library paths are supported. Local publication needs a filesystem with hard-link
support, such as NTFS; FAT/exFAT destinations are not supported.

If a required path is missing, interactive setup asks for the movie path first,
then the TV path, skipping settings already saved. It offers CLI values as defaults
for missing saved settings. Accepting a value during setup saves it; the explicit
CLI override still controls the current invocation. When creating or completing a
config, setup also asks for storage display units
if that setting is missing, with decimal preselected. It explains both choices
and shows an example before asking. Setup saves the paths and unit choice
before continuing, even when triggered by movie-only ripping. Cancelling setup
leaves the existing config unchanged.

Without an interactive terminal, missing paths produce an error explaining which
flags or config settings to provide. Invalid/unreadable config files and save
failures are reported with their filename; malformed files are not overwritten.

## Rip a movie

Install MakeMKV, configure its registration and audio/subtitle preferences, and
ensure your user can access the optical drive. On Linux, put `makemkvcon` on `PATH`.
On Windows, Discora searches `PATH`, then the standard MakeMKV folders under
Program Files, for `makemkvcon64.exe` or `makemkvcon.exe`. To use a custom
installation, supply `--makemkv-path` before or after `rip`; an invalid explicit
path fails instead of silently selecting another installation.
Set `TMDB_TOKEN` to your TMDB API read access token, then run:

```sh
cargo run
# The explicit `rip` command is also supported.
# Choose a particular MakeMKV drive index or a different library root:
cargo run -- rip --disc 0 --movies-path "/mnt/storage/Movies"
```

For Windows, install Rust with the MSVC toolchain and Visual Studio C++ Build
Tools, then run in PowerShell or Windows Terminal:

```powershell
cargo install --path .
$env:TMDB_TOKEN = "your-api-read-access-token"
discora rip --movies-path 'D:\Movies'
# Optional executable override:
discora --makemkv-path 'C:\Program Files (x86)\MakeMKV\makemkvcon64.exe' rip --movies-path 'D:\Movies'
```

Generated Windows names replace forbidden characters and avoid reserved device
names. Existing folders are not renamed automatically.

The full-screen interface walks through TMDB movie search, drive discovery, title
scanning, selection, and confirmation. Titles are shown longest first with their
IDs, duration, size, source playlist/file, and name when available. Choose the
correct title explicitly; the longest title may not be the main movie.

With `rip.auto_select_single_drive = true` (the default), finding exactly one
optical drive skips the drive picker and starts scanning that drive. Set it to
`false` to always choose manually. Multiple drives still require selection;
`--disc` selects an explicit MakeMKV drive index and takes priority.

On the confirmation screen, **Enter starts ripping** and **Esc returns to title
selection**. Going back preserves the filter and highlighted title without
rescanning. Starting the rip launches a separate MakeMKV process, which opens
and analyzes the disc again. See the [integration investigation](MAKEMKV_INTEGRATION.md)
for the current limitations and alternatives.

While choosing a TMDB movie, terminals at least 100 columns wide with 12 rows
available for results show a side preview with its poster and synopsis. The
preview follows the highlighted result, including when filtering. Posters load
in the background and are cached in memory for the current search; they are not
saved to movie folders. Missing or unavailable posters do not prevent selection.
Terminal graphics are detected automatically, with a lower-resolution colored
block fallback (including on Windows Terminal when graphics are unavailable).
Narrower or shorter windows keep the full-width text list. Press F2 for logs
while browsing movies. The separate `mkdir` picker remains text-only.

MakeMKV runs headlessly, using its current audio/subtitle preferences. Progress,
elapsed time, an approximate ETA, and live logs are displayed during the rip.
During the final ripping job, including disc opening and finalization, Enter,
Esc, or Ctrl+C opens a full-screen cancellation question with **No selected**.
The rip keeps running while you decide. Use arrows or Tab to select and Enter to
confirm, N/Esc to continue ripping, or Y to cancel. Repeated Ctrl+C does not
bypass confirmation. Confirming Ctrl+C cancellation exits after cleanup;
confirming Enter/Esc cancellation shows the canceled result. If the job finishes
while the question is open, its result appears automatically. Esc in the full log
viewer closes logs first.

The resulting file is named `Movie Title (Year)/Movie Title (Year).mkv`.
This workflow handles one movie title per run; TV episodes and multiple-title
ripping are not included.

| Key | Action |
| --- | --- |
| Type | Search TMDB or filter the current selection list |
| Up/Down, Enter | Select and continue; Enter on confirmation starts ripping; during ripping, Enter asks to cancel |
| Esc | Back/exit; during ripping, confirm cancellation; during earlier tasks, cancel |
| Ctrl+C | During ripping, confirm cancellation and exit; otherwise cancel and exit after cleanup |
| F2 | Open/close the full log viewer |
| L | Open/close logs while ripping or on the result screen |
| Up/Down in logs | Scroll within the current page |
| PageUp/PageDown, Home/End | Navigate the full on-disk log |
| R in logs | Refresh the current page |

The header shows used and total storage for the movie destination filesystem,
with a percentage and units selected by the top-level `storage_units` setting:

- `"decimal"` (default): powers of 1,000, displayed as KB/MB/GB/TB.
  One TB is 1,000,000,000,000 bytes.
- `"binary"`: powers of 1,024, displayed as KiB/MiB/GiB/TiB.
  One TiB is 1,099,511,627,776 bytes.

For example, the same 2,000,000,000,000 bytes displays as **2 TB** or **1.82 TiB**.
Only the display changes; the capacity and percentage stay the same. Existing
configs without `storage_units` use decimal without prompting or rewriting the
file. Edit this setting to change the display on the next run. An explicit
`--movies-path` still works with an unreadable or invalid config, using decimal
units in that case.

The indicator refreshes in the background every
five seconds, including while ripping. For SSHFS destinations it queries the
mounted remote filesystem, so the reported capacity may reflect server quotas
or limits. If statistics are unavailable, the header shows `Storage: unavailable`;
this does not interrupt ripping. On narrow terminals the indicator shortens or
hides to keep the title centered.

Logs are retained at `$XDG_STATE_HOME/discora/logs`, or
`~/.local/state/discora/logs` when `XDG_STATE_HOME` is unset. The log path is
shown in the interface and printed on exit. On Windows, logs live at
`%LOCALAPPDATA%\discora\logs` (with native Known Folder fallback). Only a bounded
recent log buffer is kept in memory; the full viewer reads the disk log in pages.

Each rip writes to a unique `.discora-rip-*` directory inside the movie
directory. After MakeMKV succeeds, exactly one nonempty, regular MKV must exist
before it is published. Existing final filenames are never replaced, including
when another process creates one during the rip. Local publication uses a hard
link and requires a filesystem that supports hard links. Windows SSHFS uses a server-side SFTP hard link because WinFsp does not
expose hard links to Windows. Existing extras are left alone.

On failure, confirmed rip cancellation, cancellation of earlier tasks, or SIGTERM
on Unix, the application stops and reaps MakeMKV, deletes only that attempt's temporary output, and retains the log. Cleanup
failures report the remaining path. Forced termination or power loss can leave
staging directories behind; these are not automatically deleted on later runs.
An empty movie directory may remain after a failed attempt.

The application orchestrates MakeMKV through its
[robot interface](https://www.makemkv.com/developers/usage.txt); it does not decode discs itself.

## Rip directly to another computer over SSHFS

The ripping computer uses its own optical drive and writes the MKV directly to
your computer over SFTP, without saving a complete local copy. Discora manages
the SSHFS mount and unmount automatically; no manual mounting is needed.
The ripping computer can run Linux or Windows.

On Linux, install `sshfs` and FUSE (`fusermount3`, or `fusermount`) alongside
MakeMKV. FUSE must be available to your user. On Windows, install
[WinFsp](https://winfsp.dev/) and [SSHFS-Win](https://github.com/winfsp/sshfs-win).
Discora discovers `sshfs-win.exe` on PATH or under the standard SSHFS-Win
installation folders, selects an unused drive letter, and owns the foreground
mount process. No manual mapping or WSL is needed. On the receiving computer, run an SSH/SFTP server with an existing movie directory
writable by the upload account. The server/filesystem must support SFTP hard
links and file syncing (`hardlink@openssh.com` and `fsync@openssh.com` for a
Windows client). Discora checks these operations before opening the rip
interface and refuses to start if they are unavailable.

Configure SSH key authentication or an SSH agent, then verify the receiving
computer's host key through a trusted channel and add it to `known_hosts`.
For example, connect once using `ssh -p 22 movie-upload@192.168.1.50` and verify
the fingerprint before accepting it. Discora requires a known host key and uses
noninteractive SSH authentication; it does not store passwords or prompt for
key passphrases. Load encrypted keys into your SSH agent first.

On Windows, mounting and SFTP publication use SSHFS-Win's bundled SSH client,
not Windows OpenSSH. Both require verified host keys in the current user's
`%USERPROFILE%\.ssh\known_hosts`. Use the bundled client when testing authentication
and verifying the server fingerprint:

```powershell
& 'C:\Program Files\SSHFS-Win\bin\ssh.exe' -o 'UserKnownHostsFile="/cygdrive/c/Users/YourName/.ssh/known_hosts"' -p 22 movie-upload@192.168.1.50
```

Adjust the installation and user paths. Do not accept a host key without checking
its fingerprint through a trusted channel. For a custom key, add
`identity_file = 'C:\Users\YourName\.ssh\id_ed25519'` to the destination config.
Discora translates local Windows paths for the bundled Cygwin SSH environment;
`movies_path` remains an absolute POSIX path on the server. Paths used as SSH
options cannot contain commas, quotes, or control characters. Native Windows
OpenSSH agent bridging is not included: use a key or an agent compatible with
the bundled SSH client. The bundled client's default identities use its own home
configuration; an explicit `identity_file` is the most predictable setup.

The Windows mount presents ownership as the mounting Windows user/group
(`uid=-1,gid=-1`), matching SSHFS-Win's default service mapping. The receiving
server still enforces the SSH account's permissions and ACLs for every operation.

Windows connection logs include verbose bundled SSH diagnostics, executable and
key/known-hosts paths, SFTP initialization and process status, and SSHFS mount
progress. Use F2 or the on-disk log above to inspect connection failures. Logs
include usernames, host addresses, and local paths, but do not log private-key
contents.
For launcher diagnostics, set `$env:DISCORA_SSH_PROBE = '1'` in PowerShell
before running Discora. This additionally runs the bundled `ssh -V` through the
same Windows process launcher and records its output and hexadecimal exit code
before attempting the normal connection. Remove the variable with
`Remove-Item Env:DISCORA_SSH_PROBE` when finished.

The friend's `discora.toml` can contain only:

```toml
[rip.destination]
type = "sshfs"
host = "192.168.1.50"
port = 22 # Optional; defaults to 22
username = "movie-upload"
movies_path = "/media/Movies" # Absolute path on the receiving computer

# Optional; otherwise use the SSH agent/default identities
identity_file = "/home/friend/.ssh/id_ed25519"
```

Run `discora` or `discora rip` normally. No local movie or TV path is required
for remote ripping. Movie selection checks the shared destination before disc
scanning, and publication still refuses to replace existing files, even if two
people rip the same movie concurrently. There is no shared reservation to prevent
that duplicated work yet.

To rip locally for one invocation, use `discora rip --movies-path /local/Movies`.
An explicit movie path always selects local output and bypasses the saved remote
destination. To switch back permanently, use the local config shown above.
`mkdir` and `file-cleanup` continue to use local top-level library paths and do
not mount the remote destination.

The connection stays open through disc scanning, ripping, and finalization.
Movie data and unique `.discora-rip-*` staging directories live on the receiving
computer; the ripping computer stores only session logs and a temporary empty
mountpoint. Network failures fail the rip rather than reconnecting and resuming
open files. You may need to rip again, and a disconnection can leave remote
partial output; the error/log reports the affected paths. Unmount failures report the affected mount instead of deleting its contents.
Windows disconnects only the mount process tree it started and checks that the
drive disappears. Linux reports a manual unmount command when needed. Forced
termination can also leave a mount behind.

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
or MakeMKV installation is needed. The shared process tests require Python 3 on PATH (or `DISCORA_TEST_PYTHON` set
to its executable) and run on both Linux and Windows. Linux SSHFS lifecycle tests
remain Linux-only. CI runs formatting, Clippy, and tests on both operating systems.
A real-disc acceptance check still requires selecting and ripping a movie on a
machine with an accessible optical drive.

For SSHFS acceptance, use a Linux client with FUSE and an SSH/SFTP server:
verify that a movie appears only on the receiver, that an existing movie is
rejected before scanning, and that successful completion/cancellation unmounts
the library. Interrupt the network during a rip and verify failure, preserved
existing files, and actionable cleanup reporting. Automated SSHFS lifecycle
tests use fake processes and do not require FUSE or a real server.


For Windows acceptance, run both a local NTFS rip and a managed SSHFS rip to an
SSH/SFTP server supporting the required extensions. Verify the final movie,
Esc/Ctrl+C cancellation, and that a pre-existing destination survives. Create a
final filename from another client during ripping and verify publication refuses
to overwrite it. Interrupt the network during ripping and publication; confirm
failure reports the remote paths, retains logs, and releases Discora's drive while
leaving unrelated mounts intact. If publication loses its acknowledgement, inspect
the reported final path before retrying. Repeat with spaces in the key, library,
and executable paths. These live checks require a Windows machine and optical
drive; the fake-process suite does not certify a real WinFsp installation.

After movie selection, Discora creates the movie directory and tests writing,
syncing, and hard-link publication before discovering or scanning the disc.
An empty movie directory may remain if you cancel.

By default, exactly one scanned title with a positive chapter count within two minutes of the selected
movie's TMDB runtime starts ripping automatically, without confirmation.
Set `rip.auto_select_runtime_match = false` to always choose manually.
Missing runtime, lookup failures, and multiple matching titles keep manual
selection and confirmation. Runtime matching is a heuristic and may select
a different edition. The title-selection status includes a MakeMKV forum
search URL for investigating ambiguous discs.
