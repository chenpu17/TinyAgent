#!/usr/bin/env sh
# Build the TUI release binary and install it onto your PATH.
#
#   ./install.sh                     # → ~/.local/bin/tiny-agent (no sudo)
#   PREFIX=/usr/local ./install.sh   # → /usr/local/bin/tiny-agent (needs sudo)
#
# ~/.local/bin is the default because it needs no sudo. The script prints the
# PATH line to add to your shell profile if that directory isn't on PATH yet.

set -e

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
PREFIX="${PREFIX:-$HOME/.local}"
BINDIR="$PREFIX/bin"
BIN="$BINDIR/tiny-agent"

echo "[install] building release (features=tui)…"
( cd "$SCRIPT_DIR" && cargo build --release --features tui )

mkdir -p "$BINDIR"
install -m 755 "$SCRIPT_DIR/target/release/tiny-agent" "$BIN"
echo "[install] installed → $BIN"

case ":$PATH:" in
  *":$BINDIR:"*)
    echo "[install] $BINDIR is on PATH. Run: tiny-agent"
    ;;
  *)
    echo "[install] note: $BINDIR is NOT on your PATH yet."
    echo "         Add this to your shell profile (~/.zshrc or ~/.bashrc):"
    echo "           export PATH=\"$BINDIR:\$PATH\""
    ;;
esac
