#!/usr/bin/env bash
set -euo pipefail

if grep -nIH $'\342\200\246' "$@"; then
  printf 'Error: the lines above use U+2026 in place of ...\n' >&2
  exit 1
else
  status=$?
  if [ "$status" -ne 1 ]; then
    exit "$status"
  fi
fi
