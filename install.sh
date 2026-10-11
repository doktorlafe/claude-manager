#!/bin/sh
# Sestaví aktuální verzi a nainstaluje ji do ~/.local/bin.
set -eu

cd "$(dirname "$0")"
bin_dir="$HOME/.local/bin"
export CARGO_TARGET_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/claude-manager-target"

if ! command -v cargo >/dev/null 2>&1; then
    if [ -x "$HOME/.cargo/bin/cargo" ]; then
        PATH="$HOME/.cargo/bin:$PATH"
    else
        echo "chyba: chybí cargo, nainstalujte Rust z https://rustup.rs" >&2
        exit 1
    fi
fi

cargo build --release --locked
install -Dm755 "$CARGO_TARGET_DIR/release/claude-manager" "$bin_dir/claude-manager"
echo
echo "nainstalováno: $bin_dir/claude-manager"

case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo "pozor: $bin_dir není v PATH" ;;
esac
command -v tmux >/dev/null 2>&1 || echo "pozor: chybí tmux (sudo apt install tmux), bez něj nejde mezi sessions přepínat"
command -v claude >/dev/null 2>&1 || echo "pozor: chybí claude v PATH, bez něj nepoběží analýza přes Haiku"
