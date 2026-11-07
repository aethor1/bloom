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
    /// Questions opened or settled by the lines fed so far (Codex).
    pub ask_updates: Vec<AskUpdate>,
    open_ask: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum AskUpdate {
    Open(AgentAsk),
    Close(String),
}

impl Session {
    pub fn new(agent: Agent) -> Self {
        Session {
            agent: Some(agent),
            ..Default::default()
        }
    }

    fn event(
        &self,
        kind: &'static str,
        message: String,
        at: i64,
        duration_ms: Option<i64>,
    ) -> AgentEvent {
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
                    return Some(self.event(
                        "interrupted",
                        String::new(),
                        at,
                        Some(at - self.started_at),
                    ));
                }
                self.begin_turn(at);
                Some(self.event("working", String::new(), at, None))
            }
            "assistant" => {
                let msg = v.get("message")?;
                let id = msg
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(u) = msg.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    self.out_by_msg.insert(id, n("output_tokens"));
                    let ctx = n("input_tokens")
                        + n("cache_read_input_tokens")
                        + n("cache_creation_input_tokens");
                    if ctx > 0 {
                        self.context = ctx;
                    }
                }
                let mut question = None;
                for item in msg
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match item.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = item.get("text").and_then(Value::as_str) {
                                if !t.trim().is_empty() {
                                    self.last_text = t.to_string();
                                }
                            }
                        }
                        Some("tool_use")
                            if item.get("name").and_then(Value::as_str)
                                == Some("AskUserQuestion") =>
                        {
                            let tool_id = item
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
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
                    Some("end_turn")
                    | Some("stop_sequence")
                    | Some("max_tokens")
                    | Some("refusal")
                        if self.working =>
                    {
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

    fn settle_ask(&mut self) {
        if let Some(id) = self.open_ask.take() {
            self.ask_updates.push(AskUpdate::Close(id));
        }
    }

    /// Codex asks with `request_user_input` (blocks the turn) or
    /// `request_user_input_async` (desktop; answered by a later user message).
    /// Neither can be answered from outside Codex, so the notch shows the
    /// options and hands the chosen one over to paste.
    fn codex_question_item(&mut self, p: &Value, at: i64) -> Option<AgentEvent> {
        match p.get("type")?.as_str()? {
            "function_call" => {
                let name = p.get("name")?.as_str()?;
                if name != "request_user_input" && name != "request_user_input_async" {
                    return None;
                }
                let call_id = p.get("call_id")?.as_str()?.to_string();
                let args: Value = serde_json::from_str(p.get("arguments")?.as_str()?).ok()?;
                let s =
                    |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                let questions: Vec<AskQuestion> = as_list(args.get("questions"))
                    .into_iter()
                    .map(|q| AskQuestion {
                        question: if q.get("title").is_some() {
                            s(q, "title")
                        } else {
                            s(q, "question")
                        },
                        header: s(q, "header"),
                        multi_select: false,
                        options: as_list(q.get("options"))
                            .into_iter()
                            .map(|o| match o {
                                Value::String(label) => AskOption {
                                    label: label.clone(),
                                    description: String::new(),
                                },
                                _ => AskOption {
                                    label: s(o, "label"),
                                    description: s(o, "description"),
                                },
                            })
                            .filter(|o| !o.label.is_empty())
                            .collect(),
                    })
                    .filter(|q| !q.question.is_empty())
                    .collect();
                if questions.is_empty() {
                    return None;
                }
                self.settle_ask();
                let id = format!("codex-{call_id}");
                let first = questions[0].question.clone();
                self.ask_updates.push(AskUpdate::Open(AgentAsk {
                    id: id.clone(),
                    agent: "codex".into(),
                    project: self.project.clone(),
                    questions,
                    answerable: false,
                }));
                self.open_ask = Some(id);
                Some(self.event("question", first, at, Some(at - self.started_at)))
            }
            // The blocking form's answer.
            "function_call_output" => {
                let call_id = p.get("call_id")?.as_str()?;
                if self.open_ask.as_deref() == Some(format!("codex-{call_id}").as_str())
                    && p.get("output")
                        .and_then(Value::as_str)
                        .is_some_and(|o| o.contains("\"answers\""))
                {
                    self.settle_ask();
                }
                None
            }
            // The async form's answer: a user message naming the call.
            "message" if p.get("role").and_then(Value::as_str) == Some("user") => {
                let open = self.open_ask.clone()?;
                let call_id = open.strip_prefix("codex-")?;
                let text = p.to_string();
                if text.contains("send_user_message_question_reply") && text.contains(call_id) {
                    self.settle_ask();
                }
                None
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
            "response_item" => self.codex_question_item(p, at),
            "event_msg" => match p.get("type")?.as_str()? {
                "task_started" => {
                    self.settle_ask();
                    let start = p
                        .get("started_at")
                        .and_then(Value::as_i64)
                        .map(|s| s * 1000)
                        .unwrap_or(at);
                    self.begin_turn(start);
                    Some(self.event("working", String::new(), at, None))
                }
                "token_count" => {
                    let info = p.get("info")?;
                    if let Some(o) = info
                        .pointer("/total_token_usage/output_tokens")
                        .and_then(Value::as_u64)
                    {
                        self.codex_out_total = o;
                        if self.codex_out_at_start.is_none() {
                            self.codex_out_at_start = Some(o);
                        }
                    }
                    if let Some(i) = info
                        .pointer("/last_token_usage/input_tokens")
                        .and_then(Value::as_u64)
                    {
                        self.context = i;
                    }
                    None
                }
                "task_complete" => {
                    self.working = false;
                    let msg = p
                        .get("last_agent_message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let dur = p
                        .get("duration_ms")
                        .and_then(Value::as_i64)
                        .or(Some(at - self.started_at));
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
    let Some(home) = home() else {
        return Vec::new();
    };
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
    let Ok(file) = File::open(path) else {
        return titles;
    };
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
    let start = if from_start {
        0
    } else {
        len.saturating_sub(PRIME_BYTES)
    };
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
        // Keep a question still open in a recently active transcript.
        let still_open: Vec<AgentAsk> = session
            .ask_updates
            .drain(..)
            .filter_map(|u| match u {
                AskUpdate::Open(a) => Some(a),
                AskUpdate::Close(_) => None,
            })
            .filter(|a| session.open_ask.as_deref() == Some(a.id.as_str()))
            .collect();
        if recent_within(path, Duration::from_secs(20 * 60)) {
            if let (Some(ask), Ok(mut asks)) = (still_open.into_iter().last(), ASKS.lock()) {
                asks.push(ask);
            }
        }
        return Some(Tail {
            session,
            offset: len,
            partial: String::new(),
        });
    }
    Some(Tail {
        session,
        offset: 0,
        partial: String::new(),
    })
}

fn read_new(path: &Path, tail: &mut Tail) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let Ok(mut file) = File::open(path) else {
        return events;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return events;
    };
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

// ---------- Questions answered from the notch ----------
//
// An agent-side hook (scripts/bloom-ask-hook.ps1 for Claude Code) drops
// `asks/<id>.json` with the question and its options and waits for
// `asks/<id>.answer.json`. Removing the request file withdraws the question.

const ASK_MAX_AGE: Duration = Duration::from_secs(330);

#[derive(Clone, Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AskOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AskQuestion {
    pub question: String,
    pub header: String,
    pub multi_select: bool,
    pub options: Vec<AskOption>,
}

#[derive(Clone, Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentAsk {
    pub id: String,
    pub agent: String,
    pub project: String,
    pub questions: Vec<AskQuestion>,
    /// False when the notch can only offer the answer to paste (Codex).
    pub answerable: bool,
}

static ASKS: Mutex<Vec<AgentAsk>> = Mutex::new(Vec::new());

fn asks_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("com.sehaz.bloom").join("asks"))
}

/// A one-element array may arrive as a bare object (PowerShell's JSON).
fn as_list(v: Option<&Value>) -> Vec<&Value> {
    match v {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(other) => vec![other],
    }
}

pub fn parse_ask(text: &str) -> Option<AgentAsk> {
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let questions: Vec<AskQuestion> = as_list(v.get("questions"))
        .into_iter()
        .map(|q| AskQuestion {
            question: s(q, "question"),
            header: s(q, "header"),
            multi_select: q
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            options: as_list(q.get("options"))
                .into_iter()
                .map(|o| AskOption {
                    label: s(o, "label"),
                    description: s(o, "description"),
                })
                .filter(|o| !o.label.is_empty())
                .collect(),
        })
        .filter(|q| !q.question.is_empty())
        .collect();
    if questions.is_empty() {
        return None;
    }
    Some(AgentAsk {
        id: s(&v, "id"),
        agent: s(&v, "agent"),
        project: folder_name(&s(&v, "cwd")),
        questions,
        answerable: true,
    })
}

#[tauri::command]
pub fn get_agent_asks() -> Vec<AgentAsk> {
    ASKS.lock().map(|a| a.clone()).unwrap_or_default()
}

/// Sends the user's answers (question → chosen label(s)), or `None` to
/// release the question back to the agent's own UI.
#[tauri::command]
pub fn answer_agent_ask(
    app: AppHandle,
    id: String,
    answers: Option<HashMap<String, String>>,
) -> Result<(), String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("bad id".into());
    }
    // Codex questions are answered in Codex; the notch only dismisses them.
    if id.starts_with("codex-") {
        close_ask(&app, &id);
        return Ok(());
    }
    let dir = asks_dir().ok_or("no APPDATA")?;
    let body = serde_json::json!({ "answers": answers }).to_string();
    let tmp = dir.join(format!("{id}.answer.tmp"));
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join(format!("{id}.answer.json"))).map_err(|e| e.to_string())?;
    close_ask(&app, &id);
    Ok(())
}

fn open_ask(app: &AppHandle, ask: AgentAsk) {
    if let Ok(mut asks) = ASKS.lock() {
        asks.retain(|a| a.id != ask.id);
        asks.push(ask.clone());
    }
    crate::services::follow_cursor_monitor(app);
    let _ = app.emit("agent-ask", ask);
}

fn close_ask(app: &AppHandle, id: &str) {
    let removed = ASKS
        .lock()
        .map(|mut asks| {
            let before = asks.len();
            asks.retain(|a| a.id != id);
            before != asks.len()
        })
        .unwrap_or(false);
    if removed {
        let _ = app.emit("agent-ask-closed", id);
    }
}

/// Picks up new questions and notices withdrawn ones.
fn sync_asks(app: &AppHandle) {
    let Some(dir) = asks_dir() else { return };
    let mut on_disk = HashSet::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if id.ends_with(".answer") {
            continue;
        }
        // The hook was killed (turn cancelled) and never cleaned up.
        if !recent_within(&path, ASK_MAX_AGE) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        // Answered; the hook removes both files once it has read the answer.
        if dir.join(format!("{id}.answer.json")).exists() {
            continue;
        }
        on_disk.insert(id.to_string());
        let known = ASKS
            .lock()
            .map(|a| a.iter().any(|x| x.id == id))
            .unwrap_or(true);
        if known {
            continue;
        }
        let Some(mut ask) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| parse_ask(&t))
        else {
            continue;
        };
        ask.id = id.to_string();
        open_ask(app, ask);
    }
    let gone: Vec<String> = ASKS
        .lock()
        .map(|a| {
            a.iter()
                .filter(|x| x.agent == "claude" && !on_disk.contains(&x.id))
                .map(|x| x.id.clone())
                .collect()
        })
        .unwrap_or_default();
    for id in gone {
        close_ask(app, &id);
    }
}

fn recent_within(path: &Path, age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|a| a < age)
}

fn publish(app: &AppHandle, ev: AgentEvent) {
    if let Ok(mut latest) = LATEST.lock() {
        latest.retain(|e| e.agent != ev.agent);
        latest.push(ev.clone());
    }
    // A new prompt or an interrupt makes that agent's open question moot.
    if ev.kind == "working" || ev.kind == "interrupted" {
        let stale: Vec<String> = ASKS
            .lock()
            .map(|a| {
                a.iter()
                    .filter(|x| x.agent == ev.agent)
                    .map(|x| x.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(dir) = asks_dir() {
            for id in stale {
                let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
                close_ask(app, &id);
            }
        }
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
            sync_asks(&app);

            for (path, tail) in tails.iter_mut() {
                let events = read_new(path, tail);
                for update in tail.session.ask_updates.drain(..) {
                    match update {
                        AskUpdate::Open(ask) => open_ask(&app, ask),
                        AskUpdate::Close(id) => close_ask(&app, &id),
                    }
                }
                for mut ev in events {
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
        assert_eq!(
            parse_iso_ms("2026-11-07T05:43:29.548Z"),
            Some(1_794_030_209_548)
        );
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
        assert!(s
            .feed(r#"{"type":"user","isSidechain":true,"message":{"content":"sub"}}"#)
            .is_none());
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
    fn parses_hook_requests_including_powershell_single_items() {
        let ask = parse_ask(
            "\u{feff}{\"id\":\"claude-1\",\"agent\":\"claude\",\"cwd\":\"C:\\\\p\\\\bloom\",\"questions\":{\"question\":\"Ship it?\",\"header\":\"Ship\",\"multiSelect\":false,\"options\":[{\"label\":\"Yes\",\"description\":\"push now\"},{\"label\":\"No\"}]}}",
        )
        .unwrap();
        assert_eq!(ask.project, "bloom");
        assert_eq!(ask.questions.len(), 1);
        assert_eq!(
            ask.questions[0].options[1],
            AskOption {
                label: "No".into(),
                description: String::new()
            }
        );
        assert!(parse_ask(r#"{"id":"x","questions":[]}"#).is_none());
    }

    #[test]
    fn codex_questions_open_and_settle() {
        let mut s = Session::new(Agent::Codex);
        s.feed(r#"{"type":"session_meta","payload":{"id":"t1","cwd":"C:\\p\\mp3"}}"#);
        // Desktop async form: string options, answered by a later user message.
        let ev = s
            .feed(r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input_async","call_id":"call_A","arguments":"{\"questions\":[{\"title\":\"What player?\",\"options\":[\"Portable\",\"USB\"]}]}"}}"#)
            .unwrap();
        assert_eq!((ev.kind, ev.message.as_str()), ("question", "What player?"));
        let AskUpdate::Open(ask) = s.ask_updates.remove(0) else {
            panic!()
        };
        assert_eq!(ask.id, "codex-call_A");
        assert_eq!(ask.project, "mp3");
        assert_eq!(ask.questions[0].options[1].label, "USB");
        assert!(!ask.answerable);
        s.feed(r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_A","output":"{\"accepted\":true}"}}"#);
        assert!(s.ask_updates.is_empty(), "accepted is not an answer");
        s.feed(r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<send_user_message_question_reply>[{\"questionItemId\":\"[\\\"request_user_input_async\\\",\\\"call_A\\\",0]\",\"answer\":\"USB\"}]</send_user_message_question_reply>"}]}}"#);
        assert_eq!(
            s.ask_updates.remove(0),
            AskUpdate::Close("codex-call_A".into())
        );
        // Blocking form: label/description options, answered by its output.
        s.feed(r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"call_B","arguments":"{\"questions\":[{\"id\":\"src\",\"header\":\"Source\",\"question\":\"Files or Spotify?\",\"options\":[{\"label\":\"Files\",\"description\":\"offline\"}]}]}"}}"#);
        let AskUpdate::Open(b) = s.ask_updates.remove(0) else {
            panic!()
        };
        assert_eq!(b.questions[0].options[0].description, "offline");
        s.feed(r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_B","output":"{\"answers\":{\"src\":{\"answers\":[\"Files\"]}}}"}}"#);
        assert_eq!(
            s.ask_updates.remove(0),
            AskUpdate::Close("codex-call_B".into())
        );
    }

    #[test]
    fn question_detection_ignores_trailing_markdown() {
        assert!(ends_with_question("Done.\n\n**Want me to commit?**\n"));
        assert!(!ends_with_question("Is it? Yes. Fixed."));
        assert!(!ends_with_question(""));
    }
}
