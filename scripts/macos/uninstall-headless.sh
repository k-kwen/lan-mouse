#!/bin/sh
set -eu

INSTALL_DIR="${INSTALL_DIR:-$HOME/Tools/lan-mouse}"
LABEL="${LABEL:-com.$USER.lan-mouse}"
REMOVE_CONFIG=0
REMOVE_LOGS=0
KEEP_BINARY=0

usage() {
  cat <<'USAGE'
Usage: uninstall-headless.sh [options]

Options:
  --install-dir DIR   install directory (default: ~/Tools/lan-mouse)
  --label LABEL       LaunchAgent label (default: com.$USER.lan-mouse)
  --keep-binary       keep installed binary
  --remove-config     remove ~/.config/lan-mouse
  --remove-logs       remove ~/Library/Logs/lan-mouse
  -h, --help          show this help
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --install-dir)
      INSTALL_DIR="${2:?missing value for --install-dir}"
      shift 2
      ;;
    --label)
      LABEL="${2:?missing value for --label}"
      shift 2
      ;;
    --keep-binary)
      KEEP_BINARY=1
      shift
      ;;
    --remove-config)
      REMOVE_CONFIG=1
      shift
      ;;
    --remove-logs)
      REMOVE_LOGS=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

expand_path() {
  case "$1" in
    "~") printf '%s\n' "$HOME" ;;
    "~/"*) printf '%s/%s\n' "$HOME" "${1#~/}" ;;
    *) printf '%s\n' "$1" ;;
  esac
}

INSTALL_DIR="$(expand_path "$INSTALL_DIR")"
INSTALL_BIN="$INSTALL_DIR/lan-mouse"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
UID_VALUE="$(id -u)"

launchctl bootout "gui/$UID_VALUE/$LABEL" 2>/dev/null || true
launchctl bootout "gui/$UID_VALUE" "$PLIST" 2>/dev/null || true

if [ -x "$INSTALL_BIN" ]; then
  PIDS="$(pgrep -f "$INSTALL_BIN" || true)"
  if [ -n "$PIDS" ]; then
    echo "$PIDS" | xargs kill 2>/dev/null || true
    sleep 1
    PIDS="$(pgrep -f "$INSTALL_BIN" || true)"
    if [ -n "$PIDS" ]; then
      echo "$PIDS" | xargs kill -9 2>/dev/null || true
    fi
  fi
fi

rm -f "$PLIST"

if [ "$KEEP_BINARY" -eq 0 ]; then
  rm -f "$INSTALL_BIN"
  rmdir "$INSTALL_DIR/backups" 2>/dev/null || true
  rmdir "$INSTALL_DIR" 2>/dev/null || true
fi

if [ "$REMOVE_CONFIG" -eq 1 ]; then
  rm -rf "$HOME/.config/lan-mouse"
fi

if [ "$REMOVE_LOGS" -eq 1 ]; then
  rm -rf "$HOME/Library/Logs/lan-mouse"
fi

cat <<EOF
Uninstalled LaunchAgent: $LABEL

Preserved by default unless flags were used:
  config: $HOME/.config/lan-mouse
  logs:   $HOME/Library/Logs/lan-mouse

Privacy permissions are not reset automatically. Remove lan-mouse manually from:
  System Settings -> Privacy & Security -> Accessibility
  System Settings -> Privacy & Security -> Input Monitoring
EOF
