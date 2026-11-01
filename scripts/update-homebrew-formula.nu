#!/usr/bin/env nu

const SCRIPT_DIR = path self .

# Regenerate the Homebrew formula from a release's checksum sidecars.
def main [
  tag: string # Release tag, including the v prefix
  sidecars: string # Directory containing the release's .sha256 files
] {
  let version = $tag | str replace --regex '^v' ''
  let base = $"https://github.com/hakula139/oxide-code/releases/download/($tag)"
  let mac_arm_sha = open --raw ($sidecars | path join 'oxide-code-aarch64-apple-darwin.sha256')
    | split words
    | first
  let linux_x86_sha = open --raw ($sidecars | path join 'oxide-code-x86_64-unknown-linux-gnu.sha256')
    | split words
    | first

  open --raw ($SCRIPT_DIR | path join 'oxide-code-formula.rb')
    | str replace --all '{{base}}' $base
    | str replace --all '{{version}}' $version
    | str replace --all '{{linux_x86_sha}}' $linux_x86_sha
    | str replace --all '{{mac_arm_sha}}' $mac_arm_sha
    | save --force Formula/oxide-code.rb

  print $"Updated Formula/oxide-code.rb to ($tag)"
}
