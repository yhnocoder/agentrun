use std::thread;

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
    let handles: Vec<_> = selected
        .into_iter()
        .map(|(name, test)| (name, thread::spawn(test)))
        .collect();
    let mut failed = 0;
    for (name, handle) in handles {
        match handle.join() {
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
    if failed > 0 {
        println!("\ntest result: FAILED. {failed} failed");
        std::process::exit(1);
    }
    println!("\ntest result: ok");
}
