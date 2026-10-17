# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0-alpha.3] - 2026-10-17

### Breaking changes

- _(model)_ Support only the Claude 5.5 models (#183)
- _(install)_ Gate releases on CI, attest archives, and drop the Intel Mac build (#210)

### Added

- _(permission)_ Tiered tool-permission system with approval gate (#95)
- _(prompt)_ Send the model identity in the session context (#189)
- _(config)_ Authenticate with gateway tokens, OAuth tokens, and custom headers (#197)
- _(session)_ Archive and unarchive sessions (#216)
- _(session)_ Open the session picker from the shell with ox --resume (#218)
- _(session)_ Restore the session cost estimate on resume (#226)
- _(config)_ Add config.d drop-ins, .ox/config.toml, and per-value provenance (#229)
- _(config)_ Name the setting and the fix in startup errors (#235)
- _(driver)_ Print headless runs as a JSON result or event stream (#236)
- _(permission)_ Remember approvals for the rest of the session (#233)
- _(permission)_ Answer approvals in every frontend (#243)
- _(slash)_ Add /login and /logout (#242)
- _(compaction)_ Stream the summary into the chat as it generates (#252)
- _(permission)_ Parse shell commands for permission analysis (#250)
- _(tui)_ Add opt-in status line segments for limits, PR state, and more (#259)
- _(slash)_ Ask which instruction file /init creates, and diff updates (#263)

### Fixed

- _(deps)_ Refresh dependencies and patch DoS advisories (#94)
- _(oauth)_ Write refreshed tokens back to their source (#103)
- _(util)_ Run git probes off the event loop and harden env and log handling (#104)
- _(main)_ Finalize the live session, share one turn driver, and freeze the prompt context (#108)
- _(tui)_ Render code, diffs, HTML, and CJK wrapping faithfully (#112)
- _(permission)_ Close symlink, protected-path, and glob bypasses (#113)
- _(tool)_ Align tool semantics with what the model sees (#114)
- _(tui)_ Route scroll keys to the chat and handle bracketed paste (#115)
- _(tui)_ Emit OSC 8 natively in tmux and scope the panic hook (#116)
- _(session)_ Keep transcripts intact across crashes and failed flushes (#179)
- _(agent)_ Handle stop reasons, retries, and truncated streams (#194)
- _(agent)_ Keep completed tool results and kill process groups on cancel (#195)
- _(compaction)_ Summarize the full transcript and count its usage (#196)
- _(session)_ Replay the resumed branch's prompt context (#213)
- _(oxide-code)_ Run the test suite on Windows and fix what it found (#211)
- _(permission)_ Compare Windows 8.3 short names as their long names (#223)
- _(permission)_ Keep hard-linked files out of the Windows workspace zone (#224)
- _(tool)_ Withhold read-denied files from grep and glob results (#227)
- _(tool)_ Refuse a symlink swapped into the final component on Windows (#225)
- _(nix)_ Keep the pre-commit hooks path absolute so linked worktrees run the hooks (#234)
- _(engine)_ Tell the model when the user interrupted the previous prompt (#260)
- _(compaction)_ Compact tool-heavy sessions and hold the prompt when auto-compaction fails (#265)

### Changed

- _(client)_ Cache the conversation and keep the request prefix stable (#107)
- _(tui)_ Cache rendered chat blocks and paint only the visible slice (#111)
- _(session)_ Resolve resume targets without reading every transcript (#182)
- _(tui)_ Render only chat blocks near the viewport (#244)

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

[0.1.0-alpha.3]: https://github.com/hakula139/oxide-code/compare/v0.1.0-alpha.2..v0.1.0-alpha.3
[0.1.0-alpha.2]: https://github.com/hakula139/oxide-code/compare/v0.1.0-alpha.1..v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/hakula139/oxide-code/releases/tag/v0.1.0-alpha.1
