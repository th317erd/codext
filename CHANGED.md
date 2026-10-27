# Changes in This Fork

This file is the requirements source for upstream reapplies. It has two parts:

- **Applied** — behavior that exists on the current branch and must be re-implemented on every
  new stable tag.
- **Deferred** — older fork work that is *not* present in the current tree. Do not re-implement
  it by accident; revisit it only with an explicit decision.

## Applied on `rust-v0.164.0`

### Safety buffering is not surfaced

- `codex-rs/core/src/session/turn.rs` swallows `ResponseEvent::SafetyBuffering`: the turn loop
  never emits `EventMsg::SafetyBuffering`.
- Consequence: the TUI's safety-buffering status/retry UI stays dormant. Its upstream code is
  kept as-is (no fork-side deletions) because no event ever reaches it.
- Do not "fix" this by re-emitting the event; suppressing it is the fork behavior.

### Server-overloaded Responses requests retry once per second

- `codex-rs/core/src/responses_retry.rs` retries `CodexErrorDetails::ServerOverloaded` every
  second with a user-visible "Selected model is at capacity; retrying in 1s" notice, before the
  generic retry-budget gate, so a missing server `retry-after` hint no longer ends the turn.
- `codex-rs/protocol/src/error.rs` renders that variant as "Selected model is at capacity.
  Retrying automatically."

### Remote app-server transport hardening

- `codex-rs/app-server-client/src/remote.rs` raises the max WebSocket message size to 512 MiB,
  rejects oversized outbound payloads with an explicit `InvalidData` error instead of failing
  deep inside tungstenite, and reports transport failures through one shared
  `remote_transport_error_message` helper (including during `initialize`).

### Paginated resume sends metadata-only history

- `codex-rs/app-server/src/request_processors/thread_processor.rs`: a paginated `thread/resume`
  without `includeTurns` and without an initial page no longer clones the full rollout into the
  JSON-RPC response. The response carries thread metadata with an empty history; the persisted
  revision is preserved.

### TUI reflow sanitizer

- `codex-rs/tui/src/wrapping.rs` gains `sanitize_line_for_reflow`: a line containing a `data:`
  URL, a token over 8 KiB, or a line over 30 KiB is replaced by a short placeholder before
  wrapping, and both `adaptive_wrap_line_with_source` and `word_wrap_line_with_source` short
  circuit on it.
- `codex-rs/tui/src/terminal_hyperlinks.rs` exposes `sanitize_hyperlink_lines_for_reflow` and
  `sanitize_lines_for_reflow`; a sanitized line drops its terminal hyperlink metadata.
- Applied at history-cell render/height and static pager content. Upstream's
  `MAX_HYPERLINK_DESTINATION_BYTES` guard covers the oversized-destination half.

### Missing tool outputs degrade instead of panicking

- `codex-rs/core/src/context_manager/normalize.rs` keeps upstream's inherited-call guard but logs
  through `tracing::error!` instead of `error_or_panic` for a missing custom-tool or local-shell
  output, so a preserved-prefix fork's snapshot cannot abort a debug build.

### npm / release packaging

- `@loongphy/codext` with the `codext` command, per
  `.agents/skills/codex-upstream-reapply/references/npm-release.md`: fork `rust-release.yml`,
  `codex-cli/{package.json,bin/codex.js,bin/rg,scripts/build_npm_package.py,scripts/install_native_deps.py}`,
  and every upstream workflow the fork does not need deleted.

## Deferred

These sections describe fork work that is **absent** from every branch since
`feat/rust-v0.144.3`. `README.md` still advertises the user-facing ones; the code does not
implement them. Bring any of them back only as a deliberate decision.

- **TUI status header and polling** — model/effort, cwd, git ahead/behind/changes, rate-limit
  remaining, account identity above the composer. Former modules:
  `chatwidget/status_header.rs` (up to `feat/rust-v0.141.0`). See
  `.agents/skills/status-header/SKILL.md` for the layout rules; upstream now has its own
  rate-limit and effort status surfaces, so re-implementing needs a fresh design pass.
- **TUI composer draft clipboard shortcut** — `Ctrl+Shift+C` copies the draft, falling back to
  the existing `Ctrl+C` path when empty.
- **TUI auth.json watcher** — reload credentials on external `auth.json` writes, debounced,
  deferred while a turn runs, with retry and an account-change warning.
- **App-server auth.json account switching** — `reload_auth_from_storage_if_idle` at the
  `thread/start`, `thread/resume`, and `turn/start` boundaries.
- **TUI queued messages after usage-limit exhaustion** — pause queued-message autosend while
  quota is exhausted, resume with exactly the first queued message when quota returns.
- **`usage_limit_resume_prompt`** — `[tui].usage_limit_resume_prompt` config plus the synthetic
  recovery turn parked until an auth change.
- **`codext resume` hints** — user-facing resume guidance must say `codext resume <session>`.
  Currently the tree still prints upstream's `codex resume ...`.
- **Rollout-reconstruction ownership refactor** — superseded: upstream's reconstruction now
  replays borrowed slices and moves the `Arc` on resume, so the fork's index-based lazy plan
  bought nothing that this tag does not already do.
- **Shell snapshot `shopt`/`extglob` capture** — superseded: upstream's bash snapshot script
  already emits `shopt -p` before `declare -f`.
