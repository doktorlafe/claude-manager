use std::env;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

mod haiku;
mod sessions;
mod tmux;
mod tui;

const USAGE: &str = "\
claude-manager - přehled běžících Claude Code sessions seřazený podle toho, která vás potřebuje

Použití:
  claude-manager           interaktivní přehled v terminálu
  claude-manager --once    totéž, ale po přepnutí do session skončí (pro tmux popup)

Sessions, které dostaly finální odpověď, přečte Haiku a podle výsledku je seřadí:
potvrzení > otázka > problém > hotovo > odpověď > pracuje.
Enter přepne do tmux panelu vybrané session, n spustí novou ve zvolené složce
jako další okno vedle přehledu. Mimo tmux si přehled spustí vlastní tmux session
\"claude-manager\"; q přehled zavře, sessions běží dál a další spuštění se k nim vrátí.

Model pro analýzu určuje CLAUDE_MANAGER_MODEL (výchozí haiku).";

fn home() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn cache_dir() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map_or_else(|| home().join(".cache"), PathBuf::from)
        .join("claude-manager")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let once = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => false,
        ["--once"] => true,
        ["-h" | "--help" | "help"] => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    if !io::stdout().is_terminal() {
        eprintln!("error: claude-manager potřebuje terminál");
        return ExitCode::from(2);
    }
    // New sessions open as windows next to the manager, so it has to live in tmux.
    if tmux::installed() && !tmux::inside() {
        eprintln!("error: {}", tmux::enter());
        return ExitCode::FAILURE;
    }
    match tui::run(once) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
