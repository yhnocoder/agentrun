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
    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    let selected: Vec<(&'static str, fn())> = tests
        .into_iter()
        .filter(|(name, _)| filters.is_empty() || filters.iter().any(|f| name.contains(f.as_str())))
        .collect();
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
