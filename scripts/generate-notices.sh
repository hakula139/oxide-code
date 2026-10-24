#!/usr/bin/env bash

set -euo pipefail

if [[ "$#" -lt 2 || "$#" -gt 3 || ("$#" -eq 3 && "$3" != --offline) ]]; then
  echo "Usage: $0 <target> <output> [--offline]" >&2
  exit 2
fi

target="$1"
output="$2"
shift 2

# A workspace scan would also count ox-core's test-only `testing` dependencies.
cargo about generate \
  --locked \
  --manifest-path crates/ox/Cargo.toml \
  --target "$target" \
  --config about.toml \
  about.hbs \
  -o "$output" \
  "$@"
cat licenses/syntect-assets.txt >>"$output"
