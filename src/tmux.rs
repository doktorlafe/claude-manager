//! Finding the tmux pane a session runs in and moving the terminal there.

use std::env;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// The session the manager starts for itself when it is run outside tmux.
const HOME_SESSION: &str = "claude-manager";
/// The name of the manager's own window there.
const HOME_WINDOW: &str = "manager";

#[derive(Clone)]
pub struct Pane {
    /// Unique pane id, e.g. `%3`.
    pub id: String,
    pub pid: u32,
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

const PANE_FORMAT: &str = "#{pane_id}\t#{pane_pid}\t#{session_name}:#{window_index}.#{pane_index}";

fn parse_pane(line: &str) -> Option<Pane> {
    let mut f = line.split('\t');
    Some(Pane {
        id: f.next()?.to_string(),
        pid: f.next()?.parse().ok()?,
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

/// Quotes `s` for the shell tmux runs commands with.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Replaces this process with tmux showing the manager in its home session, starting
/// either as needed. Returns only when that failed.
pub fn enter() -> String {
    let exe = match env::current_exe() {
        Ok(exe) => sh_quote(&exe.to_string_lossy()),
        Err(e) => return format!("nelze zjistit cestu k programu: {e}"),
    };
    let session = format!("={HOME_SESSION}");
    let mut cmd = Command::new("tmux");
    if tmux(&["has-session", "-t", &session]).is_ok() {
        let windows = tmux(&[
            "list-windows",
            "-t",
            &session,
            "-F",
            "#{window_name}\t#{window_id}",
        ])
        .unwrap_or_default();
        let manager = windows
            .lines()
            .find_map(|l| l.strip_prefix(HOME_WINDOW)?.strip_prefix('\t'));
        // The sessions outlive the manager; it rejoins them in a window of its own.
        let placed = match manager {
            Some(id) => tmux(&["select-window", "-t", id]),
            None => tmux(&[
                "new-window",
                "-t",
                &format!("{session}:"),
                "-n",
                HOME_WINDOW,
                &exe,
            ]),
        };
        if let Err(e) = placed {
            return e;
        }
        cmd.args(["attach-session", "-t", &session]);
    } else {
        cmd.args(["new-session", "-s", HOME_SESSION, "-n", HOME_WINDOW, &exe]);
    }
    format!("tmux: {}", cmd.exec())
}

/// Our pane, when we run as the manager window of the home session.
pub struct Home {
    pane: String,
}

impl Home {
    pub fn find() -> Option<Home> {
        let pane = env::var("TMUX_PANE").ok()?;
        let place = tmux(&[
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{session_name}\t#{window_name}",
        ])
        .ok()?;
        (place == format!("{HOME_SESSION}\t{HOME_WINDOW}")).then_some(Home { pane })
    }

    /// Shows in the session's status line how to get back to the manager.
    pub fn show_way_back(&self) {
        let index = tmux(&["display-message", "-p", "-t", &self.pane, "#{window_index}"]);
        let hint = format!(" #{{prefix}} {} = přehled ", index.unwrap_or_default());
        let _ = tmux(&["set-option", "-t", &self.pane, "status-right", &hint]);
    }

    /// Returns the terminal to the shell; the session and its windows keep running.
    pub fn detach(&self) {
        let _ = tmux(&["detach-client", "-s", &self.pane]);
    }
}

/// Starts `claude` in `dir` in a new window of the session we run in and shows it.
pub fn new_window(dir: &Path) -> Result<(), String> {
    let name = dir
        .file_name()
        .map_or("claude".into(), |n| n.to_string_lossy().into_owned());
    let dir = dir.to_string_lossy();
    tmux(&["new-window", "-n", &name, "-c", &dir, "claude"]).map(drop)
}
