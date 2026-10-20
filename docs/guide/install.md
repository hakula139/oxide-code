# Installation

How to install `ox` on macOS, Linux, and Windows. For first-run setup once installed, see the [quickstart](quickstart.md).

## System Requirements

- **macOS**: 12 or later on Apple Silicon.
- **Linux**: x86_64 Linux with glibc 2.39 or later, such as Ubuntu 24.04, Debian 13, Fedora 40, or RHEL 10.
- **Windows**: 10 or later on x86_64, with [Git for Windows](https://git-scm.com/downloads/win) and `git.exe` on `PATH`. The `bash` tool runs the `bash.exe` included with Git for Windows.
- **Other platforms**: Intel Macs, ARM Linux, Linux with an older glibc, and musl-based Linux such as Alpine need a source build through [Cargo](#from-source-via-cargo) or [Nix](#nix).
- **Network**: Outbound HTTPS access to `api.anthropic.com` or your [LLM gateway](configuration.md#llm-gateways). Claude account logins also exchange and refresh tokens through `platform.claude.com`.

## Install `ox`

| Method                              | Platforms                                     | Builds from source |
| ----------------------------------- | --------------------------------------------- | ------------------ |
| [Prebuilt binary](#prebuilt-binary) | Apple Silicon macOS, x86_64 Linux and Windows | No                 |
| [Homebrew](#homebrew)               | Apple Silicon macOS, x86_64 Linux             | No                 |
| [Cargo](#from-source-via-cargo)     | Any platform Rust supports                    | Yes                |
| [Nix](#nix)                         | Linux and macOS                               | Yes                |

### Prebuilt Binary

Download the archive for your platform from [Releases](https://github.com/hakula139/oxide-code/releases), extract it, and move the `ox` binary to a directory on your `PATH`. Set `VERSION` to the newest tag listed on that page:

```bash
VERSION=v0.1.0-alpha.5

# macOS aarch64 (Apple Silicon)
curl -fsSL "https://github.com/hakula139/oxide-code/releases/download/$VERSION/oxide-code-aarch64-apple-darwin.tar.gz" | tar -xz
sudo mv ox /usr/local/bin/

# Linux x86_64
curl -fsSL "https://github.com/hakula139/oxide-code/releases/download/$VERSION/oxide-code-x86_64-unknown-linux-gnu.tar.gz" | tar -xz
sudo mv ox /usr/local/bin/

# Windows x86_64 (Git Bash / MSYS). Under WSL, use the Linux commands above.
curl -fsSLO "https://github.com/hakula139/oxide-code/releases/download/$VERSION/oxide-code-x86_64-pc-windows-msvc.zip"
unzip oxide-code-x86_64-pc-windows-msvc.zip
```

Each archive contains `ox`, `LICENSE`, and `THIRD-PARTY-NOTICES`. Each release also publishes an `oxide-code-<target>.sha256` checksum alongside the archive.

Releases after `v0.1.0-alpha.2` also include a build provenance attestation linking each archive to the release workflow run that built it. To verify an archive with the GitHub CLI, download it to disk before extracting it:

```bash
curl -fsSLO "https://github.com/hakula139/oxide-code/releases/download/$VERSION/oxide-code-aarch64-apple-darwin.tar.gz"
gh attestation verify oxide-code-aarch64-apple-darwin.tar.gz --repo hakula139/oxide-code
tar -xzf oxide-code-aarch64-apple-darwin.tar.gz
```

### Homebrew

The formula installs the prebuilt release binary on Apple Silicon macOS and x86_64 Linux:

```bash
brew tap hakula139/oxide-code https://github.com/hakula139/oxide-code
brew install oxide-code
```

### From Source via Cargo

Requires [Rust](https://www.rust-lang.org/tools/install) 1.91 or later.

```bash
cargo install --git https://github.com/hakula139/oxide-code --locked
```

This builds the `main` branch and installs `ox` to `~/.cargo/bin`. To pin a release, pass `--tag vX.Y.Z`.

### Nix

The repo includes a flake that builds `ox` from source:

```bash
nix run github:hakula139/oxide-code                     # One-shot
nix profile install github:hakula139/oxide-code         # Install to user profile
nix profile install github:hakula139/oxide-code/vX.Y.Z  # Install a specific release
```

Or as a flake input from another project:

```nix
inputs.oxide-code.url = "github:hakula139/oxide-code";
# Outputs: packages.${system}.{default, oxide-code}
```

A ref without a tag builds the `main` branch. Tags from `v0.1.0-alpha.2` onward include the flake, so adding a `/vX.Y.Z` suffix to the flake input pins a release.

## Verify Your Installation

```bash
ox --version
```

This prints the version string. If you see `command not found`, the binary is not in a directory on your `PATH`. Recheck the move or install step for your method.

## Update `ox`

| Install method               | Update command                                                                                        |
| ---------------------------- | ----------------------------------------------------------------------------------------------------- |
| Prebuilt binary              | Re-run the download commands with `VERSION` set to the new tag                                        |
| Homebrew                     | `brew update && brew upgrade oxide-code`                                                              |
| Cargo, from `main`           | `cargo install --git https://github.com/hakula139/oxide-code --locked --force`                        |
| Cargo, pinned to a tag       | The same command with `--tag` set to the new tag                                                      |
| Nix profile, from `main`     | `nix profile upgrade oxide-code`                                                                      |
| Nix profile, pinned to a tag | `nix profile remove oxide-code`, then install the new tag's ref                                       |
| Nix flake input              | `nix flake update oxide-code`. For an input pinned to a tag, change the `/vX.Y.Z` in its URL instead. |

## Uninstall `ox`

| Install method  | Uninstall command                                              |
| --------------- | -------------------------------------------------------------- |
| Prebuilt binary | `sudo rm /usr/local/bin/ox` (or wherever you moved it)         |
| Homebrew        | `brew uninstall oxide-code && brew untap hakula139/oxide-code` |
| Cargo           | `cargo uninstall oxide-code`                                   |
| Nix profile     | `nix profile remove oxide-code`                                |

To also remove session data and config:

```bash
rm -rf ${XDG_DATA_HOME:-$HOME/.local/share}/ox        # Sessions, device ID, and the oxide-code login
rm -rf ${XDG_STATE_HOME:-$HOME/.local/state}/ox       # Log files
rm -rf ${XDG_CONFIG_HOME:-$HOME/.config}/ox           # User config (if you wrote one)
```
