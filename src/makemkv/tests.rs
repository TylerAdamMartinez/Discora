use super::*;
use crate::{files, rip};
use std::{fs, time::Instant};

fn fake(body: &str) -> (tempfile::TempDir, MakeMkv, RunLog) {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("fake makemkvcon");
    fs::write(
        &executable,
        format!("import os, sys, time\nfrom pathlib import Path\n{body}\n"),
    )
    .unwrap();
    let log = RunLog::create(root.path()).unwrap();
    // Invoke the interpreter directly: unrelated parallel forks can briefly inherit
    // a fixture writer fd, causing ETXTBSY if the script itself is exec'd.
    let mkv = MakeMkv::with_arguments(
        crate::test_support::python(),
        vec![executable.into_os_string()],
    );
    (root, mkv, log)
}

#[test]
fn robot_parser_handles_escaped_strings_and_real_title_field_order() {
    assert_eq!(
        parse(r#"TINFO:7,2,0,"A title, with \"quotes\" and \\ slash\nline""#),
        Record::Title {
            id: 7,
            attribute: 2,
            value: "A title, with \"quotes\" and \\ slash\nline".into()
        }
    );
    assert_eq!(
        parse("PRGV:34000,68000,65536"),
        Record::Progress {
            total: 68000,
            max: 65536
        }
    );
    assert_eq!(
        parse(r#"MSG:1005,0,1,"Opened disc, ready","%1","ready""#),
        Record::Message {
            code: 1005,
            text: "Opened disc, ready".into()
        }
    );
    for line in [
        "PRGV:1,2",
        "PRGV:x,2,3",
        "TINFO:7,9,0,\"unfinished",
        "FUTURE:1,2,3",
        "PRGV:1,-1,2",
        "PRGV:1,18446744073709551616,2",
    ] {
        assert_eq!(parse(line), Record::Unknown, "{line}");
    }
    assert_eq!(parse("PRGV:1,2,0"), Record::Progress { total: 2, max: 0 });
}

#[test]
fn discovery_and_scan_handle_exit_status_and_sort_titles() {
    let (_root, mkv, log) = fake(
        r#"
assert sys.argv[1:4] == ['-r', '--messages=-stdout', '--progress=-same']
if 'disc:9999' in sys.argv:
    print('DRV:0,2,999,1,"Optical drive, USB","DISC","/dev/sr0"')
    print('DRV:1,0,0,0,"Unavailable",""')
    print('DRV:2,256,999,0,"","",""')
    sys.exit(1)
print('TINFO:0,9,0,"0:05:00"')
print('TINFO:0,10,0,"500 MB"')
print('TINFO:3,2,0,"Main movie"')
print('TINFO:3,9,0,"1:48:55"')
print('TINFO:3,10,0,"25.3 GB"')
print('TINFO:3,16,0,"00800.mpls"')
print('TINFO:3,8,0,"16"')
print('TINFO:3,26,0,"501,502,503"')
"#,
    );
    let cancel = AtomicBool::new(false);
    let drives = mkv.drives(&cancel, &log, &mut |_| {}).unwrap();
    assert_eq!(drives.len(), 1);
    assert_eq!(drives[0].name, "Optical drive, USB");
    let titles = mkv.titles(0, &cancel, &log, &mut |_| {}).unwrap();
    assert_eq!(titles.iter().map(|t| t.id).collect::<Vec<_>>(), [3, 0]);
    assert_eq!(titles[0].seconds(), 6535);
    assert_eq!(titles[0].source, "00800.mpls");
    assert_eq!(titles[0].chapters, "16");
    assert_eq!(titles[0].segment_map, "501,502,503");
    assert!(titles[0].to_string().contains("Chapters: 16"));
    assert!(titles[0].to_string().contains("Segments: 501,502,503"));
    assert!(titles[1].to_string().contains("Chapters: —"));
}

#[test]
fn successful_rip_drains_both_streams_and_publishes_only_new_movie() {
    let (root, mkv, log) = fake(
        r#"
assert 'mkv' in sys.argv and 'disc:2' in sys.argv
assert sys.argv[-2] == '7'
destination = Path(sys.argv[-1])
(destination / 'title00.mkv').write_bytes(b'movie data')
for i in range(1000):
    print(f'stderr {i}', file=sys.stderr)
print('UNKNOWN:kept verbatim')
print('PRGT:5017,0,"Saving title"')
print('PRGV:1,2,0')
print('PRGV:65536,65536,65536')
print('MSG:5005,0,0,"1 titles saved",""')
"#,
    );
    let movie = root.path().join("Movie with spaces (2005)");
    fs::create_dir_all(&movie).unwrap();
    let extra = movie.join("bonus.mkv");
    fs::write(&extra, "existing extra").unwrap();
    let mut events = Vec::new();
    let path = rip::run(
        &mkv,
        rip::Output::local(&movie),
        2,
        7,
        &AtomicBool::new(false),
        &log,
        &mut |event| events.push(event),
    )
    .unwrap();
    assert_eq!(path, files::named_mkv(&movie).unwrap());
    assert_eq!(fs::read_to_string(path).unwrap(), "movie data");
    assert_eq!(fs::read_to_string(extra).unwrap(), "existing extra");
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Progress {
            total: 65536,
            max: 65536
        }
    )));
    let text = fs::read_to_string(&log.path).unwrap();
    assert!(text.contains("stderr 999"));
    assert!(text.contains("UNKNOWN:kept verbatim"));
    assert_eq!(fs::read_dir(movie).unwrap().count(), 2);
}

#[test]
fn failed_and_unexpected_rips_delete_only_current_staging_and_keep_logs() {
    for body in [
        "Path(sys.argv[-1], 'partial.mkv').write_text('partial')\nsys.exit(1)",
        "pass",
        "Path(sys.argv[-1], 'empty.mkv').touch()",
        "Path(sys.argv[-1], 'a.mkv').write_text('a')\nPath(sys.argv[-1], 'b.mkv').write_text('b')",
        "Path(sys.argv[-1], 'partial.mkv').write_text('partial')\nprint('MSG:5004,0,0,\"0 titles saved, 1 failed\",\"\"')",
    ] {
        let (root, mkv, log) = fake(body);
        let movie = root.path().join("Movie (2005)");
        fs::create_dir_all(&movie).unwrap();
        let old_staging = movie.join(".discora-rip-old");
        fs::create_dir(&old_staging).unwrap();
        fs::write(old_staging.join("old.mkv"), "old partial").unwrap();
        assert!(
            rip::run(
                &mkv,
                rip::Output::local(&movie),
                0,
                0,
                &AtomicBool::new(false),
                &log,
                &mut |_| {},
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(&movie).unwrap().count(), 1);
        assert!(old_staging.join("old.mkv").exists());
        assert!(log.path.exists());
        assert!(!files::named_mkv(&movie).unwrap().exists());
    }
}

#[test]
fn cancellation_reaps_child_before_partial_output_is_deleted() {
    let (root, mkv, log) = fake(
        r#"
Path(sys.argv[-1], 'partial.mkv').write_text('partial')
print(f'READY:{os.getpid()}', flush=True)
while True:
    print('PRGV:1,1,65536', flush=True)
    time.sleep(0.01)
"#,
    );
    let movie = root.path().join("Movie (2005)");
    let cancel = AtomicBool::new(false);
    let mut pid = None;
    let result = rip::run(
        &mkv,
        rip::Output::local(&movie),
        0,
        0,
        &cancel,
        &log,
        &mut |event| {
            if let Event::Log(line) = event
                && let Some(value) = line.strip_prefix("READY:")
            {
                pid = Some(value.parse::<i32>().unwrap());
                cancel.store(true, Ordering::Relaxed);
            }
        },
    );
    assert!(result.unwrap_err().to_string().contains("Canceled"));
    assert!(!crate::test_support::process_exists(pid.unwrap() as u32));
    assert_eq!(fs::read_dir(movie).unwrap().count(), 0);
    assert!(log.path.exists());
}

#[test]
fn missing_executable_and_scan_failure_are_errors() {
    let (_root, mkv, log) = fake("sys.exit(1)");
    assert!(
        mkv.titles(0, &AtomicBool::new(false), &log, &mut |_| {})
            .is_err()
    );
    assert!(
        mkv.drives(&AtomicBool::new(false), &log, &mut |_| {})
            .is_err()
    );
    let missing = MakeMkv::new(PathBuf::from("/nonexistent/discora-makemkvcon"));
    assert!(
        missing
            .titles(0, &AtomicBool::new(false), &log, &mut |_| {})
            .is_err()
    );
}

#[test]
fn a_helper_holding_pipes_open_cannot_hide_parent_failure() {
    let (root, mkv, log) = fake(
        r#"
import subprocess
subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(5)'])
Path(sys.argv[-1], 'partial.mkv').write_text('partial')
sys.exit(1)
"#,
    );
    let movie = root.path().join("Movie (2005)");
    let start = Instant::now();
    assert!(
        rip::run(
            &mkv,
            rip::Output::local(&movie),
            0,
            0,
            &AtomicBool::new(false),
            &log,
            &mut |_| {},
        )
        .is_err()
    );
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(fs::read_dir(movie).unwrap().count(), 0);
}
