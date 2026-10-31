#!/usr/bin/env bash
set -euo pipefail

set +u
SCRIPT_SOURCE="${BASH_SOURCE[0]:-}"
set -u
ROOT="$(cd "$(dirname "${SCRIPT_SOURCE:-.}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/lafd"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"

if ! command -v systemctl >/dev/null 2>&1; then
  echo "install: systemctl is required" >&2
  exit 1
fi
if [[ ! -x "$CARGO" ]]; then
  CARGO="$(command -v cargo || true)"
fi
if [[ -z "$CARGO" ]]; then
  echo "install: cargo not found (install rustup or set CARGO=/path/to/cargo)" >&2
  exit 1
fi

# A piped install (`curl ... | bash`) has no source tree next to the script.
REPO_URL="${LAFD_REPO:-https://github.com/pharmaragist/linux-audio-focus-daemon.git}"
SRC_DIR="${LAFD_SRC_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/lafd/src}"
if [[ ! -f "$ROOT/Cargo.toml" ]]; then
  if ! command -v git >/dev/null 2>&1; then
    echo "install: git is required for a piped install" >&2
    exit 1
  fi
  echo "==> fetching sources to $SRC_DIR"
  rm -rf "$SRC_DIR"
  if ! git clone --depth 1 "$REPO_URL" "$SRC_DIR" >/dev/null 2>&1 </dev/null; then
    echo "install: could not clone $REPO_URL" >&2
    echo "         set LAFD_REPO to a fork or private remote and retry" >&2
    exit 1
  fi
  ROOT="$SRC_DIR"
fi

echo "==> building"
(
  cd "$ROOT"
  "$CARGO" build --release
)

echo "==> installing binaries to $BIN_DIR"
install -d -m755 "$BIN_DIR"
install -m755 "$ROOT/target/release/lafd" "$BIN_DIR/lafd"
install -m755 "$ROOT/target/release/lafdctl" "$BIN_DIR/lafdctl"

echo "==> installing systemd user unit"
install -d -m755 "$UNIT_DIR"
install -m644 "$ROOT/lafd.service" "$UNIT_DIR/lafd.service"

echo "==> installing default config"
install -d -m755 "$CONFIG_DIR"
if [[ ! -f "$CONFIG_DIR/config.json" ]]; then
  install -m644 "$ROOT/config.example.json" "$CONFIG_DIR/config.json"
else
  echo "    keeping existing $CONFIG_DIR/config.json"
fi

# The old Go daemon must not run next to lafd, or both will fight over players.
if [[ -f "$UNIT_DIR/audio-focus.service" ]]; then
  echo "==> disabling the old audio-focus.service"
  systemctl --user disable --now audio-focus.service >/dev/null 2>&1 || true
fi

echo "==> enabling and restarting lafd.service"
systemctl --user daemon-reload
systemctl --user enable lafd.service >/dev/null
systemctl --user restart lafd.service

sleep 0.3
if systemctl --user is-active --quiet lafd.service; then
  echo "==> running"
  systemctl --user --no-pager --lines=5 status lafd.service || true
  echo
  echo "Socket: ${XDG_RUNTIME_DIR:-/tmp}/lafd.sock"
  echo "Try: lafdctl status"
else
  echo "install: service did not stay running" >&2
  systemctl --user --no-pager --lines=30 status lafd.service >&2 || true
  exit 1
fi
