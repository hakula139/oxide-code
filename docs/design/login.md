# Login and Logout

`/login` signs oxide-code in to a Claude account with its own stored credentials, and `/logout` removes them.

## Sign-In Flow

1. `/login` sends `Op::Login`. The engine binds a loopback listener on an ephemeral port and emits `Event::LoginStarted` with two authorization URLs sharing one PKCE verifier and `state`: a loopback URL the TUI opens in a browser, and a fallback URL redirecting to `https://platform.claude.com/oauth/code/callback` (displaying `code#state`) printed when a browser cannot open or runs on another machine.
2. During sign-in, input shaped like `code#state` submits as `Op::LoginCode` without echo (wrapped in `SignInCode` whose `Debug` redacts it), other text stays in the composer with a note, and slash commands still run.
3. The engine accepts whichever arrives first with a matching `state`: `GET http://localhost:<port>/callback` or a pasted code. Incomplete codes or mismatched `state` emit `Event::Error` with recovery instructions while sign-in keeps waiting.
4. The engine exchanges the code and verifier using the matching redirect URI, writes `$XDG_DATA_HOME/ox/credentials.json`, and emits `Event::SignedIn` with the active credential source.

`/logout` sends `Op::Logout`, deleting that file and falling back to Claude Code's stored login if the session used the oxide-code login or was signed out. `Event::SignedOut` reports whether a file was removed, the resulting source, and any reason a Claude Code login could not take over. Both commands finish with the resulting `/status` label.

## Stored Logins

Resolution follows [Authentication](../guide/authentication.md). `client.auth` selects a fixed bearer, API-key, or OAuth credential (inline or from a file read once at startup), and `type = "login"` clears lower-layer file credentials to re-enable stored logins.

Both stored logins share one refresh path with separate files and locks. The Claude Code store uses the same lock directory as Claude Code's `proper-lockfile` beside its resolved config directory (such as `~/.claude.lock`):

- The holder updates mtime every 5 s, and a lock idle for 10 s counts as abandoned.
- A holder whose lock another process took over (such as after suspension) stops refreshing and leaves it in place.
- A waiter whose token is expired after the 20 s lock wait fails with a transient error naming the lock so requests retry with backoff without prompting re-login, and a session signed out by that failure reloads on its next prompt.

## Refresh

Sessions load stored logins at startup or sign-in and cache the access token and expiry in memory. Any request within 5 minutes of expiry reloads from the store, refreshing and writing back unless another process already did. All credential clones share the token so concurrent requests (such as title generation) share one refresh.

On HTTP 401 for a stored login, the session reloads and retries once (using a token another process wrote, or forcing a refresh regardless of expiry if the store still holds the rejected token). If refresh fails, no refresh token exists, or the retry returns 401, the error gives the sign-in step. Fixed credentials from env vars or `client.auth` fail immediately on 401.

Transient token-endpoint failures (no response or 5xx) during a needed refresh retry the request with backoff, while permanent failures return the sign-in step.

Reloads run on a detached task holding the token lock until completion so mid-refresh cancellation (such as Esc) still persists rotated tokens (the refresh already invalidated the old refresh token, so losing the rotated tokens would force re-login). Shutdown awaits all in-flight reloads, including logins replaced by `/login` (bounded by the 20 s lock wait, 15 s token request, and any Keychain prompt).

If write-back fails, the session warns and keeps the rotated access and refresh tokens in memory. Subsequent reloads compare the store's refresh token: while it still matches the replaced token, the session reuses its retained tokens and retries write-back on the next refresh (another process refreshing meanwhile hits the invalidated token), and once the store's refresh token changes, the session adopts it.

## Design Decisions

1. **oxide-code keeps its own credential store.** Claude Code stores logins in macOS Keychain (`Claude Code-credentials`, suffixed with `-` plus the first 8 hex digits of SHA-256 of `CLAUDE_CONFIG_DIR` when set) or `.credentials.json` under `~/.claude` / `CLAUDE_CONFIG_DIR`. Keeping oxide-code's login in its own file lets `/logout` leave Claude Code signed in, while refreshing an inherited Claude Code login still writes rotated tokens back to Claude Code's store because refresh invalidates the old refresh token.
2. **One file on every platform, in Claude Code's JSON shape.** Like Codex and pi (`auth.json` at `0600`), a single file avoids Keychain prompts and platform code. Using Claude Code's `claudeAiOauth` JSON layout shares one parser, refresh path, and write-back (preserving unknown fields in Claude Code's file), written atomically with mode `0600` in a `0700` directory alongside `user-id`.
3. **The oxide-code login takes precedence over the Claude Code login.** Explicit `/login` outranks an inherited Claude Code login, and a broken oxide-code login (such as expired without refresh token) stops with its own recovery message so it never silently falls back. Explicit credentials still outrank both, and stored logins go only to first-party base URLs.
4. **The flow matches Claude Code's login.** Uses Claude Code's client ID, authorize URL (`https://claude.ai/oauth/authorize`, the redirect target of `claude.com/cai`), token URL (`https://platform.claude.com/v1/oauth/token`), and scopes. PKCE generates 32 OS-random bytes each for verifier and `state` (base64url, `S256`). The callback uses `localhost` bound to `127.0.0.1` (as in pi), reading connections concurrently so idle browser pre-connects never block `GET /callback`, redirecting valid requests to Claude's success page, and returning 400 on bad `state` while still waiting. Manual paste uses Claude Code's manual redirect URI and `code#state` format, requiring `state` to match so codes from other attempts are rejected.
5. **The engine runs the flow, and the TUI opens the browser.** Keeping sign-in in the engine swaps credentials in place without passing tokens over the protocol, lets each request carry its operation's credential while preserving the client connection pool (so `/login` and `/logout` apply from the next op and in-flight titles keep theirs), and lets the frontend handle browser launch. `Op::Interrupt` closes the listener and marks sign-in interrupted with pre-queued prompts held, and login times out after 5 minutes. Pasting into the prompt input keeps codes out of transcripts and the queue.
6. **Signed out is a session state.** TUI sessions without a working credential start signed out (and stay open after `/logout`) so `/login` can authenticate them, while prompts point to `/login`. After transient sign-out failures (held lock or silent token endpoint at startup or `/logout` fallback), prompts and `/compact` retry loading on a reload task whose wait Esc can cancel while any refresh still writes back (emitting `Event::LoginRestored` on recovery) until the failure turns permanent. `-p` fails at startup because it cannot run `/login`.
7. **TUI only.** `-p` runs no slash commands, so headless runs authenticate via an interactive `/login` or `CLAUDE_CODE_OAUTH_TOKEN`.
8. **Requests read the store only near expiry or after a 401.** Matching Claude Code's 5-minute window avoids slow or prompting macOS Keychain reads on every request.
9. **A 401 refreshes the login whatever its expiry.** Recovers from clock skew or server-side revocation while the refresh token remains valid.
10. **The stored refresh token shows whether another process wrote the store.** Comparing only the refresh token detects concurrent rotations while ignoring unrelated writes like Claude Code updating `mcpOAuth` entries in the same document.

## Sources

- `crates/ox-core/src/auth.rs`: `AuthSource`, the stored-login search, and `Auth::credential`, which refreshes a stored login before a request.
- `crates/ox-core/src/auth/oauth.rs`: `SessionLogin`, which holds a session's token, and `save_login` and `delete_login`, which `/login` and `/logout` call.
- `crates/ox-core/src/auth/oauth/keychain.rs`: Claude Code's macOS Keychain entry.
- `crates/ox-core/src/auth/oauth/lock.rs`: the lock directory, which matches Claude Code's `proper-lockfile`.
- `crates/ox-core/src/auth/oauth/login.rs`: PKCE, the loopback listener, the pasted code, and code exchange.
- `crates/ox-core/src/auth/oauth/store.rs`: credential stores, refresh, and write-back.
- `crates/ox-core/src/auth/oauth/token.rs`: the session's `Token`, its expiry checks, and the token endpoint requests that issue it.
- `crates/ox-core/src/config/credentials.rs`: `AuthState`, resolution order, and the fallback that keeps stored logins off gateways.
- `crates/ox-core/src/engine/login.rs`: `SignIn`, `Engine::ensure_signed_in`, `Engine::login`, and `Engine::logout`.
- `crates/ox-tui/src/slash/login.rs`: `LoginCmd` and `LogoutCmd`.
