#!/bin/sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"

INSTALL_DIR="${INSTALL_DIR:-$HOME/Tools/lan-mouse}"
LABEL="${LABEL:-com.$USER.lan-mouse}"
SIGN_IDENTITY="${SIGN_IDENTITY:-Lan-mouse Dev Cert}"
SIGN_IDENTIFIER="${SIGN_IDENTIFIER:-$LABEL}"
SOURCE_BIN="${SOURCE_BIN:-}"
DO_SIGN=1
DO_START=1
OPEN_PRIVACY=1
LOG_LEVEL="${LAN_MOUSE_LOG_LEVEL:-info}"

usage() {
  cat <<'USAGE'
Usage: install-headless.sh [options]

Options:
  --source PATH           lan-mouse binary to install
  --install-dir DIR       install directory (default: ~/Tools/lan-mouse)
  --label LABEL           LaunchAgent label (default: com.$USER.lan-mouse)
  --sign-identity NAME    codesign identity (default: Lan-mouse Dev Cert)
  --sign-identifier ID    codesign identifier (default: LaunchAgent label)
  --no-sign              skip codesign
  --no-start             write LaunchAgent but do not start it
  --no-open-privacy      print permission steps without opening Settings
  --log-level LEVEL      daemon log level (default: LAN_MOUSE_LOG_LEVEL or info)
  -h, --help             show this help
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --source)
      SOURCE_BIN="${2:?missing value for --source}"
      shift 2
      ;;
    --install-dir)
      INSTALL_DIR="${2:?missing value for --install-dir}"
      shift 2
      ;;
    --label)
      LABEL="${2:?missing value for --label}"
      shift 2
      ;;
    --sign-identity)
      SIGN_IDENTITY="${2:?missing value for --sign-identity}"
      shift 2
      ;;
    --sign-identifier)
      SIGN_IDENTIFIER="${2:?missing value for --sign-identifier}"
      shift 2
      ;;
    --no-sign)
      DO_SIGN=0
      shift
      ;;
    --no-start)
      DO_START=0
      shift
      ;;
    --no-open-privacy)
      OPEN_PRIVACY=0
      shift
      ;;
    --log-level)
      LOG_LEVEL="${2:?missing value for --log-level}"
      shift 2
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

xml_escape() {
  sed \
    -e 's/&/\&amp;/g' \
    -e 's/</\&lt;/g' \
    -e 's/>/\&gt;/g' \
    -e 's/"/\&quot;/g' \
    -e "s/'/\&apos;/g"
}

resolve_source() {
  if [ -n "$SOURCE_BIN" ]; then
    printf '%s\n' "$(expand_path "$SOURCE_BIN")"
    return
  fi

  for candidate in \
    "$SCRIPT_DIR/lan-mouse" \
    "$SCRIPT_DIR/../lan-mouse" \
    "$SCRIPT_DIR/../../target/release/lan-mouse" \
    "$PWD/target/release/lan-mouse"
  do
    if [ -x "$candidate" ]; then
      printf '%s\n' "$candidate"
      return
    fi
  done

  echo "could not find lan-mouse binary; pass --source PATH" >&2
  exit 1
}

INSTALL_DIR="$(expand_path "$INSTALL_DIR")"
SOURCE_BIN="$(resolve_source)"
INSTALL_BIN="$INSTALL_DIR/lan-mouse"
LOG_DIR="$HOME/Library/Logs/lan-mouse"
PLIST_DIR="$HOME/Library/LaunchAgents"
PLIST="$PLIST_DIR/$LABEL.plist"
UID_VALUE="$(id -u)"

if [ ! -x "$SOURCE_BIN" ]; then
  echo "source is not executable: $SOURCE_BIN" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR" "$INSTALL_DIR/backups" "$LOG_DIR" "$PLIST_DIR"

if [ -f "$INSTALL_BIN" ]; then
  BACKUP="$INSTALL_DIR/backups/lan-mouse.$(date +%Y%m%d-%H%M%S)"
  echo "backing up existing binary to $BACKUP"
  cp "$INSTALL_BIN" "$BACKUP"
fi

cp "$SOURCE_BIN" "$INSTALL_BIN"
chmod +x "$INSTALL_BIN"
xattr -d com.apple.quarantine "$INSTALL_BIN" 2>/dev/null || true

if [ "$DO_SIGN" -eq 1 ]; then
  if [ -n "$SIGN_IDENTITY" ] && security find-identity -v -p codesigning | grep -F "$SIGN_IDENTITY" >/dev/null 2>&1; then
    codesign --force --sign "$SIGN_IDENTITY" --identifier "$SIGN_IDENTIFIER" "$INSTALL_BIN"
  else
    echo "warning: signing identity not found; using ad-hoc signature" >&2
    codesign --force --sign - --identifier "$SIGN_IDENTIFIER" "$INSTALL_BIN"
  fi
fi

ESC_LABEL="$(printf '%s' "$LABEL" | xml_escape)"
ESC_BIN="$(printf '%s' "$INSTALL_BIN" | xml_escape)"
ESC_STDOUT="$(printf '%s' "$LOG_DIR/daemon.stdout.log" | xml_escape)"
ESC_STDERR="$(printf '%s' "$LOG_DIR/daemon.stderr.log" | xml_escape)"
ESC_LOG_FILE="$(printf '%s' "$LOG_DIR/daemon.log" | xml_escape)"
ESC_LOG_LEVEL="$(printf '%s' "$LOG_LEVEL" | xml_escape)"
touch "$LOG_DIR/daemon.log" "$LOG_DIR/daemon.stdout.log" "$LOG_DIR/daemon.stderr.log"

cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
 "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$ESC_LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$ESC_BIN</string>
    <string>daemon</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>LAN_MOUSE_LOG_FILE</key>
    <string>$ESC_LOG_FILE</string>
    <key>LAN_MOUSE_LOG_LEVEL</key>
    <string>$ESC_LOG_LEVEL</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
    <key>Crashed</key>
    <true/>
  </dict>
  <key>LimitLoadToSessionType</key>
  <string>Aqua</string>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>StandardOutPath</key>
  <string>$ESC_STDOUT</string>
  <key>StandardErrorPath</key>
  <string>$ESC_STDERR</string>
</dict>
</plist>
EOF

plutil -lint "$PLIST" >/dev/null

launchctl bootout "gui/$UID_VALUE/$LABEL" 2>/dev/null || true
launchctl bootout "gui/$UID_VALUE" "$PLIST" 2>/dev/null || true
launchctl bootstrap "gui/$UID_VALUE" "$PLIST"

if [ "$DO_START" -eq 1 ]; then
  launchctl kickstart -k "gui/$UID_VALUE/$LABEL"
fi

echo "installed binary: $INSTALL_BIN"
"$INSTALL_BIN" --version || true
echo "LaunchAgent: $PLIST"
launchctl print "gui/$UID_VALUE/$LABEL" >/dev/null 2>&1 && echo "LaunchAgent loaded: $LABEL"

cat <<EOF

Permission guide:
1. System Settings -> Privacy & Security -> Accessibility
   Add or enable: $INSTALL_BIN
2. System Settings -> Privacy & Security -> Input Monitoring
   Add or enable: $INSTALL_BIN
3. After toggling permissions, restart the daemon:
   launchctl kickstart -k "gui/$UID_VALUE/$LABEL"

Config is preserved at:
  $HOME/.config/lan-mouse/config.toml

Logs:
  $LOG_DIR/daemon.log
  $LOG_DIR/daemon.stderr.log
EOF

if [ "$OPEN_PRIVACY" -eq 1 ]; then
  open "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility" 2>/dev/null || true
  open "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent" 2>/dev/null || true
fi
