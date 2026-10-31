#!/bin/sh
# Download the pinned public Qwen tokenizer and verify its SHA-256, exactly as
# the original image build did (embedding_config.download_tokenizer): fixed
# Hugging Face revision, fixed checksum, no runtime downloads, no model weights.
# The pins are read from compat/producer-manifest.json so they can never drift
# from the Python producer contract.
#
# Usage: scripts/fetch_tokenizer.sh <output-path>
set -eu
out=${1:?output path required}
manifest=$(dirname "$0")/../compat/producer-manifest.json
field() { python3 -I -c "import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])" "$manifest" "$1"; }
model=$(field tokenizer_name)
revision=$(field tokenizer_revision)
sha=$(field tokenizer_sha256)
tmp="$out.partial"
curl -fsSL --proto '=https' --max-filesize 20971520 -o "$tmp" "https://huggingface.co/$model/resolve/$revision/tokenizer.json"
echo "$sha  $tmp" | sha256sum -c - >/dev/null
chmod 0644 "$tmp"
mv "$tmp" "$out"
echo "tokenizer verified: $sha"
