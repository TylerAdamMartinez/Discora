# Reusing MakeMKV disc analysis

Investigated October 23, 2026. Recommendation: retain the CLI integration. No
documented interface was found that allows Discora to enumerate titles, wait for
user selection, and convert a selected title in the same open MakeMKV session.
This is a documentation-based feasibility finding, not proof that undocumented
interfaces cannot do it. No physical-disc or alternative-integration prototype
was tested.

## Current flow and evidence

Discora runs `makemkvcon info disc:N` for title selection. That process exits;
Discora retains its parsed metadata. Confirmation runs a new
`makemkvcon mkv disc:N TITLE OUTPUT` process. Its internal disc analysis cannot be
replaced by passing our cached metadata through any documented option. Returning
from confirmation to the title list uses the existing metadata and starts no
process.

- MakeMKV's [developer documentation](https://www.makemkv.com/developers/)
  identifies the console application as its automation interface.
- The [CLI reference](https://www.makemkv.com/developers/usage.txt) documents no
  session handle, saved analysis import, or interactive command channel.
  `--cache` controls the read buffer size; it does not persist title analysis.
  `--noscan` controls media discovery/insertion checks and does not document
  reusing the previous process's title analysis.
- [LibMMBD](https://www.makemkv.com/libmmbd/) exposes Blu-ray M2TS/SSIF decryption
  through a background MakeMKV instance. Its documented purpose is decryption
  for players, not MakeMKV title enumeration or MKV conversion; it is not a
  replacement for this workflow, including DVD support.

## Alternatives and implications

| Approach | Feasibility and tradeoffs |
| --- | --- |
| Keep the CLI | Existing Linux and Windows integration retains explicit title choice, MakeMKV audio/subtitle preferences, and the current cancellation/partial-output cleanup. Repeated opening and analysis remain. Lowest maintenance risk. |
| LibMMBD plus a new reader/muxer | A separate media pipeline would need title/playlist analysis, DVD handling, track selection, and MKV muxing. Linux and Windows packaging, preference compatibility, cancellation, and output correctness would all require new implementation and validation. Does not establish reuse of MakeMKV title analysis. |
| Automate the MakeMKV GUI or a private protocol | Could potentially retain the GUI's open-disc state, but no supported automation contract was found. Linux and Windows would need separate lifecycle/UI validation; preference handling, cancellation, upgrades, and recovery remain unproven. High maintenance risk. |
| Back up the disc before selecting | Moves substantial disc reading and storage use before selection and still requires analysis of the resulting files. Does not establish a scan-once workflow and changes cancellation/cleanup requirements. |

## Next step

Do not change ripping backends on this evidence. A future backend proposal should
first identify a supported persistent-session contract and demonstrate title
enumeration followed by conversion without reopening the disc on both Linux and
Windows. It must preserve correct title identity, audio/subtitle preferences,
cancellation, and partial-output cleanup, and measure startup time on DVD and
Blu-ray. No alternative examined here meets that bar, so no backend implementation
plan or performance improvement is promised by this change.
