//! Finding the tmux pane a session runs in and moving the terminal there.

use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Clone)]
pub struct Pane {
    /// Unique pane id, e.g. `%3`.
    pub id: String,
    pub pid: u32,
    pub session: String,
    /// `session:window.pane` as the user knows it.
    pub label: String,
}

fn tmux(args: &[&str]) -> Result<String, String> {
    let out = Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("tmux: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "tmux: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

pub fn installed() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether we run inside tmux ourselves.
pub fn inside() -> bool {
    env::var_os("TMUX").is_some_and(|v| !v.is_empty())
}

const PANE_FORMAT: &str =
    "#{pane_id}\t#{pane_pid}\t#{session_name}\t#{session_name}:#{window_index}.#{pane_index}";

fn parse_pane(line: &str) -> Option<Pane> {
    let mut f = line.split('\t');
    Some(Pane {
        id: f.next()?.to_string(),
        pid: f.next()?.parse().ok()?,
        session: f.next()?.to_string(),
        label: f.next()?.to_string(),
    })
}

/// All panes of the tmux server; empty when no server runs.
pub fn panes() -> Vec<Pane> {
    tmux(&["list-panes", "-a", "-F", PANE_FORMAT])
        .map(|out| out.lines().filter_map(parse_pane).collect())
        .unwrap_or_default()
}

fn parent_pid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may itself contain spaces and parentheses.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The pane whose shell is an ancestor of `pid`.
pub fn pane_of(pid: u32, panes: &[Pane]) -> Option<&Pane> {
    let mut cur = pid;
    for _ in 0..32 {
        if let Some(pane) = panes.iter().find(|p| p.pid == cur) {
            return Some(pane);
        }
        cur = parent_pid(cur).filter(|p| *p > 1)?;
    }
    None
}

/// Moves the tmux client we run in to `pane`.
pub fn switch(pane: &Pane) -> Result<(), String> {
    tmux(&["switch-client", "-t", &pane.id]).map(drop)
}

/// Attaches this terminal to the session of `pane`; returns once the user detaches.
pub fn attach(pane: &Pane) -> Result<(), String> {
    tmux(&["select-window", "-t", &pane.id])?;
    tmux(&["select-pane", "-t", &pane.id])?;
    let status = Command::new("tmux")
        .args(["attach-session", "-t", &format!("={}", pane.session)])
        .status()
        .map_err(|e| format!("tmux: {e}"))?;
    if !status.success() {
        return Err("tmux attach selhal".into());
    }
    Ok(())
}

/// Starts `claude` in `dir` in a new detached session named after the directory.
pub fn new_session(dir: &Path) -> Result<Pane, String> {
    let base: String = dir
        .file_name()
        .map_or("claude".into(), |n| n.to_string_lossy().into_owned())
        .replace(['.', ':'], "_");
    let taken = |name: &str| tmux(&["has-session", "-t", &format!("={name}")]).is_ok();
    let name = (1..)
        .map(|n| {
            if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            }
        })
        .find(|name| !taken(name))
        .unwrap_or(base);
    let dir = dir.to_string_lossy();
    let out = tmux(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        PANE_FORMAT,
        "-s",
        &name,
        "-c",
        &dir,
        "claude",
    ])?;
    parse_pane(&out).ok_or_else(|| format!("tmux: nečekaný výstup {out:?}"))
}
