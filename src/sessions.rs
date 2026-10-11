//! Running Claude Code sessions, as Claude Code itself records them under `~/.claude`.

use std::env;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::home;

/// How much of the end of a transcript is read to find the last turn.
const TAIL: u64 = 1 << 20;

pub enum Status {
    Busy,
    Idle,
    /// Stopped on a dialog (typically a permission prompt); holds what it waits for.
    Waiting(String),
}

pub struct Session {
    pub pid: u32,
    pub id: String,
    pub cwd: PathBuf,
    pub name: String,
    pub status: Status,
    /// When the status last changed, in milliseconds since the epoch.
    pub since_ms: u64,
    pub transcript: PathBuf,
}

/// The last exchange of a transcript.
#[derive(Default)]
pub struct Turn {
    /// Uuid of the final assistant message; empty when there is none yet.
    pub key: String,
    pub prompt: String,
    pub answer: String,
    pub title: Option<String>,
}

fn claude_dir() -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR").map_or_else(|| home().join(".claude"), PathBuf::from)
}

pub fn load() -> Vec<Session> {
    let Ok(rd) = fs::read_dir(claude_dir().join("sessions")) else {
        return Vec::new();
    };
    rd.flatten().filter_map(|e| parse(&e.path())).collect()
}

fn parse(path: &Path) -> Option<Session> {
    if path.extension()? != "json" {
        return None;
    }
    let v: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    // `claude -p` runs (our own Haiku calls among them) register as "sdk-cli".
    if v["kind"] != "interactive" || v["entrypoint"] != "cli" {
        return None;
    }
    let pid = u32::try_from(v["pid"].as_u64()?).ok()?;
    // The file outlives a session that was killed.
    if !Path::new(&format!("/proc/{pid}")).exists() {
        return None;
    }
    let id = v["sessionId"].as_str()?.to_string();
    let cwd = PathBuf::from(v["cwd"].as_str()?);
    let status = match v["status"].as_str() {
        Some("busy") => Status::Busy,
        Some("waiting") => Status::Waiting(v["waitingFor"].as_str().unwrap_or("").to_string()),
        _ => Status::Idle,
    };
    let since_ms = ["statusUpdatedAt", "updatedAt", "startedAt"]
        .iter()
        .find_map(|k| v[*k].as_u64())
        .unwrap_or(0);
    Some(Session {
        pid,
        name: v["name"].as_str().unwrap_or("").to_string(),
        status,
        since_ms,
        transcript: transcript_path(&cwd, &id),
        id,
        cwd,
    })
}

fn transcript_path(cwd: &Path, id: &str) -> PathBuf {
    let projects = claude_dir().join("projects");
    let file = format!("{id}.jsonl");
    let slug: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let direct = projects.join(slug).join(&file);
    if direct.exists() {
        return direct;
    }
    // The transcript stays in the project the session started in, even after a `cd`.
    fs::read_dir(&projects)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join(&file))
        .find(|p| p.exists())
        .unwrap_or(direct)
}

fn user_text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            if blocks.iter().any(|b| b["type"] == "tool_result") {
                return None;
            }
            let texts: Vec<&str> = blocks.iter().filter_map(|b| b["text"].as_str()).collect();
            texts.join("\n")
        }
        _ => return None,
    };
    let text = text.trim();
    // Command output and other harness records are wrapped in tags.
    (!text.is_empty() && !text.starts_with('<')).then(|| text.to_string())
}

fn assistant_text(content: &Value) -> String {
    let blocks = content.as_array().map(Vec::as_slice).unwrap_or_default();
    let texts: Vec<&str> = blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    texts.join("\n").trim().to_string()
}

pub fn read_turn(path: &Path) -> Turn {
    let mut turn = Turn::default();
    let Ok(mut file) = File::open(path) else {
        return turn;
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let start = len.saturating_sub(TAIL);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return turn;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut last_prompt = None;
    // When the read starts mid-file, the first line is cut.
    for line in text.lines().skip(usize::from(start > 0)) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["isSidechain"] == true || v["isMeta"] == true {
            continue;
        }
        match v["type"].as_str() {
            Some("ai-title") => turn.title = v["aiTitle"].as_str().map(str::to_string),
            Some("last-prompt") => last_prompt = v["lastPrompt"].as_str().map(str::to_string),
            Some("user") => {
                if let Some(prompt) = user_text(&v["message"]["content"]) {
                    turn.prompt = prompt;
                    turn.answer.clear();
                    turn.key.clear();
                }
            }
            Some("assistant") => {
                let answer = assistant_text(&v["message"]["content"]);
                if !answer.is_empty() {
                    turn.answer = answer;
                    turn.key = v["uuid"].as_str().unwrap_or("").to_string();
                }
            }
            _ => {}
        }
    }
    if turn.prompt.is_empty() {
        turn.prompt = last_prompt.unwrap_or_default();
    }
    turn
}
