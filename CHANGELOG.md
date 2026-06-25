# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0-alpha.2] - 2026-06-25

### Added

- _(slash)_ /delete and Ctrl+D in /resume picker for session deletion (#75)
- _(install)_ Nix flake, Homebrew formula, and install matrix docs (#76)
- _(slash)_ /compact context compression with summary boundary (#77)
- _(agent)_ Add automatic compaction (#82)
- Configurable tool-round cap and auto-compaction follow-ups (#83)
- Trust extra CA bundles for corp gateways (#84)
- _(tui)_ Add configurable status line (#86)
- _(tui)_ OSC 8 PR hyperlink and native terminal drag-select (#87)
- _(model)_ Add Claude Opus 4.8 and retire Opus 4.1 (#89)

### Fixed

- _(slash)_ Preserve compact boundary invariants
- _(slash)_ Preserve /compact ordering and resume state

### Dependencies

- _(deps)_ Bump idna from 3.11 to 3.15 (#88)

## [0.1.0-alpha.1] - 2026-05-09

### Added

- Scaffold project with Cargo workspace, CI, and conventions (#1)
- _(oxide-code)_ Add Anthropic streaming client and async REPL (#2)
- _(oxide-code)_ Add tool system with bash tool and agent loop (#3)
- _(tool)_ Add file and search tools with structured metadata (#4)
- _(client)_ Add streaming robustness and extended thinking support (#5)
- _(oauth)_ Read and write tokens from macOS Keychain (#6)
- _(prompt)_ Add system prompt builder with context injection (#7)
- _(billing)_ Add cch attestation for OAuth requests (#8)
- _(config)_ Add TOML config file with layered loading (#9)
- _(tui)_ Add TUI foundation with event architecture and ratatui components (#10)
- Mirror Claude Code system prompt and API client for gateway compatibility (#12)
- _(tui)_ Polish TUI with markdown rendering, tool styling, and multi-line input (#11)
- _(session)_ Add JSONL-based session persistence with resume and listing (#13)
- _(session,client)_ AI session titles, path resume, and per-model anthropic-beta gating (#19)
- _(tui)_ Per-tool result views with inline Edit diff rendering (#24)
- _(tui/markdown)_ Distinguish inline code with a surface background fill (#27)
- _(client,config)_ Proper Opus 4.7 support — effort tier, 1h prompt-cache, thinking display (#29)
- _(tui)_ Render Read results as structured excerpts (#30)
- _(tui)_ Persist thinking block across streaming with markdown body (#31)
- _(tui)_ Render grep results as structured per-file matches (#39)
- _(tui)_ Visual polish round (grep title, spacer, inline code) (#40)
- _(diff)_ Add line-number gutter and Catppuccin row-bg tint (#43)
- _(tui)_ Render glob results as structured file list (#44)
- _(glob)_ Pattern body header, parenthetical footer, structured total (#45)
- _(theme)_ User-configurable TUI palette with 5 built-ins + per-slot overrides (#46)
- _(logging)_ Route TUI tracing to $XDG_STATE_HOME/ox/log (#47)
- _(tool)_ File-change tracker with strict Read-before-Edit gate and resume-aware persistence (#52)
- _(tui, agent)_ Turn interruption with mid-turn queued follow-ups (#53)
- _(slash)_ Client-side command surface (/help, /diff, /status, /config) (#55)
- _(slash)_ Autocomplete popup overlay above the input (#56)
- _(slash)_ /clear command with session-UUID rolling (#58)
- _(slash)_ /init command with SlashOutcome trait refactor (#59)
- _(slash)_ Add model and effort runtime controls (#60)
- _(tui)_ Modal infrastructure with /model+/effort and /status pickers (#64)
- _(slash)_ /effort slider + 1M-context display fix + DRY modal cancel (#66)
- _(slash)_ /theme picker with live preview (#67)
- _(slash)_ Typed-arg autocomplete + always-picker popup + model cleanup (#68)
- _(tui)_ Rich welcome screen with identity ribbon + body column (#69)
- _(slash)_ /resume picker + mid-session in-place re-init (#70)
- _(slash)_ /rename, plus tier-contrast and cursor follow-ups (#72)

### Fixed

- _(tui/chat)_ Interleave tool call and result on resume to match live (#16)
- 1P cache-scope gating + three TUI rendering polish items (#23)
- _(tui/markdown)_ Fit tables to width budget with wrapped cells (#25)
- _(config)_ Propagate file parse errors instead of silent fallthrough (#26)
- _(agent)_ Surface tool-input JSON parse errors to the model (#38)
- _(client)_ Emit canonical metadata.user_id with minted device_id (#48)
- _(tui)_ Repaint after auto-scroll re-clamps offset on appended blocks (#57)

### Dependencies

- _(deps)_ Bump rustls-webpki and rand for Dependabot alerts (#17)
- _(deps)_ Bump rustls-webpki from 0.103.12 to 0.103.13 (#34)

[0.1.0-alpha.1]: https://github.com/hakula139/oxide-code/releases/tag/v0.1.0-alpha.1
[0.1.0-alpha.2]: https://github.com/hakula139/oxide-code/compare/v0.1.0-alpha.1..v0.1.0-alpha.2
