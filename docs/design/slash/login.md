# Login and Logout

`/login` signs oxide-code in to a Claude account using credentials it owns. `/logout` removes those credentials.

## Implementation

`/login` forwards `Op::Login`. The engine starts a loopback listener on an available port and emits `Event::LoginStarted` with the authorization URL. The TUI opens that URL in a browser and prints it in case the browser does not open. When the browser returns to `http://localhost:<port>/callback` with the matching `state`, the engine exchanges the code and verifier for tokens, writes them to `$XDG_DATA_HOME/ox/credentials.json`, and emits `Event::SignedIn` with the source that requests will use.

`/logout` forwards `Op::Logout`. The engine deletes that file and, if the session was using it or is signed out, loads the Claude Code login in its place. `Event::SignedOut` reports whether a login was removed, which source requests will use afterward, and why a stored Claude Code login could not take over. Both commands end with a message naming that source using the label shown by `/status`.

Credentials resolve in this order: `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `client.api_key`, `CLAUDE_CODE_OAUTH_TOKEN`, the oxide-code login, then the Claude Code login. Both stored logins use the same refresh path, with a separate file and lock for each.

## Design Decisions

1. **oxide-code keeps its own credential store.** Claude Code stores its login in the macOS Keychain item `Claude Code-credentials` or in `~/.claude/.credentials.json`, and its `/logout` deletes that store. oxide-code stores its login only in its own file, which `/logout` deletes, so signing out of one tool leaves the other signed in. Refreshing a Claude Code login still writes the rotated tokens back to Claude Code's store because the refresh invalidates the old refresh token.

2. **One file on every platform, in Claude Code's JSON shape.** Codex and pi both store their logins in an `auth.json` file with mode `0600`, which requires neither a Keychain prompt nor platform-specific code. The file contains the same `claudeAiOauth` document as Claude Code's credentials file, so one parser, refresh path, and write-back process serve both stores. It is written atomically with mode `0600` in a `0700` directory, like the `user-id` file beside it. The token is stored in plaintext at rest, as it is in Claude Code's file store.

3. **The oxide-code login takes precedence over the Claude Code login.** Signing in with `/login` is a choice made in oxide-code, so that login takes precedence over one inherited from Claude Code. If an oxide-code login exists but fails, for example because it expired without a refresh token, the search ends with its own recovery step. A broken login therefore never quietly yields to the Claude Code account. Env vars and `client.api_key` still take precedence over both stored logins, and `/login` explains when one of them keeps the session on another credential. Like the Claude Code login, the oxide-code login is sent only to first-party base URLs.

4. **The flow matches Claude Code's login.** It uses Claude Code's client ID, the Claude.ai authorize URL `https://claude.ai/oauth/authorize` that Claude Code's `claude.com/cai` URL redirects to, the token URL `https://platform.claude.com/v1/oauth/token`, and the scopes Claude Code requests at sign-in. As a result, the tokens refresh and authorize exactly like a Claude Code login. The verifier and `state` are 32 bytes from the OS random source, base64url-encoded, and the challenge is the base64url SHA-256 of the verifier (`S256`). The redirect URI names `localhost`, as Claude Code's does, and the listener binds `127.0.0.1` behind it, as pi's does. It reads connections concurrently, so an idle connection opened by the browser ahead of time never holds up the redirect. It answers only `GET /callback` and redirects the browser to Claude's success page. A callback without the sign-in's `state` gets a 400 while the listener keeps waiting, so a stray local request cannot end the sign-in.

5. **The engine runs the flow, and the TUI opens the browser.** The engine owns the API client, so it can swap the credential in place without passing a token between the engine and frontend over the protocol. Each request adds the credential header itself, so the swap cannot fail and keeps the connection pool. The frontend handles opening the browser because it knows whether one can appear. `Op::Interrupt` closes the listener, and login fails if the browser does not return within 5 minutes. Prompts typed during sign-in wait in the TUI and run once sign-in succeeds or fails. Cancelling with Esc ends the sign-in like a cancelled turn, marking it interrupted and keeping those prompts queued.

6. **Signed out is a session state.** If no explicit credential is set and no stored login works, an interactive session starts signed out and explains why, allowing `/login` to sign it in. Prompts fail with a pointer to `/login` until a credential resolves. If `/logout` leaves no credential, the session stays open in the same state. `-p` and `--no-tui` sessions still fail at startup because they cannot run `/login`.

7. **TUI only.** `-p` and `--no-tui` do not run slash commands. To sign in for those modes, run `ox` once and use `/login`, or set `CLAUDE_CODE_OAUTH_TOKEN`.

## Out of Scope

- **Signing in from another machine.** The redirect returns to the loopback listener, so a browser on another device, as over SSH, cannot finish the sign-in. Claude Code and pi let the user paste the code back into the terminal for this case, which [#239](https://github.com/hakula139/oxide-code/issues/239) tracks.

## Sources

- `crates/oxide-code/src/client/anthropic.rs`: `Client::set_auth`, which swaps the credential that each request sends.
- `crates/oxide-code/src/client/anthropic/transport.rs`: `MessagesRequest`, which carries the credential header into every attempt.
- `crates/oxide-code/src/config.rs`: `Config::load` starts an interactive session signed out.
- `crates/oxide-code/src/config/auth.rs`: `AuthSource`, resolution order, and the stored-login search.
- `crates/oxide-code/src/config/login.rs`: PKCE, loopback listener, and code exchange.
- `crates/oxide-code/src/config/oauth.rs`: credential stores, refresh, and write-back.
- `crates/oxide-code/src/engine.rs`: `Engine::login` and `Engine::logout`, which swap the client's credential.
- `crates/oxide-code/src/protocol.rs`: `Op::Login`, `Op::Logout`, `Event::LoginStarted`, `Event::SignedIn`, and `Event::SignedOut`.
- `crates/oxide-code/src/slash/login.rs`: `LoginCmd` and `LogoutCmd`.
- `crates/oxide-code/src/tui/app.rs`: browser launch and sign-in messages.
- `crates/oxide-code/src/tui/components/status.rs`: `Status::SigningIn`, the status line while the browser is out.
