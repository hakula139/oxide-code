#!/usr/bin/env bash
set -euo pipefail

status=0
for file in "$@"; do
  if ! grep -q '^//!' "$file"; then
    printf 'Error: %s has no //! module doc\n' "$file" >&2
    status=1
  fi
done
exit "$status"
