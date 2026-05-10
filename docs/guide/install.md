# Installation

How to install `ox` on macOS, Linux, and Windows. For first-run setup once installed, see the [quickstart](quickstart.md).

## System requirements

- **OS**: macOS 12+, a recent glibc-based Linux, or Windows 10+ (Alpine / musl-based Linux needs the source-build path)
- **Architecture**: `x86_64` everywhere, plus `aarch64` on macOS (Apple Silicon)
- **Network**: outbound HTTPS to `api.anthropic.com`
- **From-source builds only**: [Rust](https://www.rust-lang.org/tools/install) 1.91+

## Install `ox`

Pick whichever path matches how you usually install developer tools.

### Prebuilt binary

Download the latest release archive from [Releases](https://github.com/hakula139/oxide-code/releases/latest), extract, and move the `ox` binary onto your `PATH`:

```bash
# macOS aarch64 (Apple Silicon)
curl -fsSL https://github.com/hakula139/oxide-code/releases/latest/download/oxide-code-aarch64-apple-darwin.tar.gz | tar -xz
sudo mv ox /usr/local/bin/

# macOS x86_64 (Intel)
curl -fsSL https://github.com/hakula139/oxide-code/releases/latest/download/oxide-code-x86_64-apple-darwin.tar.gz | tar -xz
sudo mv ox /usr/local/bin/

# Linux x86_64
curl -fsSL https://github.com/hakula139/oxide-code/releases/latest/download/oxide-code-x86_64-unknown-linux-gnu.tar.gz | tar -xz
sudo mv ox /usr/local/bin/

# Windows x86_64 (Git Bash / MSYS / WSL)
curl -fsSLO https://github.com/hakula139/oxide-code/releases/latest/download/oxide-code-x86_64-pc-windows-msvc.zip
unzip oxide-code-x86_64-pc-windows-msvc.zip
```

Each archive ships a single `ox` executable. Each release also publishes a `<archive>.sha256` sidecar you can verify against.

### Homebrew (macOS, Linux)

Tap this repo, then install:

```bash
brew tap hakula139/oxide-code https://github.com/hakula139/oxide-code
brew install oxide-code
```

The formula pulls the prebuilt tarball published by `.github/workflows/release.yml`, so the install is binary-only and needs no Rust toolchain.

### From source via cargo

If you already have a Rust toolchain set up:

```bash
cargo install --git https://github.com/hakula139/oxide-code --locked
```

This compiles `oxide-code` from the `main` branch and installs `ox` to `~/.cargo/bin`. Pass `--tag vX.Y.Z` to pin a specific release.

### Nix

The repo ships a flake that source-builds `ox`:

```bash
nix run github:hakula139/oxide-code              # one-shot
nix profile install github:hakula139/oxide-code  # install to user profile
```

Or as a flake input from another project:

```nix
inputs.oxide-code.url = "github:hakula139/oxide-code";
# Outputs: packages.${system}.{default, oxide-code}
```

## Verify your installation

```bash
ox --version
```

If `ox` is on `PATH`, this prints the version string. A `command not found` means the binary didn't land somewhere your shell searches, so re-check the move or install step for the path you used.

## Update `ox`

| Install method  | Update command                                                                             |
| --------------- | ------------------------------------------------------------------------------------------ |
| Prebuilt binary | Re-run the `curl ... \| tar -xz` + `sudo mv` block from above against `/releases/latest/`. |
| Homebrew        | `brew update && brew upgrade oxide-code`                                                   |
| Cargo           | `cargo install --git https://github.com/hakula139/oxide-code --locked --force`             |
| Nix profile     | `nix profile upgrade oxide-code` (or refresh the flake input in your project)              |

## Uninstall `ox`

| Install method  | Uninstall command                                              |
| --------------- | -------------------------------------------------------------- |
| Prebuilt binary | `sudo rm /usr/local/bin/ox` (or wherever you moved it)         |
| Homebrew        | `brew uninstall oxide-code && brew untap hakula139/oxide-code` |
| Cargo           | `cargo uninstall oxide-code`                                   |
| Nix profile     | `nix profile remove oxide-code`                                |

To also remove session data and config:

```bash
rm -rf ${XDG_DATA_HOME:-$HOME/.local/share}/ox        # session JSONLs
rm -rf ${XDG_STATE_HOME:-$HOME/.local/state}/ox       # log file
rm -rf ${XDG_CONFIG_HOME:-$HOME/.config}/ox           # user config (if you wrote one)
```
