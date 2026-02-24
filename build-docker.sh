#!/bin/bash
# Build diskclone.exe and diskclone-visual.exe for Windows using Docker (from Linux/macOS)
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

GIT_HASH=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
echo "Building diskclone for Windows (x86_64-pc-windows-gnu) via Docker [${GIT_HASH}]..."
docker build -t diskclone-builder .

echo ""
echo "Running build (CLI + GUI)..."
docker run --rm \
  -v "$SCRIPT_DIR:/app" \
  -w /app \
  diskclone-builder \
  cargo build --release --target x86_64-pc-windows-gnu -p diskclone -p diskclone-visual

RELEASE_DIR="$SCRIPT_DIR/target/x86_64-pc-windows-gnu/release"
EXE="$RELEASE_DIR/diskclone.exe"
VISUAL_EXE="$RELEASE_DIR/diskclone-visual.exe"

if [[ ! -f "$EXE" ]]; then
  echo "Error: Expected binary not found at $EXE"
  exit 1
fi

# Copy to versioned filenames
cp "$EXE" "$RELEASE_DIR/diskclone-${GIT_HASH}.exe"
cp "$VISUAL_EXE" "$RELEASE_DIR/diskclone-visual-${GIT_HASH}.exe"

echo ""
echo "Done! Binaries:"
ls -la "$EXE" "$VISUAL_EXE"
echo "Versioned:"
ls -la "$RELEASE_DIR/diskclone-${GIT_HASH}.exe" "$RELEASE_DIR/diskclone-visual-${GIT_HASH}.exe"
