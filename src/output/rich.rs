use std::io::Write;
use std::time::{Duration, Instant};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::event::{Body, Event};
use super::text::TextFormatter;
use super::usage::{TokenCounts, tokens};
use crate::cli::SandboxMode;
use crate::json::first_line;

pub const REFRESH_PERIOD: Duration = Duration::from_millis(100);
const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const MIN_COLUMN: usize = 10;
const HEIGHT_MARGIN: usize = 6;
const FALLBACK_SIZE: (usize, usize) = (80, 24);
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const CLEAR_TO_END: &str = "\x1b[J";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";

pub fn terminal_size() -> (usize, usize) {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    if result == 0 && size.ws_col > 0 && size.ws_row > 0 {
        (size.ws_col as usize, size.ws_row as usize)
    } else {
        FALLBACK_SIZE
    }
}

struct Panel {
    started: Instant,
    main_model: Option<String>,
    main_context: Option<u64>,
    subagents: Vec<Subagent>,
    seen_subagent: bool,
    open_tools: Vec<OpenTool>,
    totals: TokenCounts,
    any_usage: bool,
}

struct OpenTool {
    id: String,
    agent: Option<String>,
    summary: String,
}

struct Subagent {
    id: String,
    description: String,
    started: Instant,
    context: Option<u64>,
}

impl Panel {
    fn new(started: Instant) -> Panel {
        Panel {
            started,
            main_model: None,
            main_context: None,
            subagents: Vec::new(),
            seen_subagent: false,
            open_tools: Vec::new(),
            totals: TokenCounts::default(),
            any_usage: false,
        }
    }

    fn observe(&mut self, event: &Event, now: Instant) {
        match &event.body {
            Body::Start(start) => self.main_model = start.model.clone(),
            Body::ToolStart(start) => self.open_tools.push(OpenTool {
                id: start.id.clone(),
                agent: start.parent.clone(),
                summary: start.summary.clone(),
            }),
            Body::Tool(tool) => {
                if let Some(index) = self.open_tools.iter().position(|open| open.id == tool.id) {
                    self.open_tools.remove(index);
                }
            }
            Body::Usage(usage) => {
                self.totals.add(&usage.counts);
                self.any_usage = true;
                match &usage.parent {
                    None => {
                        if usage.model.is_some() {
                            self.main_model = usage.model.clone();
                        }
                        self.main_context = usage.context_tokens;
                    }
                    Some(id) => {
                        if let Some(subagent) = self.subagents.iter_mut().find(|s| s.id == *id) {
                            subagent.context = usage.context_tokens;
                        }
                    }
                }
            }
            Body::SubagentStart(start) => {
                self.seen_subagent = true;
                self.subagents.push(Subagent {
                    id: start.id.clone(),
                    description: first_line(&start.description).to_string(),
                    started: now,
                    context: None,
                });
            }
            Body::SubagentEnd(end) => self.subagents.retain(|s| s.id != end.id),
            _ => {}
        }
    }

    fn open_tool(&self, agent: Option<&str>) -> Option<&str> {
        self.open_tools
            .iter()
            .rev()
            .find(|open| open.agent.as_deref() == agent)
            .map(|open| open.summary.as_str())
    }

    fn lines(
        &self,
        text: &TextFormatter,
        width: usize,
        height: usize,
        now: Instant,
        color: bool,
    ) -> Vec<String> {
        let max = width.saturating_sub(1);
        let frame =
            FRAMES[(now.duration_since(self.started).as_millis() / 100 % 10) as usize].to_string();
        let rule = dim(&"─".repeat(max), color);
        let mut lines = vec![rule.clone()];
        if self.seen_subagent {
            lines.push(format!("subagent  {} running", self.subagents.len()));
            let shown = height.saturating_sub(HEIGHT_MARGIN).max(1);
            let rows = self
                .subagents
                .iter()
                .take(shown)
                .map(|subagent| {
                    vec![
                        text.subagent_label(&subagent.id)
                            .unwrap_or_default()
                            .to_string(),
                        text.subagent_model(&subagent.id)
                            .unwrap_or_default()
                            .to_string(),
                        subagent.description.clone(),
                        self.open_tool(Some(&subagent.id))
                            .unwrap_or_default()
                            .to_string(),
                        clock(now.duration_since(subagent.started)),
                        context(subagent.context),
                    ]
                })
                .collect();
            lines.extend(table(rows, &[2, 3], &format!("  {frame} "), max));
            if self.subagents.len() > shown {
                lines.push(format!("  … and {} more", self.subagents.len() - shown));
            }
            lines.push(rule);
        }
        let status = vec![
            "main".to_string(),
            self.main_model.clone().unwrap_or_default(),
            self.open_tool(None).unwrap_or_default().to_string(),
            clock(now.duration_since(self.started)),
            self.usage(),
            context(self.main_context),
        ];
        lines.extend(table(vec![status], &[2], &format!("{frame} "), max));
        lines
    }

    fn usage(&self) -> String {
        if !self.any_usage {
            return String::new();
        }
        self.totals.summary(false).join("  ")
    }
}

fn context(tokens_count: Option<u64>) -> String {
    tokens_count
        .map(|count| format!("ctx {}", tokens(count)))
        .unwrap_or_default()
}

fn table(rows: Vec<Vec<String>>, shrink: &[usize], prefix: &str, max: usize) -> Vec<String> {
    let columns = rows.first().map_or(0, Vec::len);
    let kept: Vec<usize> = (0..columns)
        .filter(|&c| rows.iter().any(|row| !row[c].is_empty()))
        .collect();
    let mut widths: Vec<usize> = kept
        .iter()
        .map(|&c| rows.iter().map(|row| row[c].width()).max().unwrap_or(0))
        .collect();
    let gaps = 2 * kept.len().saturating_sub(1);
    for column in shrink {
        let Some(index) = kept.iter().position(|c| c == column) else {
            continue;
        };
        let excess = (prefix.width() + widths.iter().sum::<usize>() + gaps).saturating_sub(max);
        if excess == 0 {
            break;
        }
        let current = widths[index];
        widths[index] = current.saturating_sub(excess).max(MIN_COLUMN.min(current));
    }
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = kept
                .iter()
                .zip(&widths)
                .enumerate()
                .map(|(index, (&c, &width))| {
                    let cell = truncate(&row[c], width);
                    if index + 1 == kept.len() {
                        cell
                    } else {
                        format!("{cell}{}", " ".repeat(width - cell.width()))
                    }
                })
                .collect();
            truncate(format!("{prefix}{}", cells.join("  ")).trim_end(), max)
        })
        .collect()
}

fn truncate(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut used = 0;
    let mut result = String::new();
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max.saturating_sub(1) {
            break;
        }
        used += w;
        result.push(c);
    }
    if max > 0 {
        result.push('…');
    }
    result
}

fn clock(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 3600 {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    } else {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    }
}

fn dim(text: &str, color: bool) -> String {
    if color {
        format!("{DIM}{text}{RESET}")
    } else {
        text.to_string()
    }
}

fn dim_label(line: &str, color: bool) -> String {
    match line.find(']') {
        Some(end) if color && line.starts_with('[') => {
            format!("{DIM}{}{RESET}{}", &line[..=end], &line[end + 1..])
        }
        _ => line.to_string(),
    }
}

pub struct Rich {
    text: TextFormatter,
    panel: Panel,
    size: Box<dyn Fn() -> (usize, usize)>,
    color: bool,
    drawn: usize,
    cursor_hidden: bool,
    finished: bool,
    stderr_pending: Vec<u8>,
}

impl Rich {
    pub fn new(
        sandbox: SandboxMode,
        sandbox_reason: &str,
        color: bool,
        size: Box<dyn Fn() -> (usize, usize)>,
    ) -> Rich {
        Rich {
            text: TextFormatter::new(sandbox, sandbox_reason),
            panel: Panel::new(Instant::now()),
            size,
            color,
            drawn: 0,
            cursor_hidden: false,
            finished: false,
            stderr_pending: Vec::new(),
        }
    }

    pub fn add_note(&mut self, note: String) {
        self.text.add_note(note);
    }

    pub fn event(&mut self, out: &mut dyn Write, event: &Event) {
        let now = Instant::now();
        let lines: Vec<String> = self
            .text
            .lines(event)
            .iter()
            .map(|line| dim_label(line, self.color))
            .collect();
        self.panel.observe(event, now);
        if !matches!(event.body, Body::End(_)) {
            self.paint(out, &lines, now);
            return;
        }
        let mut bytes = self.erase();
        if !self.stderr_pending.is_empty() {
            bytes.append(&mut self.stderr_pending);
            bytes.push(b'\n');
        }
        for line in lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
        }
        if self.cursor_hidden {
            bytes.extend_from_slice(SHOW_CURSOR.as_bytes());
        }
        self.finished = true;
        let _ = out.write_all(&bytes);
        let _ = out.flush();
    }

    pub fn refresh(&mut self, out: &mut dyn Write) {
        self.paint(out, &[], Instant::now());
    }

    pub fn stderr(&mut self, out: &mut dyn Write, bytes: &[u8]) {
        self.stderr_pending.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(end) = self.stderr_pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.stderr_pending.drain(..=end).collect();
            lines.push(String::from_utf8_lossy(&line[..end]).into_owned());
        }
        if !lines.is_empty() {
            self.paint(out, &lines, Instant::now());
        }
    }

    fn paint(&mut self, out: &mut dyn Write, lines: &[String], now: Instant) {
        if self.finished {
            return;
        }
        let mut bytes = self.erase();
        if !self.cursor_hidden {
            bytes.extend_from_slice(HIDE_CURSOR.as_bytes());
            self.cursor_hidden = true;
        }
        for line in lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
        }
        let (width, height) = (self.size)();
        let panel = self.panel.lines(&self.text, width, height, now, self.color);
        bytes.extend_from_slice(panel.join("\n").as_bytes());
        self.drawn = panel.len();
        let _ = out.write_all(&bytes);
        let _ = out.flush();
    }

    fn erase(&mut self) -> Vec<u8> {
        let mut bytes = Vec::new();
        if self.drawn > 0 {
            bytes.push(b'\r');
            if self.drawn > 1 {
                bytes.extend_from_slice(format!("\x1b[{}A", self.drawn - 1).as_bytes());
            }
            bytes.extend_from_slice(CLEAR_TO_END.as_bytes());
        }
        self.drawn = 0;
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{NetworkMode, Runtime};
    use crate::output::event::{
        NetworkInfo, SandboxKind, Start, SubagentEnd, SubagentStart, SubagentStatus, Tool,
        ToolStart, UsageReport,
    };

    fn event(body: Body) -> Event {
        Event {
            time: "2026-10-04T13:00:00.000Z".to_string(),
            body,
        }
    }

    fn start(model: Option<&str>) -> Event {
        event(Body::Start(Start {
            runtime: Runtime::ClaudeCode,
            sandbox: SandboxKind::None,
            network: NetworkInfo {
                mode: NetworkMode::None,
                allow: vec![],
                enforced: false,
            },
            deny_read: vec![],
            model: model.map(str::to_string),
            cwd: "/work".to_string(),
            argv: vec![],
            env: vec![],
        }))
    }

    fn subagent_start(id: &str, number: u64, kind: &str, model: Option<&str>, desc: &str) -> Event {
        event(Body::SubagentStart(SubagentStart {
            id: id.to_string(),
            parent: None,
            number,
            kind: kind.to_string(),
            model: model.map(str::to_string),
            description: desc.to_string(),
        }))
    }

    fn usage(parent: Option<&str>, model: Option<&str>, counts: TokenCounts) -> Event {
        event(Body::Usage(UsageReport {
            parent: parent.map(str::to_string),
            model: model.map(str::to_string),
            counts,
            context_tokens: counts.context_tokens(),
        }))
    }

    fn counts(input: u64, output: u64, read: u64, write: u64) -> TokenCounts {
        TokenCounts {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(read),
            cache_write_tokens: Some(write),
            cost_usd: None,
        }
    }

    struct Board {
        panel: Panel,
        text: TextFormatter,
    }

    impl Board {
        fn new(started: Instant) -> Board {
            Board {
                panel: Panel::new(started),
                text: TextFormatter::new(SandboxMode::On, ""),
            }
        }

        fn observe(&mut self, event: &Event, now: Instant) {
            self.text.lines(event);
            self.panel.observe(event, now);
        }

        fn lines(&self, width: usize, height: usize, now: Instant, color: bool) -> Vec<String> {
            self.panel.lines(&self.text, width, height, now, color)
        }

        fn start_tool(&mut self, id: &str, parent: Option<&str>, summary: &str) {
            let event = event(Body::ToolStart(ToolStart {
                id: id.to_string(),
                parent: parent.map(str::to_string),
                name: "Bash".to_string(),
                summary: summary.to_string(),
            }));
            self.observe(&event, self.panel.started);
        }

        fn end_tool(&mut self, id: &str) {
            let event = event(Body::Tool(Tool {
                id: id.to_string(),
                parent: None,
                name: "Bash".to_string(),
                summary: String::new(),
                denied: false,
                error: Some(false),
            }));
            self.observe(&event, self.panel.started);
        }

        fn end_subagent(&mut self, id: &str, now: Instant) {
            self.observe(
                &event(Body::SubagentEnd(SubagentEnd {
                    id: id.to_string(),
                    status: SubagentStatus::Finished,
                    duration_ms: 0,
                    usage: Default::default(),
                })),
                now,
            );
        }
    }

    fn at(started: Instant, seconds: u64) -> Instant {
        started + Duration::from_secs(seconds)
    }

    #[test]
    fn only_rule_and_status_before_any_subagent() {
        let started = Instant::now();
        let mut panel = Board::new(started);
        panel.observe(&start(Some("sonnet")), started);
        let lines = panel.lines(40, 24, at(started, 24), false);
        assert_eq!(
            lines,
            vec!["─".repeat(39), "⠋ main  sonnet  0:24".to_string()]
        );
    }

    #[test]
    fn open_tool_is_the_last_open_call_of_each_agent() {
        let started = Instant::now();
        let mut board = Board::new(started);
        assert_eq!(board.panel.open_tool(None), None);
        board.observe(&subagent_start("s1", 1, "Explore", None, "look"), started);
        board.start_tool("t1", None, "Bash: first");
        board.start_tool("t2", Some("s1"), "Read: a.rs");
        board.start_tool("t3", None, "Bash: second");
        assert_eq!(board.panel.open_tool(None), Some("Bash: second"));
        assert_eq!(board.panel.open_tool(Some("s1")), Some("Read: a.rs"));

        board.end_tool("t3");
        assert_eq!(board.panel.open_tool(None), Some("Bash: first"));
        board.end_tool("t2");
        assert_eq!(board.panel.open_tool(Some("s1")), None);
        board.end_tool("t1");
        assert_eq!(board.panel.open_tool(None), None);
    }

    #[test]
    fn structure_after_subagents_and_zero_running() {
        let started = Instant::now();
        let mut panel = Board::new(started);
        panel.observe(&start(None), started);
        panel.observe(
            &subagent_start("s1", 1, "Explore", Some("haiku"), "look\nsecond"),
            at(started, 13),
        );
        panel.start_tool("t1", None, "Bash: cargo check");
        panel.start_tool("t2", Some("s1"), "Read: src/cli.rs");
        let lines = panel.lines(80, 24, at(started, 24), false);
        assert_eq!(
            lines,
            vec![
                "─".repeat(79),
                "subagent  1 running".to_string(),
                "  ⠋ explore#1  haiku  look  Read: src/cli.rs  0:11".to_string(),
                "─".repeat(79),
                "⠋ main  Bash: cargo check  0:24".to_string(),
            ]
        );
        panel.end_tool("t2");
        panel.end_subagent("s1", at(started, 30));
        panel.end_tool("t1");
        let lines = panel.lines(80, 24, at(started, 30), false);
        assert_eq!(
            lines,
            vec![
                "─".repeat(79),
                "subagent  0 running".to_string(),
                "─".repeat(79),
                "⠋ main  0:30".to_string(),
            ]
        );
    }

    #[test]
    fn columns_align_and_chinese_counts_two_cells() {
        let started = Instant::now();
        let mut panel = Board::new(started);
        panel.observe(
            &subagent_start("s1", 1, "Explore", None, "查找调用"),
            started,
        );
        panel.observe(
            &subagent_start(
                "s2",
                2,
                "general-purpose",
                Some("claude-sonnet-5-5"),
                "写测试",
            ),
            started,
        );
        panel.observe(
            &usage(
                Some("s1"),
                Some("claude-haiku-4-5"),
                counts(14000, 10, 100, 0),
            ),
            started,
        );
        let lines = panel.lines(120, 24, at(started, 65), false);
        assert_eq!(
            lines[2],
            "  ⠋ explore#1          claude-haiku-4-5   查找调用  1:05  ctx 14.1k"
        );
        assert_eq!(
            lines[3],
            "  ⠋ general-purpose#2  claude-sonnet-5-5  写测试    1:05"
        );
        assert_eq!("查找调用".width(), 8);
        panel.start_tool("t1", Some("s1"), "Read: src/cli.rs");
        panel.start_tool("t2", Some("s2"), "Edit: tests/parse.rs");
        let lines = panel.lines(120, 24, at(started, 65), false);
        assert_eq!(
            lines[2],
            "  ⠋ explore#1          claude-haiku-4-5   查找调用  Read: src/cli.rs      1:05  ctx 14.1k"
        );
        assert_eq!(
            lines[3],
            "  ⠋ general-purpose#2  claude-sonnet-5-5  写测试    Edit: tests/parse.rs  1:05"
        );
    }

    #[test]
    fn narrow_terminal_truncates_description_then_tool_then_line() {
        let started = Instant::now();
        let mut panel = Board::new(started);
        panel.observe(
            &subagent_start("s1", 1, "Explore", None, "这是一个很长的中文描述需要被截断"),
            started,
        );
        let full = panel.lines(200, 24, started, false)[2].clone();
        assert_eq!(
            full,
            "  ⠋ explore#1  这是一个很长的中文描述需要被截断  0:00"
        );

        panel.start_tool("t1", Some("s1"), "Read: src/some/very/long/path/to/file.rs");
        let lines = panel.lines(61, 24, started, false);
        assert_eq!(
            lines[2],
            "  ⠋ explore#1  这是一个…   Read: src/some/very/long/p…  0:00"
        );
        assert_eq!(lines[2].width(), 60);

        let lines = panel.lines(40, 24, started, false);
        assert_eq!(lines[2], "  ⠋ explore#1  这是一个…   Read: src… …");
        assert_eq!(lines[2].width(), 39);

        panel.observe(&subagent_start("s2", 2, "Explore", None, "短描述"), started);
        panel.start_tool("t2", Some("s2"), "Grep: x");
        let lines = panel.lines(61, 24, started, false);
        assert_eq!(
            lines[2],
            "  ⠋ explore#1  这是一个…   Read: src/some/very/long/p…  0:00"
        );
        assert_eq!(
            lines[3],
            "  ⠋ explore#2  短描述      Grep: x                      0:00"
        );
        panel.end_tool("t2");
        panel.end_subagent("s2", started);

        let lines = panel.lines(20, 24, started, false);
        assert_eq!(lines[2], "  ⠋ explore#1  这…");
        assert!(lines[2].width() <= 19);
    }

    #[test]
    fn status_bar_truncates_tool_then_line() {
        let started = Instant::now();
        let mut panel = Board::new(started);
        panel.observe(&start(Some("claude-sonnet-5-5")), started);
        panel.start_tool("t1", None, "Bash: cargo test --workspace --all-features");
        let lines = panel.lines(50, 24, started, false);
        assert_eq!(
            lines[1],
            "⠋ main  claude-sonnet-5-5  Bash: cargo tes…  0:00"
        );
        let lines = panel.lines(30, 24, started, false);
        assert_eq!(lines[1], "⠋ main  claude-sonnet-5-5  B…");
    }

    #[test]
    fn short_terminal_shows_first_rows_and_more_count() {
        let started = Instant::now();
        let mut panel = Board::new(started);
        for n in 1..=5 {
            panel.observe(
                &subagent_start(&format!("s{n}"), n, "Explore", None, "x"),
                started,
            );
        }
        let lines = panel.lines(80, 9, started, false);
        assert_eq!(lines.len(), 8);
        assert_eq!(lines[2], "  ⠋ explore#1  x  0:00");
        assert_eq!(lines[4], "  ⠋ explore#3  x  0:00");
        assert_eq!(lines[5], "  … and 2 more");
        let lines = panel.lines(80, 5, started, false);
        assert_eq!(lines[2], "  ⠋ explore#1  x  0:00");
        assert_eq!(lines[3], "  … and 4 more");
        assert_eq!(lines.len(), 6);
    }

    #[test]
    fn token_and_clock_formats() {
        assert_eq!(tokens(99), "99");
        assert_eq!(tokens(100), "0.1k");
        assert_eq!(tokens(912), "0.9k");
        assert_eq!(tokens(18234), "18.2k");
        assert_eq!(tokens(1_234_567), "1.2M");
        assert_eq!(clock(Duration::from_secs(9)), "0:09");
        assert_eq!(clock(Duration::from_secs(754)), "12:34");
        assert_eq!(clock(Duration::from_secs(3599)), "59:59");
        assert_eq!(clock(Duration::from_secs(3600)), "1:00:00");
    }

    #[test]
    fn status_bar_omits_missing_items() {
        let started = Instant::now();
        let mut panel = Board::new(started);
        panel.observe(&start(None), started);
        assert_eq!(
            panel.lines(80, 24, at(started, 24), false)[1],
            "⠋ main  0:24"
        );
        panel.observe(
            &usage(
                None,
                Some("claude-sonnet-5-5"),
                TokenCounts {
                    input_tokens: Some(18234),
                    output_tokens: None,
                    cache_read_tokens: Some(42000),
                    cache_write_tokens: None,
                    cost_usd: None,
                },
            ),
            started,
        );
        assert_eq!(
            panel.lines(80, 24, at(started, 24), false)[1],
            "⠋ main  claude-sonnet-5-5  0:24  in 60.2k  cached 70%"
        );
        panel.observe(
            &usage(Some("ghost"), None, counts(1000, 912, 0, 0)),
            started,
        );
        panel.observe(&usage(None, None, counts(18234, 0, 42000, 1000)), started);
        panel.start_tool("t1", None, "Bash: cargo check");
        assert_eq!(
            panel.lines(120, 24, at(started, 24), false)[1],
            "⠋ main  claude-sonnet-5-5  Bash: cargo check  0:24  in 122.5k  cached 69%  ctx 61.2k"
        );
    }

    #[test]
    fn animation_frame_follows_time() {
        let started = Instant::now();
        let panel = Board::new(started);
        let frame = |ms| {
            panel.lines(80, 24, started + Duration::from_millis(ms), false)[1]
                .chars()
                .next()
                .unwrap()
        };
        assert_eq!(frame(0), '⠋');
        assert_eq!(frame(100), '⠙');
        assert_eq!(frame(950), '⠏');
        assert_eq!(frame(1000), '⠋');
    }

    #[test]
    fn color_dims_rule_and_labels_unless_disabled() {
        let started = Instant::now();
        let panel = Board::new(started);
        let colored = panel.lines(10, 24, started, true);
        assert_eq!(colored[0], format!("\x1b[2m{}\x1b[0m", "─".repeat(9)));
        let plain = panel.lines(10, 24, started, false);
        assert!(!plain.concat().contains('\x1b'));
        assert_eq!(
            dim_label("[explore#1] tool Grep: x", true),
            "\x1b[2m[explore#1]\x1b[0m tool Grep: x"
        );
        assert_eq!(
            dim_label("[explore#1] tool Grep: x", false),
            "[explore#1] tool Grep: x"
        );
    }

    #[test]
    fn truncate_keeps_whole_characters() {
        assert_eq!(truncate("abcdef", 6), "abcdef");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("中文字", 4), "中…");
        assert_eq!(truncate("中文字", 5), "中文…");
        assert_eq!(truncate("中文字", 0), "");
    }
}
