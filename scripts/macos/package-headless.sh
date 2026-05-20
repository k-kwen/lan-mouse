#!/bin/sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)"

OUTPUT_DIR="${OUTPUT_DIR:-$REPO_ROOT/target/dist}"
BINARY="${BINARY:-}"
SKIP_BUILD=0
PACKAGE_NAME="${PACKAGE_NAME:-}"

usage() {
  cat <<'USAGE'
Usage: package-headless.sh [options]

Options:
  --binary PATH       use an existing lan-mouse binary
  --output-dir DIR    output directory (default: target/dist)
  --skip-build        do not run cargo build
  --name NAME         package base name
  -h, --help          show this help
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --binary)
      BINARY="${2:?missing value for --binary}"
      shift 2
      ;;
    --output-dir)
      OUTPUT_DIR="${2:?missing value for --output-dir}"
      shift 2
      ;;
    --skip-build)
      SKIP_BUILD=1
      shift
      ;;
    --name)
      PACKAGE_NAME="${2:?missing value for --name}"
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

if [ -z "$BINARY" ] && [ "$SKIP_BUILD" -eq 0 ]; then
  cargo build -p lan-mouse --release --no-default-features --manifest-path "$REPO_ROOT/Cargo.toml"
fi

if [ -z "$BINARY" ]; then
  BINARY="$REPO_ROOT/target/release/lan-mouse"
fi

if [ ! -x "$BINARY" ]; then
  echo "missing executable binary: $BINARY" >&2
  exit 1
fi

ARCH="$(uname -m)"
VERSION="$("$BINARY" --version 2>/dev/null | awk 'NR == 1 {print $2}')"
if [ -z "$PACKAGE_NAME" ]; then
  PACKAGE_NAME="lan-mouse-macos-headless-$ARCH-$VERSION"
fi

STAGE="$OUTPUT_DIR/$PACKAGE_NAME"
ARCHIVE="$OUTPUT_DIR/$PACKAGE_NAME.tar.gz"

rm -rf "$STAGE" "$ARCHIVE"
mkdir -p "$STAGE"

cp "$BINARY" "$STAGE/lan-mouse"
cp "$SCRIPT_DIR/install-headless.sh" "$STAGE/install-headless.sh"
cp "$SCRIPT_DIR/uninstall-headless.sh" "$STAGE/uninstall-headless.sh"
cp "$SCRIPT_DIR/README.md" "$STAGE/README.md"
chmod +x "$STAGE/lan-mouse" "$STAGE/install-headless.sh" "$STAGE/uninstall-headless.sh"

mkdir -p "$OUTPUT_DIR"
(cd "$OUTPUT_DIR" && tar -czf "$ARCHIVE" "$PACKAGE_NAME")

if command -v shasum >/dev/null 2>&1; then
  shasum -a 256 "$ARCHIVE" > "$ARCHIVE.sha256"
fi

echo "package: $ARCHIVE"
if [ -f "$ARCHIVE.sha256" ]; then
  cat "$ARCHIVE.sha256"
fi
