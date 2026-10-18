#!/usr/bin/env bash
# Fail if a given Rust file lacks a //! module doc, naming each such file.

set -euo pipefail

status=0
for file in "$@"; do
  if ! grep -q '^//!' "$file"; then
    printf 'Error: %s has no //! module doc\n' "$file" >&2
    status=1
  fi
done
exit "$status"
