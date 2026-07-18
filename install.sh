#!/usr/bin/env bash
# CodeGraph installer: build a release binary, install it, and (optionally) wire
# it into Claude Code as an MCP server. Local, no network beyond crates.io.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
DEST="${CODEGRAPH_BIN_DIR:-$HOME/.local/bin}"

echo "==> Building codegraph (release)..."
# local-embed: bundled bge-small so `semantic` works out of the box (a stock
# install that advertises semantic_search and then hard-fails is a field bug).
# indexstore: Swift compiler-grade tier — macOS only.
FEATURES="local-embed"
[ "$(uname)" = "Darwin" ] && FEATURES="$FEATURES,indexstore"
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p codegraph-cli --features "$FEATURES"

BIN="$ROOT/target/release/codegraph"
[ -x "$BIN" ] || { echo "error: build did not produce $BIN" >&2; exit 1; }

mkdir -p "$DEST"
install -m 0755 "$BIN" "$DEST/codegraph"
echo "==> Installed: $DEST/codegraph ($("$DEST/codegraph" --version))"

case ":$PATH:" in
  *":$DEST:"*) ;;
  *) echo "NOTE: $DEST is not on PATH. Add it, e.g.:"; echo "      echo 'export PATH=\"$DEST:\$PATH\"' >> ~/.zshrc" ;;
esac

cat <<EOF

==> Use from Claude Code: add this to ~/.claude.json under "mcpServers":

  "codegraph": { "command": "codegraph", "args": ["mcp", "--path", "/path/to/your/repo"] }

Then in a repo:  codegraph index .   and ask Claude to "use codegraph to ...".
EOF
