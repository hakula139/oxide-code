#!/usr/bin/env bash
# Check that comment lines in the given files fit in 100 columns, as docs/conventions/writing.md
# asks. A line is a comment when it starts with `//` in Rust or `#` elsewhere, after indentation.
#
# Columns count characters, so a multibyte character such as an arrow counts once. A line holding
# a URL may run long, since a URL cannot break. Exits 1 when a line is too long.

set -euo pipefail

LC_ALL=C awk '
  FNR == 1 { marker = FILENAME ~ /\.rs$/ ? "^[ \t]*//" : "^[ \t]*#" }
  $0 ~ marker && !/[a-z]+:\/\// {
    text = $0
    # UTF-8 continuation bytes, which belong to the character before them.
    gsub(/[\200-\277]/, "", text)
    if (length(text) > 100) {
      printf "%s:%d: comment is %d columns, over the limit of 100\n", FILENAME, FNR, length(text)
      long = 1
    }
  }
  END { exit long }
' "$@"
