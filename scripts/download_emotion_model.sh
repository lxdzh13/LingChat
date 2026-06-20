#!/usr/bin/env bash
# ============================================================
#  Download ONNX emotion model (model.onnx.data)
#  See download_emotion_model.ps1 for details.
# ============================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODEL_DIR="$SCRIPT_DIR/../data/third_party/emotion_model_19emo"
DATA_FILE="$MODEL_DIR/model.onnx.data"

# TODO: replace with your fork's GitHub Releases URL
DOWNLOAD_URL="https://github.com/你的账号/LingChat/releases/download/v0.1.0/model.onnx.data"

mkdir -p "$MODEL_DIR"

if [ -f "$DATA_FILE" ]; then
    echo "model.onnx.data already exists, skipping: $DATA_FILE"
    exit 0
fi

echo "Downloading model.onnx.data (~390 MB)..."
echo "  from: $DOWNLOAD_URL"
echo "  to:   $DATA_FILE"

if command -v curl >/dev/null 2>&1; then
    curl -L --fail -o "$DATA_FILE" "$DOWNLOAD_URL"
elif command -v wget >/dev/null 2>&1; then
    wget -O "$DATA_FILE" "$DOWNLOAD_URL"
else
    echo "Error: need curl or wget" >&2
    exit 1
fi

echo "Download complete."