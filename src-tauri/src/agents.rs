//! Claude Code / Codex turn tracking.
//!
//! Both agents append every turn to a JSONL transcript on disk
//! (`~/.claude/projects/<project>/<session>.jsonl` and
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`). Tailing those files needs
//! no hooks or config in either tool, so it works for the CLIs and the desktop
//! apps alike. Each turn start, finish, question or interrupt is emitted to the
//! notch as an `agent-event`.

use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

const POLL_MS: u64 = 600;
const RESCAN_EVERY: u32 = 8; // polls between directory rescans (~5s)
const ACTIVE_WINDOW: Duration = Duration::from_secs(6 * 3600);
const PRIME_BYTES: u64 = 768 * 1024;
const MESSAGE_CHARS: usize = 320;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

#[derive(Clone, Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentEvent {
    pub agent: &'static str,
    /// `working`, `done`, `question` or `interrupted`.
    pub kind: &'static str,
    pub session: String,
    pub title: String,
    pub project: String,
    /// The final reply (done), the question asked (question), or empty.
    pub message: String,
    /// The reply ends by asking something.
    pub asks: bool,
    pub started_at: i64,
    pub duration_ms: Option<i64>,
    pub output_tokens: u64,
    pub context_tokens: u64,
    pub at: i64,
}

/// Latest event per agent, for a webview that (re)loads after it happened.
static LATEST: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());

#[tauri::command]
pub fn get_agent_states() -> Vec<AgentEvent> {
    LATEST.lock().map(|l| l.clone()).unwrap_or_default()
}

/// Per-transcript parse state. Pure: fed one line at a time, returns the
/// event that line produced, if any.
#[derive(Default)]
pub struct Session {
    agent: Option<Agent>,
    id: String,
    title: String,
    project: String,
    started_at: i64,
    working: bool,
    // Claude streams one message as several entries sharing an id.
    out_by_msg: HashMap<String, u64>,
    context: u64,
    last_text: String,
    // Codex reports cumulative totals; per-turn output is the difference.
    codex_out_at_start: Option<u64>,
    codex_out_total: u64,
    asked_tool_ids: HashSet<String>,
}

impl Session {
    pub fn new(agent: Agent) -> Self {
        Session {
            agent: Some(agent),
            ..Default::default()
        }
    }

    fn event(&self, kind: &'static str, message: String, at: i64, duration_ms: Option<i64>) -> AgentEvent {
        let output_tokens = match self.agent {
            Some(Agent::Codex) => self
                .codex_out_total
                .saturating_sub(self.codex_out_at_start.unwrap_or(self.codex_out_total)),
            _ => self.out_by_msg.values().sum(),
        };
        AgentEvent {
            agent: self.agent.map(Agent::name).unwrap_or("claude"),
            kind,
            session: self.id.clone(),
            title: self.title.clone(),
            project: self.project.clone(),
            asks: kind == "done" && ends_with_question(&message),
            message: clip(&message, MESSAGE_CHARS),
            started_at: self.started_at,
            duration_ms,
            output_tokens,
            context_tokens: self.context,
            at,
        }
    }

    fn begin_turn(&mut self, at: i64) {
        self.started_at = at;
        self.working = true;
        self.out_by_msg.clear();
        self.last_text.clear();
        self.codex_out_at_start = Some(self.codex_out_total);
    }

    pub fn feed(&mut self, line: &str) -> Option<AgentEvent> {
        let v: Value = serde_json::from_str(line).ok()?;
        match self.agent? {
            Agent::Claude => self.feed_claude(&v),
            Agent::Codex => self.feed_codex(&v),
        }
    }

    fn feed_claude(&mut self, v: &Value) -> Option<AgentEvent> {
        let kind = v.get("type")?.as_str()?;
        if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
            self.project = folder_name(cwd);
        }
        if let Some(id) = v.get("sessionId").and_then(Value::as_str) {
            self.id = id.to_string();
        }
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        let at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso_ms)
            .unwrap_or_else(now_ms);
        match kind {
            "custom-title" => {
                if let Some(t) = v.get("customTitle").and_then(Value::as_str) {
                    self.title = t.to_string();
                }
                None
            }
            "user" => {
                if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    return None;
                }
                let content = v.pointer("/message/content")?;
                let text = match content {
                    Value::String(s) => s.clone(),
                    Value::Array(items) => {
                        if items
                            .iter()
                            .any(|i| i.get("type").and_then(Value::as_str) == Some("tool_result"))
                        {
                            return None;
                        }
                        items
                            .iter()
                            .filter_map(|i| i.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                    _ => return None,
                };
                let trimmed = text.trim_start();
                // Slash-command bookkeeping (/model, /clear output) is not a prompt.
                if trimmed.starts_with("<command-") || trimmed.starts_with("<local-command") {
                    return None;
                }
                if trimmed.starts_with("[Request interrupted") {
                    if !self.working {
                        return None;
                    }
                    self.working = false;
                    return Some(self.event("interrupted", String::new(), at, Some(at - self.started_at)));
                }
                self.begin_turn(at);
                Some(self.event("working", String::new(), at, None))
            }
            "assistant" => {
                let msg = v.get("message")?;
                let id = msg.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                if let Some(u) = msg.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    self.out_by_msg.insert(id, n("output_tokens"));
                    let ctx = n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
                    if ctx > 0 {
                        self.context = ctx;
                    }
                }
                let mut question = None;
                for item in msg.get("content").and_then(Value::as_array).into_iter().flatten() {
                    match item.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = item.get("text").and_then(Value::as_str) {
                                if !t.trim().is_empty() {
                                    self.last_text = t.to_string();
                                }
                            }
                        }
                        Some("tool_use") if item.get("name").and_then(Value::as_str) == Some("AskUserQuestion") => {
                            let tool_id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                            if self.asked_tool_ids.insert(tool_id) {
                                question = item
                                    .pointer("/input/questions/0/question")
                                    .and_then(Value::as_str)
                                    .map(str::to_string);
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(q) = question {
                    return Some(self.event("question", q, at, Some(at - self.started_at)));
                }
                match msg.get("stop_reason").and_then(Value::as_str) {
                    Some("end_turn") | Some("stop_sequence") | Some("max_tokens") | Some("refusal") if self.working => {
                        self.working = false;
                        let text = self.last_text.clone();
                        Some(self.event("done", text, at, Some(at - self.started_at)))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn feed_codex(&mut self, v: &Value) -> Option<AgentEvent> {
        let at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso_ms)
            .unwrap_or_else(now_ms);
        let p = v.get("payload")?;
        match v.get("type")?.as_str()? {
            "session_meta" => {
                if let Some(cwd) = p.get("cwd").and_then(Value::as_str) {
                    self.project = folder_name(cwd);
                }
                if let Some(id) = p.get("id").and_then(Value::as_str) {
                    self.id = id.to_string();
                }
                None
            }
            "turn_context" => {
                if let Some(cwd) = p.get("cwd").and_then(Value::as_str) {
                    self.project = folder_name(cwd);
                }
                None
            }
            "event_msg" => match p.get("type")?.as_str()? {
                "task_started" => {
                    let start = p.get("started_at").and_then(Value::as_i64).map(|s| s * 1000).unwrap_or(at);
                    self.begin_turn(start);
                    Some(self.event("working", String::new(), at, None))
                }
                "token_count" => {
                    let info = p.get("info")?;
                    if let Some(o) = info.pointer("/total_token_usage/output_tokens").and_then(Value::as_u64) {
                        self.codex_out_total = o;
                        if self.codex_out_at_start.is_none() {
                            self.codex_out_at_start = Some(o);
                        }
                    }
                    if let Some(i) = info.pointer("/last_token_usage/input_tokens").and_then(Value::as_u64) {
                        self.context = i;
                    }
                    None
                }
                "task_complete" => {
                    self.working = false;
                    let msg = p.get("last_agent_message").and_then(Value::as_str).unwrap_or("").to_string();
                    let dur = p.get("duration_ms").and_then(Value::as_i64).or(Some(at - self.started_at));
                    Some(self.event("done", msg, at, dur))
                }
                "turn_aborted" => {
                    self.working = false;
                    let dur = p.get("duration_ms").and_then(Value::as_i64);
                    Some(self.event("interrupted", String::new(), at, dur))
                }
                _ => None,
            },
            _ => None,
        }
    }
}

fn folder_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// True when the last non-empty line of a reply is a question.
fn ends_with_question(text: &str) -> bool {
    text.lines()
        .rev()
        .map(|l| l.trim().trim_end_matches(['*', '_', '`', ')', '"']))
        .find(|l| !l.is_empty())
        .is_some_and(|l| l.ends_with('?'))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// `2026-11-07T05:43:29.548Z` → unix ms. Only the UTC `Z` form both tools write.
fn parse_iso_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || !s.ends_with('Z') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let frac = s.get(19..s.len() - 1).unwrap_or("");
    let ms = frac
        .strip_prefix('.')
        .map(|f| {
            let f: String = f.chars().take(3).collect();
            format!("{f:0<3}").parse::<i64>().unwrap_or(0)
        })
        .unwrap_or(0);
    // Days from civil (Howard Hinnant's algorithm).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + sec * 1000 + ms)
}

struct Tail {
    session: Session,
    offset: u64,
    partial: String,
}

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

fn recent(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < ACTIVE_WINDOW)
}

fn collect_jsonl(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if depth > 0 {
                collect_jsonl(&p, depth - 1, out);
            }
        } else if p.extension().is_some_and(|x| x == "jsonl") && recent(&p) {
            out.push(p);
        }
    }
}

fn discover() -> Vec<(PathBuf, Agent)> {
    let Some(home) = home() else { return Vec::new() };
    let mut found = Vec::new();
    let mut claude = Vec::new();
    // Depth 1: <project>/<session>.jsonl only; subagent transcripts sit deeper.
    collect_jsonl(&home.join(".claude").join("projects"), 1, &mut claude);
    found.extend(claude.into_iter().map(|p| (p, Agent::Claude)));
    let mut codex = Vec::new();
    collect_jsonl(&home.join(".codex").join("sessions"), 3, &mut codex);
    found.extend(codex.into_iter().map(|p| (p, Agent::Codex)));
    found
}

fn codex_titles() -> HashMap<String, String> {
    let mut titles = HashMap::new();
    let Some(path) = home().map(|h| h.join(".codex").join("session_index.jsonl")) else {
        return titles;
    };
    let Ok(file) = File::open(path) else { return titles };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let (Some(id), Some(name)) = (
                v.get("id").and_then(Value::as_str),
                v.get("thread_name").and_then(Value::as_str),
            ) {
                titles.insert(id.to_string(), name.to_string());
            }
        }
    }
    titles
}

/// Opens a transcript and replays enough of it to know its project, title
/// and current turn, without emitting anything for the past.
fn prime(path: &Path, agent: Agent, from_start: bool) -> Option<Tail> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut session = Session::new(agent);
    if agent == Agent::Codex {
        // The session header (cwd, id) is always the first line.
        let mut first = String::new();
        BufReader::new(&mut file).read_line(&mut first).ok()?;
        session.feed(first.trim_end());
    }
    let start = if from_start { 0 } else { len.saturating_sub(PRIME_BYTES) };
    if !from_start {
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::new();
        file.take(len - start).read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);
        let mut lines = text.lines();
        if start > 0 {
            lines.next(); // a line cut in half by the seek
        }
        for line in lines {
            session.feed(line);
        }
        return Some(Tail { session, offset: len, partial: String::new() });
    }
    Some(Tail { session, offset: 0, partial: String::new() })
}

fn read_new(path: &Path, tail: &mut Tail) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let Ok(mut file) = File::open(path) else { return events };
    let Ok(len) = file.metadata().map(|m| m.len()) else { return events };
    if len < tail.offset {
        // Rewritten or truncated: start over.
        tail.offset = 0;
        tail.partial.clear();
    }
    if len == tail.offset || file.seek(SeekFrom::Start(tail.offset)).is_err() {
        return events;
    }
    let mut buf = Vec::new();
    if file.take(len - tail.offset).read_to_end(&mut buf).is_err() {
        return events;
    }
    tail.offset += buf.len() as u64;
    tail.partial.push_str(&String::from_utf8_lossy(&buf));
    while let Some(nl) = tail.partial.find('\n') {
        let line: String = tail.partial.drain(..=nl).collect();
        if let Some(ev) = tail.session.feed(line.trim_end()) {
            events.push(ev);
        }
    }
    events
}

fn publish(app: &AppHandle, ev: AgentEvent) {
    if let Ok(mut latest) = LATEST.lock() {
        latest.retain(|e| e.agent != ev.agent);
        latest.push(ev.clone());
    }
    if ev.kind != "working" {
        crate::services::follow_cursor_monitor(app);
    }
    let _ = app.emit("agent-event", ev);
}

pub fn setup_agent_watcher(app: AppHandle) {
    std::thread::spawn(move || {
        let mut tails: HashMap<PathBuf, Tail> = HashMap::new();
        let mut titles = codex_titles();
        let mut first_scan = true;
        let mut tick: u32 = 0;
        loop {
            if tick.is_multiple_of(RESCAN_EVERY) {
                let found = discover();
                let mut new_codex = false;
                for (path, agent) in found {
                    if tails.contains_key(&path) {
                        continue;
                    }
                    // Files that appear after startup are new sessions: read
                    // them whole so their first turn is reported.
                    if let Some(t) = prime(&path, agent, !first_scan) {
                        new_codex |= agent == Agent::Codex;
                        tails.insert(path, t);
                    }
                }
                if new_codex || tick.is_multiple_of(RESCAN_EVERY * 6) {
                    titles = codex_titles();
                }
                tails.retain(|p, _| p.exists());
                first_scan = false;
            }
            tick = tick.wrapping_add(1);

            for (path, tail) in tails.iter_mut() {
                for mut ev in read_new(path, tail) {
                    if ev.agent == "codex" && ev.title.is_empty() {
                        if let Some(t) = titles.get(&ev.session) {
                            ev.title = t.clone();
                        }
                    }
                    publish(&app, ev);
                }
            }
            std::thread::sleep(Duration::from_millis(POLL_MS));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso_timestamps() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_iso_ms("2026-11-07T05:43:29.548Z"), Some(1_794_030_209_548));
        assert_eq!(parse_iso_ms("garbage"), None);
    }

    #[test]
    fn claude_turn_reports_duration_tokens_and_reply() {
        let mut s = Session::new(Agent::Claude);
        s.feed(r#"{"type":"custom-title","customTitle":"Fix bloom"}"#);
        let w = s
            .feed(r#"{"type":"user","cwd":"C:\\x\\bloom","sessionId":"s1","timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"user","content":"go"}}"#)
            .unwrap();
        assert_eq!(w.kind, "working");
        // Tool round trip: no event, tokens accumulate, streamed entries dedupe by id.
        assert!(s.feed(r#"{"type":"assistant","timestamp":"2026-01-01T00:00:05.000Z","message":{"id":"m1","stop_reason":"tool_use","usage":{"input_tokens":10,"cache_read_input_tokens":1000,"output_tokens":40},"content":[{"type":"tool_use","name":"Bash"}]}}"#).is_none());
        assert!(s.feed(r#"{"type":"user","timestamp":"2026-01-01T00:00:06.000Z","message":{"content":[{"type":"tool_result"}]}}"#).is_none());
        s.feed(r#"{"type":"assistant","timestamp":"2026-01-01T00:01:00.000Z","message":{"id":"m2","stop_reason":null,"usage":{"input_tokens":5,"cache_read_input_tokens":2000,"output_tokens":10},"content":[{"type":"thinking"}]}}"#);
        let done = s
            .feed(r#"{"type":"assistant","timestamp":"2026-01-01T00:01:30.000Z","message":{"id":"m2","stop_reason":"end_turn","usage":{"input_tokens":5,"cache_read_input_tokens":2000,"output_tokens":60},"content":[{"type":"text","text":"All set. Want me to push?"}]}}"#)
            .unwrap();
        assert_eq!(done.kind, "done");
        assert_eq!(done.duration_ms, Some(90_000));
        assert_eq!(done.output_tokens, 100);
        assert_eq!(done.context_tokens, 2005);
        assert_eq!(done.project, "bloom");
        assert_eq!(done.title, "Fix bloom");
        assert!(done.asks);
        // A second end_turn for the same turn is not reported twice.
        assert!(s.feed(r#"{"type":"assistant","message":{"id":"m3","stop_reason":"end_turn","content":[]}}"#).is_none());
    }

    #[test]
    fn claude_question_and_interrupt() {
        let mut s = Session::new(Agent::Claude);
        s.feed(r#"{"type":"user","timestamp":"2026-01-01T00:00:00Z","message":{"content":"hi"}}"#);
        let q = s
            .feed(r#"{"type":"assistant","timestamp":"2026-01-01T00:00:10Z","message":{"id":"m","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{"questions":[{"question":"Which folder?"}]}}]}}"#)
            .unwrap();
        assert_eq!((q.kind, q.message.as_str()), ("question", "Which folder?"));
        let i = s
            .feed(r#"{"type":"user","timestamp":"2026-01-01T00:00:20Z","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#)
            .unwrap();
        assert_eq!(i.kind, "interrupted");
        // Sidechain (subagent) traffic is ignored.
        assert!(s.feed(r#"{"type":"user","isSidechain":true,"message":{"content":"sub"}}"#).is_none());
    }

    #[test]
    fn codex_turn_uses_reported_duration_and_token_delta() {
        let mut s = Session::new(Agent::Codex);
        s.feed(r#"{"type":"session_meta","payload":{"id":"c1","cwd":"C:\\Users\\a\\proj"}}"#);
        s.feed(r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"output_tokens":500},"last_token_usage":{"input_tokens":9000}}}}"#);
        let w = s
            .feed(r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","started_at":1767225600}}"#)
            .unwrap();
        assert_eq!(w.kind, "working");
        s.feed(r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"output_tokens":820},"last_token_usage":{"input_tokens":31000}}}}"#);
        let d = s
            .feed(r#"{"timestamp":"2026-01-01T00:00:44Z","type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Done.","duration_ms":44401}}"#)
            .unwrap();
        assert_eq!(d.kind, "done");
        assert_eq!(d.duration_ms, Some(44401));
        assert_eq!(d.output_tokens, 320);
        assert_eq!(d.context_tokens, 31000);
        assert_eq!(d.project, "proj");
        assert_eq!(d.session, "c1");
        assert!(!d.asks);
        let a = s
            .feed(r#"{"type":"event_msg","payload":{"type":"turn_aborted","duration_ms":7141}}"#)
            .unwrap();
        assert_eq!((a.kind, a.duration_ms), ("interrupted", Some(7141)));
    }

    #[test]
    fn question_detection_ignores_trailing_markdown() {
        assert!(ends_with_question("Done.\n\n**Want me to commit?**\n"));
        assert!(!ends_with_question("Is it? Yes. Fixed."));
        assert!(!ends_with_question(""));
    }
}
