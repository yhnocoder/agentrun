use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const TEST_LIMIT: Duration = Duration::from_secs(60);

macro_rules! test_list {
    ($($(#[$attr:meta])* $name:ident),* $(,)?) => {
        vec![$($(#[$attr])* (stringify!($name), $name as fn())),*]
    };
}
pub(crate) use test_list;

pub fn run_tests(tests: Vec<(&'static str, fn())>) {
    let mut list = false;
    let mut exact = false;
    let mut quiet = false;
    let mut ignored = false;
    let mut filters: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => list = true,
            "--exact" => exact = true,
            "--quiet" | "-q" => quiet = true,
            "--ignored" => ignored = true,
            "--nocapture" | "--show-output" | "--include-ignored" => {}
            "--test-threads" => {
                args.next();
            }
            _ if arg.starts_with("--test-threads=") => {}
            _ if arg.starts_with('-') => {
                eprintln!(
                    "error: Unrecognized option: '{}'",
                    arg.trim_start_matches('-')
                );
                std::process::exit(101);
            }
            _ => filters.push(arg),
        }
    }
    let mut selected: Vec<(&'static str, fn())> = tests
        .into_iter()
        .filter(|(name, _)| {
            !ignored
                && (filters.is_empty()
                    || filters.iter().any(|filter| {
                        if exact {
                            name == filter
                        } else {
                            name.contains(filter.as_str())
                        }
                    }))
        })
        .collect();
    if list {
        selected.sort_by_key(|(name, _)| *name);
        for (name, _) in &selected {
            println!("{name}: test");
        }
        if !quiet {
            if !selected.is_empty() {
                println!();
            }
            let count = selected.len();
            let noun = if count == 1 { "test" } else { "tests" };
            println!("{count} {noun}, 0 benchmarks");
        }
        return;
    }
    println!("\nrunning {} tests", selected.len());
    let deadline = Instant::now() + TEST_LIMIT;
    let (sender, results) = mpsc::channel();
    for (index, (name, test)) in selected.iter().copied().enumerate() {
        let sender = sender.clone();
        thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let _ = sender.send((index, std::panic::catch_unwind(test)));
            })
            .unwrap();
    }
    drop(sender);
    let mut pending = vec![true; selected.len()];
    let mut failed = 0;
    while pending.contains(&true) {
        let Ok((index, result)) =
            results.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        else {
            break;
        };
        pending[index] = false;
        let name = selected[index].0;
        match result {
            Ok(()) => println!("test {name} ... ok"),
            Err(payload) => {
                failed += 1;
                let message = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                println!("test {name} ... FAILED\n    {message}");
            }
        }
    }
    for (index, (name, _)) in selected.iter().enumerate() {
        if pending[index] {
            failed += 1;
            println!(
                "test {name} ... FAILED\n    did not finish within {} seconds",
                TEST_LIMIT.as_secs()
            );
        }
    }
    if failed > 0 {
        println!("\ntest result: FAILED. {failed} failed");
        std::process::exit(1);
    }
    println!("\ntest result: ok");
}
