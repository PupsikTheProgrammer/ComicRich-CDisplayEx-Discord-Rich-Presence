//! End-to-end check on a real desktop: opens a comic the way a double-click
//! does, with CDisplayEx and Discord running, and times the presence updates
//! from the log. Opens and closes a CDisplayEx window, so it only runs on request:
//!
//! ```text
//! set COMICRICH_E2E_COMIC=D:\Comic Books\Some Comic 001.cbz
//! cargo test --test e2e -- --ignored --nocapture
//! ```
//!
//! The comic is opened twice: the second time its metadata comes from the cache.
//! Settings (`comicrich.toml`) are read from next to the test build of the program.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(45);
/// Without a cover yet, the file name is shown.
const FIRST_PRESENCE_WITHIN: Duration = Duration::from_secs(3);
/// From the cache, the cover is shown with the first or second update.
const CACHED_COVER_WITHIN: Duration = Duration::from_millis(1500);

struct Run {
    log: String,
    first_presence: Option<Duration>,
    cover: Option<Duration>,
}

#[test]
#[ignore = "opens CDisplayEx; needs COMICRICH_E2E_COMIC and a running Discord"]
fn double_click_shows_the_cover() {
    let Some(comic) = std::env::var_os("COMICRICH_E2E_COMIC").map(PathBuf::from) else {
        panic!("set COMICRICH_E2E_COMIC to a comic file");
    };
    assert!(comic.is_file(), "no such comic: {}", comic.display());

    let first = open(&comic, "first");
    report("first open", &first);
    assert!(first.log.contains("resolved: command line"), "comic was not identified:\n{}", first.log);
    assert!(first.log.contains("connected to Discord"), "Discord is not running:\n{}", first.log);
    let shown = first.first_presence.expect("no presence was shown");
    assert!(shown <= FIRST_PRESENCE_WITHIN, "first presence took {shown:?}");
    assert!(first.cover.is_some(), "no cover was found online:\n{}", first.log);

    let second = open(&comic, "second");
    report("second open (cached)", &second);
    let cover = second.cover.expect("cached cover was not shown");
    assert!(cover <= CACHED_COVER_WITHIN, "cached cover took {cover:?}");
}

/// Opens `comic`, waits for its cover, closes everything and returns the log.
fn open(comic: &Path, label: &str) -> Run {
    let log_path = std::env::temp_dir().join(format!("comicrich-e2e-{}-{label}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_path);
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_comicrich"))
        .arg("--open")
        .arg(comic)
        .env("COMICRICH_LOG", &log_path)
        .spawn()
        .expect("start comicrich");

    let mut run = Run { log: String::new(), first_presence: None, cover: None };
    while started.elapsed() < WAIT && run.cover.is_none() {
        std::thread::sleep(Duration::from_millis(50));
        run.log = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(!run.log.contains("another instance is already running"), "close the running ComicRich first");
        if run.first_presence.is_none() && run.log.contains("presence updated: Some(") {
            run.first_presence = Some(started.elapsed());
        }
        if run.log.lines().any(|l| l.contains("presence updated: Some(") && l.contains("large_image: Some(")) {
            run.cover = Some(started.elapsed());
        }
    }
    close(&run.log, &mut child);
    run
}

fn close(log: &str, child: &mut Child) {
    for pid in log.lines().filter_map(|l| l.split("CDisplayEx found (pid=").nth(1)?.split([',', ')']).next()) {
        // A normal close, so CDisplayEx remembers the page as usual.
        let _ = Command::new("taskkill").args(["/PID", pid]).output();
    }
    std::thread::sleep(Duration::from_secs(1));
    let _ = child.kill();
    let _ = child.wait();
}

fn report(label: &str, run: &Run) {
    println!("--- {label}: presence after {:?}, cover after {:?}", run.first_presence, run.cover);
    for line in run.log.lines() {
        println!("    {}", line.chars().take(200).collect::<String>());
    }
}
