#!/usr/bin/env bash
set -euo pipefail

if grep -nIH $'\342\200\246' "$@"; then
  printf 'Use ... in place of U+2026.\n' >&2
  exit 1
else
  status=$?
  if [ "$status" -ne 1 ]; then
    exit "$status"
  fi
fi
