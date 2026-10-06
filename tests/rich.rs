#[path = "support/harness.rs"]
mod harness;
#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::Instant;

use agentrun::cli::SandboxMode;
use agentrun::output::{Aggregator, OpenTools, Rich, TextFormatter};
use agentrun::run::{Exit, conclude, translate_line};
use agentrun::runtime::Adapter;
use support::env::Env;
use support::fake::FakeAdapter;
use support::process::{self, spawn_lock};

const COLUMNS: usize = 100;
const ROWS: usize = 30;
const RULE_CHAR: char = '─';

fn main() {
    process::take_over_if_spawned();
    harness::run_tests(tests());
}

fn tests() -> Vec<(&'static str, fn())> {
    harness::test_list![
        scroll_lines_match_text_and_panel_stays_at_bottom,
        stderr_lines_enter_the_scroll_area,
        pseudo_terminal_run_ends_with_end_line_and_visible_cursor,
        no_color_turns_off_dim_labels_in_the_terminal,
        harness_lists_its_tests_and_rejects_unknown_options,
    ]
}

struct Screen {
    lines: Vec<String>,
    row: usize,
    col: usize,
    cursor_visible: bool,
}

impl Screen {
    fn new() -> Screen {
        Screen {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            cursor_visible: true,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        let text = String::from_utf8(bytes.to_vec()).expect("terminal output is UTF-8");
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            match c {
                '\r' => self.col = 0,
                '\n' => {
                    self.row += 1;
                    self.col = 0;
                    if self.row == self.lines.len() {
                        self.lines.push(String::new());
                    }
                }
                '\x1b' => {
                    assert_eq!(chars.next(), Some('['), "only CSI sequences are expected");
                    let mut params = String::new();
                    let command = loop {
                        let c = chars.next().expect("CSI sequence is complete");
                        if c.is_ascii_alphabetic() {
                            break c;
                        }
                        params.push(c);
                    };
                    match command {
                        'A' => self.row -= params.parse::<usize>().unwrap(),
                        'J' => {
                            assert!(params.is_empty());
                            let kept: String =
                                self.lines[self.row].chars().take(self.col).collect();
                            self.lines[self.row] = kept;
                            self.lines.truncate(self.row + 1);
                        }
                        'l' => {
                            assert_eq!(params, "?25");
                            self.cursor_visible = false;
                        }
                        'h' => {
                            assert_eq!(params, "?25");
                            self.cursor_visible = true;
                        }
                        'm' => assert!(params == "2" || params == "0", "unexpected SGR {params}"),
                        other => panic!("unexpected control sequence ESC [ {params} {other}"),
                    }
                }
                c => {
                    let line = &mut self.lines[self.row];
                    let kept: String = line.chars().take(self.col).collect();
                    *line = kept;
                    line.push(c);
                    self.col += 1;
                }
            }
        }
    }

    fn rows(&self) -> Vec<&str> {
        let mut rows: Vec<&str> = self.lines.iter().map(String::as_str).collect();
        if rows.last() == Some(&"") {
            rows.pop();
        }
        rows
    }
}

fn is_rule(row: &str) -> bool {
    !row.is_empty() && row.chars().all(|c| c == RULE_CHAR)
}

struct Session {
    adapter: FakeAdapter,
    aggregator: Aggregator,
    text: TextFormatter,
    rich: Rich,
    screen: Screen,
    expected: Vec<String>,
    started: Instant,
}

impl Session {
    fn new(prompt: &str) -> Session {
        let adapter = FakeAdapter::new(false);
        let aggregator = Aggregator::new(adapter.echoes_prompt());
        let mut session = Session {
            adapter,
            aggregator,
            text: TextFormatter::new(SandboxMode::On, ""),
            rich: Rich::new(SandboxMode::On, "", false, Box::new(|| (COLUMNS, ROWS))),
            screen: Screen::new(),
            expected: Vec::new(),
            started: Instant::now(),
        };
        let events = session.aggregator.begin(prompt);
        session.emit(&events);
        session
    }

    fn emit(&mut self, events: &[agentrun::output::Event]) {
        for event in events {
            self.expected.extend(self.text.lines(event));
            let mut bytes = Vec::new();
            self.rich.event(&mut bytes, event);
            self.screen.feed(&bytes);
        }
    }

    fn push(&mut self, record: &str) {
        let events =
            translate_line(record.as_bytes(), &mut self.adapter, &mut self.aggregator).events;
        self.rich.set_open_tools(self.aggregator.open_tools());
        self.emit(&events);
    }

    fn stderr(&mut self, bytes: &[u8]) {
        let mut out = Vec::new();
        self.rich.stderr(&mut out, bytes);
        self.screen.feed(&out);
    }

    fn panel(&self) -> Vec<&str> {
        let rows = self.screen.rows();
        assert_eq!(&rows[..self.expected.len()], self.expected, "scroll area");
        let panel = rows[self.expected.len()..].to_vec();
        assert!(is_rule(panel[0]), "panel starts with a rule: {panel:?}");
        let rules = rows.iter().filter(|row| is_rule(row)).count();
        assert!(rules == 1 || rules == 2, "rules on screen: {rows:?}");
        assert!(!self.screen.cursor_visible);
        panel
    }

    fn finish(mut self) -> Screen {
        let exit = Exit {
            code: Some(0),
            signal: None,
            timed_out: false,
        };
        let aggregator = std::mem::replace(&mut self.aggregator, Aggregator::new(false));
        self.rich.set_open_tools(OpenTools::default());
        let (events, _) = conclude(aggregator, &mut self.adapter, &exit, "", self.started);
        self.emit(&events);
        let rows = self.screen.rows();
        assert_eq!(rows, self.expected, "no panel after end");
        assert!(rows.last().unwrap().starts_with("[end] finished "));
        assert!(self.screen.cursor_visible);
        self.screen
    }
}

const SUBAGENT_START: &str = r#"{"record":"subagent_start","id":"s1","parent":null,"kind":"Explore","model":"haiku","description":"look around"}"#;
const SUBAGENT_TOOL_START: &str =
    r#"{"record":"tool_start","id":"t2","parent":"s1","name":"Read","summary":"Read: a.rs"}"#;
const SUBAGENT_TOOL_END: &str = r#"{"record":"tool_end","id":"t2","denied":false}"#;
const SUBAGENT_USAGE: &str = r#"{"record":"usage","parent":"s1","model":"claude-haiku-4-5","input_tokens":3000,"output_tokens":50,"cache_read_tokens":100,"cache_write_tokens":0}"#;
const SUBAGENT_END: &str = r#"{"record":"subagent_end","id":"s1","status":"finished"}"#;
const MAIN_TEXT: &str = r#"{"record":"text","parent":null,"text":"done"}"#;

fn scroll_lines_match_text_and_panel_stays_at_bottom() {
    let mut session = Session::new("fix tests");
    assert_eq!(session.expected, vec!["[main] prompt fix tests"]);
    let panel = session.panel();
    assert_eq!(panel.len(), 2);
    assert!(panel[1].contains(" main  "), "{}", panel[1]);

    session.push(r#"{"record":"tool_start","id":"t1","parent":null,"name":"Bash","summary":"Bash: cargo check"}"#);
    let mut refresh = Vec::new();
    session.rich.refresh(&mut refresh);
    session.screen.feed(&refresh);
    let panel = session.panel();
    assert!(
        panel[1].contains(" main  Bash: cargo check  "),
        "{}",
        panel[1]
    );

    session.push(SUBAGENT_START);
    assert_eq!(
        session.expected.last().unwrap(),
        "[main] agent start explore#1 (haiku): look around"
    );
    let panel = session.panel();
    assert_eq!(panel.len(), 5);
    assert_eq!(panel[1], "subagent  1 running");
    assert!(
        panel[2].contains(" explore#1  haiku  look around  "),
        "{}",
        panel[2]
    );
    assert!(is_rule(panel[3]));

    session.push(SUBAGENT_TOOL_START);
    session.push(SUBAGENT_USAGE);
    let panel = session.panel();
    assert!(
        panel[2].contains(" explore#1  claude-haiku-4-5  look around  Read: a.rs  "),
        "{}",
        panel[2]
    );
    assert!(panel[2].ends_with("  ctx 3.1k"), "{}", panel[2]);
    assert!(panel[4].ends_with("  in 3.1k  cached 3%"), "{}", panel[4]);

    session.push(SUBAGENT_TOOL_END);
    assert_eq!(
        session.expected.last().unwrap(),
        "[explore#1] tool Read: a.rs"
    );
    session.push(SUBAGENT_END);
    assert!(
        session
            .expected
            .last()
            .unwrap()
            .starts_with("[explore#1] agent finished ")
    );
    let panel = session.panel();
    assert_eq!(panel.len(), 4);
    assert_eq!(panel[1], "subagent  0 running");

    session.push(MAIN_TEXT);
    assert_eq!(session.expected.last().unwrap(), "[main] text done");
    session.push(r#"{"record":"tool_end","id":"t1","denied":false}"#);
    assert_eq!(
        session.expected.last().unwrap(),
        "[main] tool Bash: cargo check"
    );
    session.panel();

    let screen = session.finish();
    assert_eq!(screen.rows().len(), 7);
}

fn stderr_lines_enter_the_scroll_area() {
    let mut session = Session::new("hi");
    session.push(SUBAGENT_START);
    session.stderr(b"warning: first\nwarning: sec");
    session.expected.push("warning: first".to_string());
    let panel = session.panel();
    assert_eq!(panel.len(), 5);
    session.stderr(b"ond\n");
    session.expected.push("warning: second".to_string());
    session.panel();
    session.stderr(b"tail without newline");
    session.panel();
    session.push(SUBAGENT_END);
    session.expected.push("tail without newline".to_string());
    let screen = session.finish();
    let rows = screen.rows();
    assert_eq!(rows[rows.len() - 2], "tail without newline");
}

fn run_in_pty(variables: &[(&str, &str)]) -> Vec<u8> {
    let env = Env::new();
    env.install(
        "claude",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' '{SUBAGENT_START}'\nprintf '%s\\n' '{SUBAGENT_TOOL_START}'\necho 'oops' >&2\nprintf '%s\\n' '{SUBAGENT_TOOL_END}'\nprintf '%s\\n' '{SUBAGENT_END}'\nprintf '%s\\n' '{MAIN_TEXT}'\n"
        ),
    );
    let (mut master, slave) = support::open_pty(ROWS as u16, COLUMNS as u16);
    let work = env.work();
    let mut command = env.fake_command(&[
        "claude-code",
        "--sandbox",
        "off",
        "--prompt",
        "hi",
        "--cwd",
        work.to_str().unwrap(),
    ]);
    command
        .envs(variables.iter().copied())
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    let mut child = {
        let _guard = spawn_lock();
        command.spawn().unwrap()
    };
    drop(command);

    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match master.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
        }
    }
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
    bytes
}

fn rows_without_stderr(bytes: &[u8]) -> Vec<String> {
    let mut screen = Screen::new();
    screen.feed(bytes);
    assert!(screen.cursor_visible);
    let rows = screen.rows();
    assert!(rows.iter().all(|row| !is_rule(row)), "{rows:?}");
    assert!(rows.contains(&"oops"), "{rows:?}");
    rows.iter()
        .filter(|row| **row != "oops")
        .map(|row| row.to_string())
        .collect()
}

fn pseudo_terminal_run_ends_with_end_line_and_visible_cursor() {
    let bytes = run_in_pty(&[]);
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("\x1b[?25l"), "cursor was hidden: {text:?}");
    assert!(text.contains("subagent  1 running"), "{text:?}");
    assert!(text.contains("\x1b[2m[main]\x1b[0m prompt hi"), "{text:?}");
    assert!(text.ends_with("\x1b[?25h"), "{text:?}");

    let without_stderr = rows_without_stderr(&bytes);
    assert!(without_stderr[0].starts_with("[note] sandbox not running (--sandbox off)."));
    assert_eq!(without_stderr[1], "[main] prompt hi");
    assert_eq!(
        without_stderr[2],
        "[main] agent start explore#1 (haiku): look around"
    );
    assert_eq!(without_stderr[3], "[explore#1] tool Read: a.rs");
    assert!(without_stderr[4].starts_with("[explore#1] agent finished "));
    assert_eq!(without_stderr[5], "[main] text done");
    assert!(without_stderr[6].starts_with("[end] finished "));
    assert_eq!(without_stderr.len(), 7);
}

fn no_color_turns_off_dim_labels_in_the_terminal() {
    let bytes = run_in_pty(&[("NO_COLOR", "1")]);
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("\x1b[?25l"), "cursor was hidden: {text:?}");
    assert!(text.ends_with("\x1b[?25h"), "{text:?}");
    assert!(!text.contains("\x1b[2m"), "{text:?}");
    assert!(!text.contains("\x1b[0m"), "{text:?}");
    let without_stderr = rows_without_stderr(&bytes);
    assert_eq!(without_stderr.len(), 7, "{without_stderr:?}");
    assert_eq!(without_stderr[1], "[main] prompt hi");
    assert!(without_stderr[6].starts_with("[end] finished "));

    let bytes = run_in_pty(&[("NO_COLOR", "")]);
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("\x1b[2m[main]\x1b[0m prompt hi"), "{text:?}");
}

fn run_harness(args: &[&str]) -> Output {
    let child = {
        let _guard = spawn_lock();
        Command::new(std::env::current_exe().unwrap())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    child.wait_with_output().unwrap()
}

fn harness_stdout(args: &[&str]) -> String {
    let output = run_harness(args);
    assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn harness_lists_its_tests_and_rejects_unknown_options() {
    let mut names: Vec<&str> = tests().into_iter().map(|(name, _)| name).collect();
    names.sort();
    let lines: String = names.iter().map(|name| format!("{name}: test\n")).collect();
    assert_eq!(
        harness_stdout(&["--list"]),
        format!("{lines}\n{} tests, 0 benchmarks\n", names.len())
    );
    assert_eq!(
        harness_stdout(&["--list", "--exact", "stderr_lines_enter_the_scroll_area"]),
        "stderr_lines_enter_the_scroll_area: test\n\n1 test, 0 benchmarks\n"
    );
    assert_eq!(
        harness_stdout(&["--list", "scroll"]),
        "scroll_lines_match_text_and_panel_stays_at_bottom: test\nstderr_lines_enter_the_scroll_area: test\n\n2 tests, 0 benchmarks\n"
    );
    assert_eq!(harness_stdout(&["--list", "--quiet"]), lines);
    assert_eq!(
        harness_stdout(&["--list", "no_such_test"]),
        "0 tests, 0 benchmarks\n"
    );
    assert_eq!(
        harness_stdout(&["--list", "--ignored"]),
        "0 tests, 0 benchmarks\n"
    );
    let output = run_harness(&["--bogus"]);
    assert_eq!(output.status.code(), Some(101), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "error: Unrecognized option: 'bogus'\n"
    );
}
