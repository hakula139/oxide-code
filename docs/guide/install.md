# Installation

Install `ox` on macOS, Linux, and Windows. See the [quickstart](quickstart.md) for first-run setup.

## System Requirements

- **macOS**: 12 or later on Apple Silicon.
- **Linux**: x86_64 Linux with glibc 2.39 or later, such as Ubuntu 24.04, Debian 13, Fedora 40, or RHEL 10.
- **Windows**: 10 or later on x86_64 with [Git for Windows](https://git-scm.com/downloads/win) and `git.exe` on `PATH` (the `bash` tool uses its `bash.exe`).
- **Other platforms**: ARM Linux, older glibc, and musl Linux (such as Alpine) require a source build via [Cargo](#from-source-via-cargo) or [Nix](#nix). Intel Macs require Cargo (unsupported by the flake).
- **Network**: Outbound HTTPS to `api.anthropic.com` or your [LLM gateway](authentication.md#llm-gateways). Claude account logins also contact `platform.claude.com`.

## Install `ox`

| Method                              | Platforms                                     | Builds from source |
| ----------------------------------- | --------------------------------------------- | ------------------ |
| [Homebrew](#homebrew)               | Apple Silicon macOS, x86_64 Linux             | No                 |
| [Prebuilt binary](#prebuilt-binary) | Apple Silicon macOS, x86_64 Linux and Windows | No                 |
| [Cargo](#from-source-via-cargo)     | Any platform Rust supports                    | Yes                |
| [Nix](#nix)                         | Linux and Apple Silicon macOS                 | Yes                |

### Homebrew

Installs the prebuilt release binary on Apple Silicon macOS and x86_64 Linux:

```bash
brew install hakulab/tap/oxide-code
```

### Prebuilt Binary

Download the archive for your platform from [Releases](https://github.com/hakulab/oxide-code/releases), extract it, and place `ox` on `PATH` (set `VERSION` to the latest tag):

```bash
VERSION=v0.1.0-alpha.13

# macOS aarch64 (Apple Silicon)
curl -fsSL "https://github.com/hakulab/oxide-code/releases/download/$VERSION/oxide-code-aarch64-apple-darwin.tar.gz" | tar -xz
sudo mv ox /usr/local/bin/

# Linux x86_64
curl -fsSL "https://github.com/hakulab/oxide-code/releases/download/$VERSION/oxide-code-x86_64-unknown-linux-gnu.tar.gz" | tar -xz
sudo mv ox /usr/local/bin/

# Windows x86_64 (Git Bash / MSYS). Under WSL, use the Linux commands above.
curl -fsSLO "https://github.com/hakulab/oxide-code/releases/download/$VERSION/oxide-code-x86_64-pc-windows-msvc.zip"
unzip oxide-code-x86_64-pc-windows-msvc.zip
mkdir -p ~/bin
mv ox.exe ~/bin/
```

Each archive contains `ox`, `LICENSE`, and `THIRD-PARTY-NOTICES`. Each release also publishes an `oxide-code-<target>.sha256` checksum and a build provenance attestation alongside the archive. To verify an archive with the GitHub CLI before extracting:

```bash
curl -fsSLO "https://github.com/hakulab/oxide-code/releases/download/$VERSION/oxide-code-aarch64-apple-darwin.tar.gz"
gh attestation verify oxide-code-aarch64-apple-darwin.tar.gz --repo hakulab/oxide-code
tar -xzf oxide-code-aarch64-apple-darwin.tar.gz
```

### From Source via Cargo

Requires [Rust](https://www.rust-lang.org/tools/install) 1.91 or later.

```bash
cargo install --git https://github.com/hakulab/oxide-code --locked
```

Builds `main` into `~/.cargo/bin`. Pass `--tag vX.Y.Z` to pin a release.

### Nix

The flake's packages are cached on the `hakula` Cachix cache and build from source when unavailable:

```bash
nix run github:hakulab/oxide-code                     # One-shot
nix profile install github:hakulab/oxide-code         # Install to user profile
nix profile install github:hakulab/oxide-code/vX.Y.Z  # Install a specific release
```

Or as a flake input:

```nix
inputs.oxide-code.url = "github:hakulab/oxide-code";
# Outputs: packages.${system}.{default, oxide-code}, overlays.default, homeModules.default
```

Without a tag suffix, Nix builds `main`. Append `/vX.Y.Z` to pin any tag from `v0.1.0-alpha.2` onward.

### Home Manager

The Home Manager module installs `ox` and generates its config from Nix options. Keep oxide-code's `nixpkgs` input pinned upstream to reuse cached binaries, and keep only the `follows` inputs you use:

```nix
inputs.oxide-code = {
  url = "github:hakulab/oxide-code";
  inputs = {
    devkit.inputs.git-hooks-nix.follows = "git-hooks-nix";
    home-manager.follows = "home-manager";
    rust-overlay.follows = "rust-overlay";
  };
};
```

Import the module in Home Manager:

```nix
imports = [ inputs.oxide-code.homeModules.default ];

programs.oxide-code = {
  enable = true;
  settings.tui.show_thinking = true;
  context = ./AGENTS.md;
};
```

| Option        | Default                       | Purpose                                     |
| ------------- | ----------------------------- | ------------------------------------------- |
| `enable`      | `false`                       | Install and configure `ox`                  |
| `package`     | oxide-code from this flake    | Install `ox`, or set `null` for config only |
| `checkConfig` | When the builder can run `ox` | Validate `config.toml` at build time        |
| `settings`    | `{ }`                         | Write nonempty settings to `ox/config.toml` |
| `context`     | `""`                          | Set user instructions in `ox/AGENTS.md`     |

`checkConfig` defaults to `true` when `package != null` and the build platform can run host binaries. A `context` string writes inline text, while a Nix path or package links as `ox/AGENTS.md`.

- **Files**: Paths are relative to `$XDG_CONFIG_HOME` (default `~/.config`). Put local overrides in `ox/config.d/` so they take precedence over the module-managed `config.toml`.

- **Wrapping the package**: The default `package` comes directly from the flake and ignores overlays on `pkgs.oxide-code`. Apply `inputs.oxide-code.overlays.default` and pass your wrapper around `pkgs.oxide-code` to `package`.

- **`checkConfig`**: Runs [`ox config check`](configuration.md#checking-the-config) on the generated `config.toml` so typos or invalid values fail `home-manager switch`. It runs as a Nix build user with an empty scratch home and no credentials (inside a sandbox where Nix enables one, unlike macOS by default), so wrappers must run without your home or secrets. Because `~` resolves to the scratch home, pass theme files as Nix paths (`tui.theme.base = ./theme.toml`) and extra roots under home with `~/`, or set `checkConfig = false` if a runtime-only path is rejected.

- **Credentials**: Generated settings enter the Nix store, so keep secrets in a separate file (never opened by `checkConfig`). Set `client.auth.file` to a quoted runtime path with its `type` ([Fixed Credentials](authentication.md#fixed-credentials)):

  ```nix
  settings.client.auth = {
    type = "bearer";
    file = "/run/secrets/ox-token";
  };
  ```

## Verify Your Installation

```bash
ox --version
```

## Update `ox`

| Install method               | Update command                                                                                        |
| ---------------------------- | ----------------------------------------------------------------------------------------------------- |
| Prebuilt binary              | Re-run the download commands with `VERSION` set to the new tag                                        |
| Homebrew                     | `brew upgrade hakulab/tap/oxide-code`                                                                 |
| Cargo, from `main`           | `cargo install --git https://github.com/hakulab/oxide-code --locked --force`                          |
| Cargo, pinned to a tag       | The same command with `--tag` set to the new tag                                                      |
| Nix profile, from `main`     | `nix profile upgrade oxide-code`                                                                      |
| Nix profile, pinned to a tag | `nix profile remove oxide-code`, then install the new tag's ref                                       |
| Nix flake input              | `nix flake update oxide-code`. For an input pinned to a tag, change the `/vX.Y.Z` in its URL instead. |

## Uninstall `ox`

| Install method  | Uninstall command                                                                                  |
| --------------- | -------------------------------------------------------------------------------------------------- |
| Prebuilt binary | `sudo rm /usr/local/bin/ox` (or wherever you moved it)                                             |
| Homebrew        | `brew uninstall oxide-code`                                                                        |
| Cargo           | `cargo uninstall ox`, or `cargo uninstall oxide-code` if `cargo install --list` lists `oxide-code` |
| Nix profile     | `nix profile remove oxide-code`                                                                    |
| Home Manager    | Remove the module import or set `programs.oxide-code.enable = false`, then switch                  |

To also remove session data and config:

```bash
rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/ox"      # Sessions, device ID, and the oxide-code login
rm -rf "${XDG_STATE_HOME:-$HOME/.local/state}/ox"     # Log files
rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/ox"         # User config (if you wrote one)
```
