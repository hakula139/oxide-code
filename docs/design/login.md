# Login and Logout

`/login` signs oxide-code in to a Claude account using credentials it owns, and `/logout` removes those credentials.

## Sign-In Flow

1. `/login` forwards `Op::Login`. The engine starts a loopback listener on an available port and emits `Event::LoginStarted` with two authorization URLs that share the verifier and `state`. The TUI opens the first, which redirects to the listener, in a browser, and prints the second, which redirects to Claude's page at `https://platform.claude.com/oauth/code/callback` that shows the code as `code#state`, for a browser that does not open or runs on another machine.
2. While signing in, the TUI sends a submission shaped like `code#state` as `Op::LoginCode` without echoing it, keeps any other text in the input with a note, and still runs slash commands. `Op::LoginCode` wraps the code in `SignInCode`, whose `Debug` leaves it out, so a logged op never records it.
3. The engine takes whichever arrives first: the browser returning to `http://localhost:<port>/callback` with the matching `state`, or a pasted code whose `state` matches. A pasted code that is incomplete or carries another `state` gets an `Event::Error` with the recovery step, and the sign-in keeps waiting.
4. The engine exchanges the code and verifier for tokens with the redirect URI that carried the code, writes them to `$XDG_DATA_HOME/ox/credentials.json`, and emits `Event::SignedIn` with the source requests will use.

`/logout` forwards `Op::Logout`. The engine deletes that file and, if the session was using it or is signed out, loads the Claude Code login in its place. `Event::SignedOut` reports whether a login was removed, which source requests will use afterward, and why a stored Claude Code login could not take over. Both commands end with a message naming that source by its `/status` label.

## Stored Logins

Credentials resolve in the order [Authentication](../guide/authentication.md) lists. `client.auth` selects a fixed bearer, API-key, or OAuth credential from a private value or a file read once at startup, and its `login` type clears a lower file credential and leaves the fallback chain available.

Both stored logins use the same refresh path, with a separate file and lock for each. The Claude Code login's lock is the directory Claude Code's `proper-lockfile` creates beside the real path of its config directory, as in `~/.claude.lock`, and both tools hold it the same way:

- The holder refreshes its mtime every 5 seconds, and a lock untouched for 10 seconds counts as abandoned.
- A holder whose lock another process took over in the meantime, as after it was suspended, stops refreshing it and leaves it in place.
- A waiter whose token has expired once the 20-second wait ends fails with a transient error that names the lock. A request retries it with backoff, so the user is not told to sign in again, and a session it signs out loads the login again on its next prompt.

## Refresh

A session loads its stored login at startup or sign-in and keeps the access token and its expiry in memory. A request within 5 minutes of that expiry first reloads the login from its store, which refreshes the token and writes it back unless another process already did. Every copy of the credential shares the token, so concurrent requests, such as the title request during a turn, wait for a single refresh.

When the API rejects a stored login with HTTP 401, the session reloads it and resends the request once, with the token another process stored or, if the store still holds the rejected token, one refreshed regardless of its expiry. If that refresh fails or no refresh token is stored, nothing is resent, and the error names the login's sign-in step, as it does when the resent request also gets a 401. Fixed credentials from environment variables and `client.auth` are never reloaded, so their 401 fails at once.

A refresh a request needs, for an expired token or after a 401, retries the request with backoff when the token endpoint sends no response or a server error, as a transient API failure does. Any other refresh failure ends the request with the sign-in step.

Either reload runs on a task of its own, which keeps the token locked until the reload finishes, so a request cancelled mid-refresh, such as a turn interrupted with Esc, still leaves the rotated tokens in the store and in the session. The refresh invalidates the old refresh token, so losing them would force a new sign-in. Shutting down would still cancel the task, so exiting waits for every reload in flight, including one for a login that `/login` replaced. A reload spends at most the 20-second wait for another process's lock and the 15-second token request, plus any Keychain prompt it raises.

Writing the rotated tokens back can fail, as when the Keychain refuses the write or the credentials file cannot be replaced. The session then logs a warning and keeps the rotated refresh token with the access token. While the store still holds the refresh token the session replaced, each reload uses the session's tokens and writes the next refresh's tokens back, warning again if that write fails, so the store catches up once writing succeeds. Any other refresh token in the store came from another process, such as `claude` or a second `ox`, so the session adopts the stored login. Until the store catches up, another process that refreshes the login sends the invalidated refresh token, and its refresh fails.

## Design Decisions

1. **oxide-code keeps its own credential store.** Claude Code stores its login in the macOS Keychain item `Claude Code-credentials` or in `.credentials.json` in its config directory, and its `/logout` deletes that store. The directory is `~/.claude` unless `CLAUDE_CONFIG_DIR` moves it, in which case the Keychain item name gains `-` and the first 8 hex digits of the SHA-256 of the variable's value, and oxide-code reads the same variable to find the same store. oxide-code stores its own login only in its own file, which its `/logout` deletes, so signing out of one tool leaves the other signed in. Refreshing a Claude Code login still writes the rotated tokens back to Claude Code's store because the refresh invalidates the old refresh token.
2. **One file on every platform, in Claude Code's JSON shape.** Codex and pi both store their logins in an `auth.json` file with mode `0600`, which needs neither a Keychain prompt nor platform-specific code. The file holds the same `claudeAiOauth` document as Claude Code's credentials file, so one parser, refresh path, and write-back serve both stores. Write-back keeps the fields it does not parse, so Claude Code's credential file stays intact. The file is written atomically with mode `0600` in a `0700` directory, like the `user-id` file beside it, and stores the token in plaintext at rest, as Claude Code's file store does.
3. **The oxide-code login takes precedence over the Claude Code login.** Signing in with `/login` is a choice made in oxide-code, so that login outranks one inherited from Claude Code. If an oxide-code login exists but fails, such as one that expired without a refresh token, the search ends with its own recovery step, so a broken login never quietly yields to the Claude Code account. An explicit credential still outranks both stored logins, and `/login` explains when it keeps the session on another credential. Like the Claude Code login, the oxide-code login is sent only to first-party base URLs.
4. **The flow matches Claude Code's login.** It uses Claude Code's client ID, the Claude.ai authorize URL `https://claude.ai/oauth/authorize` that Claude Code's `claude.com/cai` URL redirects to, the token URL `https://platform.claude.com/v1/oauth/token`, and the scopes Claude Code requests at sign-in, so the tokens refresh and authorize like a Claude Code login.

   - **PKCE.** The verifier and `state` are 32 bytes from the OS random source, base64url-encoded, and the challenge is the base64url SHA-256 of the verifier (`S256`).
   - **Listener.** The redirect URI names `localhost`, as Claude Code's does, and the listener binds `127.0.0.1` behind it, as pi's does. It reads connections concurrently, so an idle connection the browser opened ahead of time never holds up the redirect. It answers only `GET /callback` and redirects the browser to Claude's success page, and a callback without the sign-in's `state` gets a 400 while the listener keeps waiting, so a stray local request cannot end the sign-in.
   - **Manual code.** The manual path uses Claude Code's manual redirect URI and the same `code#state` format. Claude Code splits the pasted value without comparing its `state`, while oxide-code requires it to match, so a code from another sign-in attempt cannot complete this one.

5. **The engine runs the flow, and the TUI opens the browser.** The engine owns the session's sign-in state, so it can swap the credential in place without passing a token over the protocol. Each request carries the credential its operation signed in with and adds its header itself, so the swap cannot fail and the client keeps its connection pool. `/login` and `/logout` therefore take effect from the next operation, and a title request in flight keeps its turn's credential. The frontend opens the browser because it knows whether one can appear. `Op::Interrupt` closes the listener, and login fails if neither the browser nor a pasted code arrives within 5 minutes. While signing in, the prompt input takes the pasted code, as Claude Code's sign-in screen does, so the code never enters the transcript or the prompt queue. Prompts queued before sign-in wait in the TUI and run once sign-in succeeds or fails. Esc ends the sign-in like a cancelled turn, marking it interrupted and keeping those prompts queued.

6. **Signed out is a session state.** If no explicit credential is set and no stored login works, a TUI session starts signed out and explains why, so `/login` can sign it in, and prompts fail with a pointer to `/login` until a credential resolves. When a transient failure, such as a held refresh lock or a silent token endpoint, signed the session out at startup or as the Claude Code login took over after `/logout`, each prompt and `/compact` first loads the stored login again, emitting `Event::LoginRestored` on success, until the failure turns permanent. The load runs on a reload task, so Esc ends the wait while a refresh still writes back. If `/logout` leaves no credential, the session stays open in the same state. `-p` sessions still fail at startup because they cannot run `/login`.

7. **TUI only.** `-p` runs no slash commands. To sign in for headless runs, run `ox` once and use `/login`, or set `CLAUDE_CODE_OAUTH_TOKEN`.

8. **Requests read the store only near expiry or after a 401.** Claude Code refreshes its login whenever a request falls within 5 minutes of expiry, so a long session never sees the token expire. Reading the macOS Keychain on every request is slow and can prompt, so oxide-code does the same from the expiry it holds in memory.

9. **A 401 refreshes the login whatever its expiry.** The API can reject a token the local clock still counts as valid, as when the two clocks disagree, so counting a rejected token as expired lets the session recover without a new sign-in while the refresh token still works.

10. **The stored refresh token shows whether another process wrote the store.** A refresh invalidates the refresh token it sends, so a store still holding the one the session replaced has seen no sign-in or refresh since, and any other refresh token came from a newer one. Claude Code also rewrites the document to save MCP server logins under `mcpOAuth`, so comparing the whole document would drop the session's tokens after a write that left the login untouched.

## Sources

- `crates/ox-core/src/auth.rs`: `AuthSource`, the stored-login search, and `Auth::credential`, which refreshes a stored login before a request.
- `crates/ox-core/src/auth/oauth.rs`: `SessionLogin`, which holds a session's token, and `save_login` and `delete_login`, which `/login` and `/logout` call.
- `crates/ox-core/src/auth/oauth/keychain.rs`: Claude Code's macOS Keychain entry.
- `crates/ox-core/src/auth/oauth/lock.rs`: the lock directory, which matches Claude Code's `proper-lockfile`.
- `crates/ox-core/src/auth/oauth/login.rs`: PKCE, the loopback listener, the pasted code, and code exchange.
- `crates/ox-core/src/auth/oauth/store.rs`: credential stores, refresh, and write-back.
- `crates/ox-core/src/auth/oauth/token.rs`: the session's `Token`, its expiry checks, and the token endpoint requests that issue it.
- `crates/ox-core/src/client.rs`: `Request`, which carries the credential each conversation request sends.
- `crates/ox-core/src/client/anthropic.rs`: `Client::finish_reloads`, which waits for the reloads in flight.
- `crates/ox-core/src/client/anthropic/transport.rs`: `MessagesRequest`, which carries the credential into every attempt, and `post_messages`, which resends a request after a 401.
- `crates/ox-core/src/config.rs`: `Config::resolve_auth`, which loads the credential or starts a TUI session signed out.
- `crates/ox-core/src/config/credentials.rs`: `AuthState`, resolution order, and the fallback that keeps stored logins off gateways.
- `crates/ox-core/src/engine.rs`: `Engine::finalize`, which waits at exit for the reloads in flight.
- `crates/ox-core/src/engine/login.rs`: `SignIn`, the credential requests send or whether a signed-out session loads the stored login again, `Engine::ensure_signed_in`, which retries a stored login after a transient sign-out, and `Engine::login` and `Engine::logout`, which swap the credential.
- `crates/ox-core/src/protocol.rs`: `Op::Login`, `Op::LoginCode`, `Op::Logout`, `Event::LoginStarted`, `Event::SignedIn`, `Event::LoginRestored`, and `Event::SignedOut`.
- `crates/ox-core/src/util/path.rs`: `claude_config_var` and `claude_dir`, which follow `CLAUDE_CONFIG_DIR` to Claude Code's config directory.
- `crates/ox-tui/src/app/dispatch.rs`: `App::handle_submit_prompt`, which sends the pasted code.
- `crates/ox-tui/src/app/events.rs`: browser launch and sign-in messages.
- `crates/ox-tui/src/app/operation.rs`: `Activity::SigningIn`, the status line while the browser is out, and `Activity::SigningOut`.
- `crates/ox-tui/src/slash/login.rs`: `LoginCmd` and `LogoutCmd`.
