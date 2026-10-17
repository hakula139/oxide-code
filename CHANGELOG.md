# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0-alpha.3] - 2026-10-17

### Highlights

- Claude 5.5 only: the catalogue drops older models, and thinking blocks replay across model swaps.
- A tiered permission system with an approval gate in every frontend, session-wide approvals, and shell commands parsed into segments before rules match.
- Gateway and OAuth token authentication with custom headers, plus /login and /logout.
- Layered config through config.d drop-ins and .ox/config.toml, with per-value provenance and startup errors that name the setting and the fix.
- Headless runs print a JSON result or an event stream.
- Sessions can be archived, resumed from the shell with ox --resume, and keep their cost estimate.
- Compaction streams its summary, handles tool-heavy sessions, and holds the prompt when auto-compaction fails.
- A cancelled prompt is marked as interrupted, so the next reply answers only the new prompt.
- A guided /init and opt-in status line segments for usage limits and PR state.
- The TUI and the bash tool work on Windows, and release archives carry attestations that `gh attestation verify` checks.

### Breaking changes

- _(model)_ Support only Claude Opus 5.5, Sonnet 5.5, and Haiku 5.5, with Opus 5.5 as the default. Other model IDs still pass through, without thinking, effort, or a cost estimate ([#183](https://github.com/hakula139/oxide-code/pull/183))
- _(install)_ Stop publishing an Intel Mac archive, so Intel Macs now build from source or install through Nix. Every release archive now has a build provenance attestation that `gh attestation verify` checks ([#210](https://github.com/hakula139/oxide-code/pull/210))

### Added

- _(permission)_ Check every tool call before it runs. Read-only calls and edits within the working directory run without asking. Built-in deny rules block dangerous commands, edits to protected paths such as `.git/hooks` always require approval, and any call not settled by a rule prompts for approval. `read` deny rules also withhold matching files from `grep` and `glob` results. `plan` mode denies edits and `bash`, while `yolo` never asks ([#95](https://github.com/hakula139/oxide-code/pull/95), [#113](https://github.com/hakula139/oxide-code/pull/113), [#227](https://github.com/hakula139/oxide-code/pull/227))
- _(permission)_ Respond to approval prompts in the TUI and REPL by allowing a call once or for the rest of the session, denying it, or denying it with feedback for the model. Headless runs deny calls with feedback naming the allow rule to add ([#233](https://github.com/hakula139/oxide-code/pull/233), [#243](https://github.com/hakula139/oxide-code/pull/243))
- _(permission)_ Judge a `bash` command by each simple command it runs and each file it writes. A chained or piped command runs when every part matches an allow rule, and a deny rule matches any part, including a quoted or full-path command name such as `/bin/rm` ([#250](https://github.com/hakula139/oxide-code/pull/250))
- _(permission)_ On Windows, the gate compares 8.3 short names as their long names, strips NTFS stream suffixes, and matches path globs across either separator. A file with more than one hard link counts as outside the workspace, so edits to it ask, and file tools refuse a symlink swapped into the final path component after the check ([#211](https://github.com/hakula139/oxide-code/pull/211), [#223](https://github.com/hakula139/oxide-code/pull/223), [#224](https://github.com/hakula139/oxide-code/pull/224), [#225](https://github.com/hakula139/oxide-code/pull/225))
- _(config)_ Authenticate using `ANTHROPIC_AUTH_TOKEN`, an API key, or a long-lived `CLAUDE_CODE_OAUTH_TOKEN`, following Claude Code's order, and send gateway headers from `ANTHROPIC_CUSTOM_HEADERS`. A stored Claude Code login is never sent to a custom base URL, so a gateway setup that relied on it now fails at startup and lists the credentials that work there ([#197](https://github.com/hakula139/oxide-code/pull/197))
- _(slash)_ Sign in to a Claude account with `/login`, independently of Claude Code's login, and remove that login with `/logout` ([#242](https://github.com/hakula139/oxide-code/pull/242))
- _(config)_ Layer config from `config.d/*.toml` drop-ins and a project `.ox/config.toml`, and show in `/config` which file set each value. A legacy `ox.toml` still loads with a notice to move it, and inline theme-override tables for the same slot now merge across layers ([#229](https://github.com/hakula139/oxide-code/pull/229))
- _(config)_ Name the setting and the fix in startup errors. A config file's `max_tokens = 0` or `max_tool_rounds = 0` now fails to load, as its environment variable already did ([#235](https://github.com/hakula139/oxide-code/pull/235))
- _(driver)_ Print headless runs as a single JSON result with `--output json` or as an event stream with `--output stream-json`. A refusal now fails the turn, so headless and REPL runs exit with failure ([#236](https://github.com/hakula139/oxide-code/pull/236))
- _(session)_ Archive a session with `/archive` to hide it from `ox --list`, the `/resume` picker, and `ox -c` while keeping it resumable. Restore it with `/unarchive` ([#216](https://github.com/hakula139/oxide-code/pull/216))
- _(session)_ Open the session picker from the shell with `ox --resume` ([#218](https://github.com/hakula139/oxide-code/pull/218))
- _(session)_ Restore the session cost estimate when resuming ([#226](https://github.com/hakula139/oxide-code/pull/226))
- _(compaction)_ Stream the summary into the chat as it is generated ([#252](https://github.com/hakula139/oxide-code/pull/252))
- _(tui)_ Add opt-in status line segments for usage limits, PR checks and review, queued prompts, the session ID, and the theme ([#259](https://github.com/hakula139/oxide-code/pull/259))
- _(slash)_ Have `/init` ask which instruction file to create and propose a diff before changing an existing one ([#263](https://github.com/hakula139/oxide-code/pull/263))

### Fixed

- _(session)_ Keep transcripts intact if ox crashes mid-write or a write fails, so resuming no longer drops later history ([#179](https://github.com/hakula139/oxide-code/pull/179))
- _(agent)_ Retry transient API failures with backoff, and end replies cut off by the output limit or a dropped stream without running unfinished tool calls ([#194](https://github.com/hakula139/oxide-code/pull/194))
- _(agent)_ Keep results from tool calls that finished before cancellation, and stop a cancelled `bash` command along with its background children ([#195](https://github.com/hakula139/oxide-code/pull/195))
- _(oauth)_ Save refreshed login tokens to the Keychain entry or file they came from, so token refresh no longer fails on macOS with "no credentials available" ([#103](https://github.com/hakula139/oxide-code/pull/103))
- _(compaction)_ Compact sessions made mostly of tool output, and hold the prompt when auto-compaction fails instead of sending it against the oversized transcript ([#265](https://github.com/hakula139/oxide-code/pull/265))
- _(compaction)_ Summarize the full conversation through the prompt cache, fail a cut-off summary without replacing the transcript, and count summary requests toward the session cost ([#196](https://github.com/hakula139/oxide-code/pull/196))
- _(engine)_ Mark a cancelled prompt as interrupted, so the next reply answers only the new prompt ([#260](https://github.com/hakula139/oxide-code/pull/260))
- _(oxide-code)_ On Windows, start the TUI, run `bash` through Git for Windows, and end the whole process tree on timeout or cancellation ([#211](https://github.com/hakula139/oxide-code/pull/211))
- _(tool)_ Keep the head and tail of long `bash` output and partial output from timed-out commands. Let `glob` and `grep` search dotfiles and return paths that a later read can open ([#114](https://github.com/hakula139/oxide-code/pull/114))
- _(main)_ Save the live session's file-tracking state on exit after `/clear` or `/resume`, and keep the REPL running after a failed turn ([#108](https://github.com/hakula139/oxide-code/pull/108))
- _(util)_ Refresh the status line's branch and PR in the background so they no longer block input, and read boolean environment variables in any letter case, failing on an unknown value ([#104](https://github.com/hakula139/oxide-code/pull/104))
- _(tui)_ Render fenced code, diffs, and HTML blocks faithfully, and wrap CJK text between characters ([#112](https://github.com/hakula139/oxide-code/pull/112))
- _(tui)_ Scroll an idle chat with PageUp, PageDown, Ctrl+Home, and Ctrl+End, and paste multi-line text as a single input ([#115](https://github.com/hakula139/oxide-code/pull/115))
- _(tui)_ Emit status-line hyperlinks that tmux 3.4 and later forward without `allow-passthrough`, and keep the terminal intact if a background task panics ([#116](https://github.com/hakula139/oxide-code/pull/116))

### Changed

- _(client)_ Cache the whole conversation and keep each session's request prefix fixed across resumes and model swaps. Send instruction, date, and model changes as reminders so later turns can reuse the prompt cache and earlier thinking ([#107](https://github.com/hakula139/oxide-code/pull/107), [#108](https://github.com/hakula139/oxide-code/pull/108), [#189](https://github.com/hakula139/oxide-code/pull/189), [#213](https://github.com/hakula139/oxide-code/pull/213))
- _(tui)_ Keep long chats responsive while streaming, scrolling, resizing, and changing themes by caching rendered blocks and rendering only those near the viewport ([#111](https://github.com/hakula139/oxide-code/pull/111), [#244](https://github.com/hakula139/oxide-code/pull/244))
- _(session)_ Find the session targeted by `ox -c`, `/resume`, and `/delete` without parsing every transcript ([#182](https://github.com/hakula139/oxide-code/pull/182))

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
