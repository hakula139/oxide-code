# Installation

How to install `ox` on macOS, Linux, and Windows. For first-run setup once installed, see the [quickstart](quickstart.md).

## System requirements

- **OS**: macOS 12+, Linux with glibc 2.39 or later (such as Ubuntu 24.04, Debian 13, Fedora 40, or RHEL 10), or Windows 10+. Older glibc and musl-based Linux, such as Alpine, need a source build.
- **Architecture**: prebuilt binaries cover `aarch64` macOS (Apple Silicon) and `x86_64` Linux and Windows. Other platforms, including Intel Macs, need a source build.
- **Network**: outbound HTTPS to `api.anthropic.com`
- **Windows only**: [Git for Windows](https://git-scm.com/downloads/win) with `git.exe` on `PATH`, since the bash tool runs the `bash.exe` it ships
- **From-source builds only**: [Rust](https://www.rust-lang.org/tools/install) 1.91+

## Install `ox`

Pick whichever path matches how you usually install developer tools.

### Prebuilt binary

Download the archive for your platform from [Releases](https://github.com/hakula139/oxide-code/releases), extract it, and move the `ox` binary onto your `PATH`. The commands below fetch the release named by `VERSION`, so set it to the newest tag on that page:

```bash
VERSION=v0.1.0-alpha.2

# macOS aarch64 (Apple Silicon)
curl -fsSL "https://github.com/hakula139/oxide-code/releases/download/$VERSION/oxide-code-aarch64-apple-darwin.tar.gz" | tar -xz
sudo mv ox /usr/local/bin/

# Linux x86_64
curl -fsSL "https://github.com/hakula139/oxide-code/releases/download/$VERSION/oxide-code-x86_64-unknown-linux-gnu.tar.gz" | tar -xz
sudo mv ox /usr/local/bin/

# Windows x86_64 (Git Bash / MSYS / WSL)
curl -fsSLO "https://github.com/hakula139/oxide-code/releases/download/$VERSION/oxide-code-x86_64-pc-windows-msvc.zip"
unzip oxide-code-x86_64-pc-windows-msvc.zip
```

Each archive includes `LICENSE` and `THIRD-PARTY-NOTICES` alongside the `ox` executable, and each release publishes an `oxide-code-<target>.sha256` checksum beside it. Releases after `v0.1.0-alpha.2` also carry a build provenance attestation, which ties each archive to the release workflow run that built it. To verify one with the GitHub CLI before extracting it, download the archive to disk first:

```bash
curl -fsSLO "https://github.com/hakula139/oxide-code/releases/download/$VERSION/oxide-code-aarch64-apple-darwin.tar.gz"
gh attestation verify oxide-code-aarch64-apple-darwin.tar.gz --repo hakula139/oxide-code
tar -xzf oxide-code-aarch64-apple-darwin.tar.gz
```

### Homebrew (Apple Silicon macOS, x86_64 Linux)

Tap this repo, then install:

```bash
brew tap hakula139/oxide-code https://github.com/hakula139/oxide-code
brew install oxide-code
```

The formula installs the prebuilt release binary, so it needs no Rust toolchain.

### From source via cargo

If you already have a Rust toolchain set up:

```bash
cargo install --git https://github.com/hakula139/oxide-code --locked
```

This compiles `oxide-code` from the `main` branch and installs `ox` to `~/.cargo/bin`. Pass `--tag vX.Y.Z` to pin a specific release.

### Nix

The repo ships a flake that source-builds `ox`:

```bash
nix run github:hakula139/oxide-code                     # one-shot
nix profile install github:hakula139/oxide-code         # install to user profile
nix profile install github:hakula139/oxide-code/vX.Y.Z  # install a specific release
```

Or as a flake input from another project:

```nix
inputs.oxide-code.url = "github:hakula139/oxide-code";
# Outputs: packages.${system}.{default, oxide-code}
```

Without a tag, these refs build the `main` branch. Tags from `v0.1.0-alpha.2` on ship the flake, and a tag suffix pins the flake input the same way.

## Verify your installation

```bash
ox --version
```

If `ox` is on `PATH`, this prints the version string. A `command not found` means the binary didn't land somewhere your shell searches, so re-check the move or install step for the path you used.

## Update `ox`

| Install method  | Update command                                                                 |
| --------------- | ------------------------------------------------------------------------------ |
| Prebuilt binary | Re-run the download block from above with `VERSION` set to the new tag         |
| Homebrew        | `brew update && brew upgrade oxide-code`                                       |
| Cargo           | `cargo install --git https://github.com/hakula139/oxide-code --locked --force` |
| Nix profile     | `nix profile upgrade oxide-code` (or refresh the flake input in your project)  |

The Cargo and Nix commands follow `main`. An install pinned to a tag stays on that tag, so update it by installing the new tag: pass `--tag vX.Y.Z` to `cargo install --force`, or run `nix profile remove oxide-code` and then install the new tag's ref.

## Uninstall `ox`

| Install method  | Uninstall command                                              |
| --------------- | -------------------------------------------------------------- |
| Prebuilt binary | `sudo rm /usr/local/bin/ox` (or wherever you moved it)         |
| Homebrew        | `brew uninstall oxide-code && brew untap hakula139/oxide-code` |
| Cargo           | `cargo uninstall oxide-code`                                   |
| Nix profile     | `nix profile remove oxide-code`                                |

To also remove session data and config:

```bash
rm -rf ${XDG_DATA_HOME:-$HOME/.local/share}/ox        # sessions, device ID, and the oxide-code login
rm -rf ${XDG_STATE_HOME:-$HOME/.local/state}/ox       # log files
rm -rf ${XDG_CONFIG_HOME:-$HOME/.config}/ox           # user config (if you wrote one)
```
