#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod log;
mod archive;
mod config;
mod engine;
mod http;
mod install;
mod meta;
mod presence;
mod rate;
mod resolver;
mod setup;
mod watcher;
mod win;

use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use engine::Input;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--list") {
        log::enable_console();
        args[i + 1..].iter().for_each(|p| list_archive(p.as_ref()));
        return;
    }
    if let Some(pid) = args.iter().position(|a| a == "--open-files").and_then(|i| args.get(i + 1)?.parse().ok()) {
        // Diagnostics: comic files a process holds open, and its command-line file.
        log::enable_console();
        let started = std::time::Instant::now();
        println!("open: {:?} in {:?}", watcher::locks::open_comics(pid), started.elapsed());
        println!("command line: {:?}", watcher::locks::command_line_comic(pid));
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--lookup") {
        // Diagnostics: full metadata pipeline (online included) for the given files.
        log::enable_console();
        let config = config::Config::load();
        for path in &args[i + 1..] {
            let path = std::path::PathBuf::from(path);
            let Ok(listing) = archive::list(&path) else { continue };
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(meta::worker::Job { path, listing: Arc::new(listing), refresh: true });
            drop(tx);
            let config = config.clone();
            thread::spawn(move || meta::worker::run(rx, &config, |_, _| {})).join().unwrap();
        }
        return;
    }
    if let Some(path) = args.iter().position(|a| a == "--hash-pages").and_then(|i| args.get(i + 1)) {
        // Diagnostics for the cost of cover matching, at the worker's background priority.
        log::enable_console();
        let path = std::path::PathBuf::from(path);
        let listing = Arc::new(archive::list(&path).expect("readable archive"));
        let job = meta::worker::Job { path, listing, refresh: true };
        thread::spawn(move || {
            meta::worker::background_mode();
            meta::worker::hash_pages(&job);
        })
        .join()
        .unwrap();
        return;
    }
    if args.len() == 1 {
        // A double-click on the program itself: install, update or open the settings.
        setup::run();
        return;
    }
    if args.iter().any(|a| a == "--uninstall") && args.iter().any(|a| a == "--gui") {
        // Uninstalling from Settings → Apps → Installed apps.
        setup::uninstall();
        return;
    }
    for (flag, action) in [("--install", install::install as fn() -> _), ("--uninstall", install::uninstall)] {
        if args.iter().any(|a| a == flag) {
            log::enable_console();
            match action() {
                Ok(report) => println!("{report}"),
                Err(e) => {
                    println!("error: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
    }
    if args.iter().any(|a| a == "--debug") {
        log::enable_console();
    }

    // `--open <file>` is what a double-click on a comic runs: start CDisplayEx
    // first so the reader never waits, then follow it and exit when it is closed.
    let opened = args.iter().position(|a| a == "--open").and_then(|i| args.get(i + 1));
    if let Some(file) = opened {
        if let Err(e) = install::launch_cdisplayex(file) {
            log!("{e}");
        }
    }
    let Some(instance) = win::Instance::acquire() else {
        log!("another instance is already running");
        win::request_keepalive();
        return;
    };

    let config = config::Config::load();
    log!(
        "ComicRich {} started (app_id={}, language={:?}, config={:?}, exits when idle: {})",
        env!("CARGO_PKG_VERSION"),
        config.app_id,
        config.language,
        config::Config::path(),
        opened.is_some()
    );

    let (presence_tx, presence_rx) = mpsc::channel();
    let app_id = config.app_id.clone();
    thread::spawn(move || presence::run(app_id, presence_rx));

    let (input_tx, input_rx) = mpsc::channel();
    let (job_tx, job_rx) = mpsc::channel();
    let meta_tx = input_tx.clone();
    let worker_config = config.clone();
    thread::spawn(move || {
        meta::worker::run(job_rx, &worker_config, move |path, meta| {
            let _ = meta_tx.send(Input::Meta(path, meta));
        })
    });

    let language = config.language;
    thread::spawn(move || engine::run(input_rx, presence_tx, job_tx, language));

    let registry_tx = input_tx.clone();
    watcher::registry::watch(move || {
        let _ = registry_tx.send(Input::Watch(watcher::WatchEvent::Registry));
    });

    // The window watcher needs a message loop, so it owns the main thread.
    let exit_when_idle = opened.is_some();
    watcher::run(
        move |event| {
            let _ = input_tx.send(Input::Watch(event));
        },
        idle_exit(instance, exit_when_idle),
    );
}

/// How long to stay after the last CDisplayEx window closes, so moving on to the
/// next issue keeps the Discord connection.
const IDLE_GRACE: Duration = Duration::from_secs(15);

/// Ends the process once no CDisplayEx window has been open for `IDLE_GRACE`.
/// Exiting closes the Discord connection, which clears the presence.
fn idle_exit(mut instance: win::Instance, enabled: bool) -> impl FnMut(usize) {
    let mut idle_since = Instant::now();
    move |windows| {
        if windows > 0 || instance.keepalive_requested() || !enabled {
            idle_since = Instant::now();
            return;
        }
        if idle_since.elapsed() >= IDLE_GRACE {
            if instance.release_for_exit() {
                log!("no CDisplayEx window for {IDLE_GRACE:?}, exiting");
                std::process::exit(0);
            }
            idle_since = Instant::now();
        }
    }
}

/// `--list <files...>`: diagnostics for archive parsing and file metadata.
fn list_archive(path: &std::path::Path) {
    let started = std::time::Instant::now();
    match archive::list(path) {
        Ok(l) => {
            let l = Arc::new(l);
            let m = meta::local(path, &l);
            println!(
                "OK   {:>4} pages {:>6.0?}  {:?} [{}] #{} vol={:?} year={:?} of={:?} contents={} segments={}  {}",
                l.pages.len(),
                started.elapsed(),
                m.kind,
                m.title,
                m.number.as_deref().unwrap_or("-"),
                m.volume,
                m.year,
                m.count,
                m.contents.len(),
                m.segments.len(),
                path.display()
            );
        }
        Err(e) => println!("FAIL {e}  {}", path.display()),
    }
}
