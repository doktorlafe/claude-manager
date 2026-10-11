# claude-manager

An interactive terminal overview of the Claude Code sessions running on this
machine, written in Rust. It sorts them by how much they need you and jumps to
the tmux pane of the one you pick.

- finds every running interactive `claude` and the tmux pane it lives in
- when a session receives its final answer, Haiku reads the end of the chat and
  classifies it; the list is sorted by the result
- `Enter` takes you to the session, `n` starts a new one in a folder you pick

The interface and its messages are in Czech.

## Installation

Requires [Rust](https://rustup.rs), `tmux` and a logged-in `claude`.

```sh
sudo apt install tmux
sh install.sh
```

The script builds a release binary (in `~/.cache/claude-manager-target`) and
installs it to `~/.local/bin/claude-manager`. Updates are installed the same way.

## Usage

```sh
claude-manager
```

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | move |
| `g` / `G` | first / last session |
| `Enter` | jump to the session's tmux pane |
| `n` | start a new session: opens a folder browser |
| `a` | have Haiku read the selected chat again |
| `q` | quit |

In the folder browser `Enter`/`→` opens a folder, `←` goes up, `Space` starts
`claude` in the highlighted folder (in a new tmux session named after it) and
`.` shows hidden folders.

The lower pane shows the selected session's last prompt and the end of its
final answer, so many decisions can be made without jumping at all.

## Priority

Most urgent first; among equals, the session that has waited longest wins.

| Label | Meaning |
| --- | --- |
| `POTVRDIT` | stopped on a dialog, typically a permission prompt |
| `OTÁZKA` | finished by asking you something |
| `PROBLÉM` | could not finish: an error or a blocker |
| `HOTOVO` | finished the work, ready for review or the next task |
| `DOPSÁNO` | has a final answer that Haiku has not classified (yet) |
| `ODPOVĚĎ` | a purely informational reply, nothing left to do |
| `NOVÁ` | no answer yet |
| `PRACUJE` | still working |

Whether a session is working, idle or waiting comes from Claude Code's own
status files (`~/.claude/sessions`), so that part is instant and free. Haiku is
only asked once per final answer, to tell the four idle cases apart and to
write the one-line summary; it gets the last prompt and the final message, not
the whole chat. The call goes through `claude -p --model haiku`, so it uses
your existing login and needs no API key. Verdicts are cached in
`~/.cache/claude-manager`. Set `CLAUDE_MANAGER_MODEL` to use a different model.

## Jumping

Started **outside tmux**, `Enter` attaches the terminal to the session; detach
(`Ctrl+b d`) and you are back in the list. This is the simplest way to use it:
the manager is home, every session is one `Enter` away.

Started **inside tmux**, `Enter` switches the client to that pane and the
manager keeps running in its own window (`Ctrl+b L` returns to it). It also
works as a popup that closes after the jump; add to `~/.tmux.conf`:

```
bind m display-popup -E -w 90% -h 90% "claude-manager --once"
```

Sessions started outside tmux are listed and classified too (`mimo tmux`), but
there is no pane to jump to.
