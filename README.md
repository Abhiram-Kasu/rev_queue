# Rev Queue

A local, Git-backed review queue for agent changes. The backend is implemented in Rust; the GPUI desktop application is a future consumer of the shared `model` and `client` modules. See [DESIGN.md](DESIGN.md) for the product design.

## Run

Requires Rust and Git on PATH. Start inside an existing source repository:

```sh
cargo run -- serve . 127.0.0.1:3000
```

The service binds loopback only. Startup prints the path to a local `revqueue-runtime.json` in the repository's Git directory. It contains separate reviewer and supervisor-agent bearer tokens and actor IDs; it is created with owner-only permissions on Unix. Pass the appropriate token as `Authorization: Bearer <token>`. Credentials are not committed to review history. The repository has one writer daemon, enforced by an OS file lock; Git ref compare-and-swap remains the durable concurrency boundary.

```sh
cargo run -- inspect .
cargo run -- repair . <goal-uuid>
cargo run -- export . /tmp/review-backup.bundle
cargo run -- import /path/to/other/git-repository /tmp/review-backup.bundle
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Export includes all Rev Queue goal/workspace refs and their complete reachable object closure. Import never updates normal branches and refuses any nonidentical existing application ref; import into a separate repository to inspect a divergent backup. Review state restoration uses an appended restore command, rather than importing an older bundle over newer history.

## Rust and HTTP contracts

`model` owns serde domain records, commands, query records and errors. It has no GPUI types. `core` contains deterministic reduction, graph validation and queue derivation. `persistence` and `source` invoke Git plumbing using isolated indexes; `review` lexes Rust annotations; `anchors` reattaches immutable anchors without guessing through ambiguity; `diff` produces structured byte-accurate diffs. `service` validates authenticated commands and publishes atomic transactions. `api` serves Axum routes and `client::Client` provides a typed asynchronous client. These are separate modules in one package, rather than individual workspace crates.

The route table in DESIGN.md is implemented under `/v1`. Additional endpoints:

- `POST /goals/{goal}/commands`: accepts any `CommandEnvelope<GoalCommand>`, including goal closure and acknowledgment of restore/control events.
- `GET /goals/{goal}/drafts[?head=<goal-history-oid>]`: recover that reviewer's saved draft editions at a pinned history point.
- `POST /goals/{goal}/source/patch`: apply `PatchInput { base, patch }` to an owned source snapshot in an isolated index; `patch` is a JSON byte array.
- `POST /goals/{goal}/source/capture`: capture `CaptureInput { base, worktree, paths }` from an explicit regular-file allowlist in a registered worktree of this repository.
- `POST /goals/{goal}/source/compose`: compose active thunk patches, rejecting conflicts.
- `POST /goals/{goal}/checkpoints/{checkpoint}/workspace`: create a new detached worktree at an absolute, nonexistent destination.
- `POST /goals/{goal}/anchors/reattach`: return exact/context/ambiguous/orphaned attachment against an authorized blob.

All paths above are prefixed by `/v1`. Every state-sensitive command supplies a UUID `command_id`, exact `expected_head`, and current `expected_epoch`. Retry an uncertain response with the identical envelope and actor: deduplication precedes version checks. Changing intent requires a new ID. Creation requires a client-assigned goal UUID and absent expected head/epoch. Named-route payload structs live in `api`; generic payloads use the snake-case tagged `GoalCommand` enum. Invalid input, authorization and conflicts return typed `ApiError` bodies. Requests and blob responses are limited to 16 MiB.

Queue/history pagination returns a JSON cursor containing a pinned application head and offset. Pass the returned cursor unchanged (URL-encoded) to continue; updates do not alter the pinned result. SSE sends `goal_changed` hints or `refresh` after lag; refetch durable queries on connect/reconnect. SSE acknowledgment is never delivery acknowledgment.

## Review and recovery semantics

Thunks are immutable, with explicit replacement lineage and exact dependency IDs. Atomic submission batches may reference other client-assigned IDs in that batch. Approval is separate from delivery, feedback resolution and code application. Validation commands are reports; the backend never executes them.

Draft saves append editions. Published/discarded draft IDs cannot be reused; start a new draft to continue editing. Preview freezes exact editions and the current head. Publication recomputes and verifies its digest, retains annotated/cleaned trees and byte maps, delivers structured feedback, advances the epoch, and invalidates the declared transitive closure. No marker is delivered as an instruction before publication.

Rust `// #! instruction` extraction uses comment token recognition, preserving strings, raw strings, nested block comments, docs, existing marker comments, CRLF, UTF-8 and unrelated bytes. Standalone instructions target the next code line; trailing instructions target preceding code. Existing markers need explicit enablement; new markers can be disabled. Whitespace preceding trailing comment tokens remains exact and can therefore appear as an actual whitespace edit. Marker-only standalone drafts create feedback without an edit patch. Source maps are retained even when no edit patch is needed. EOF instructions use a file-level anchor and preview diagnostic.

Restore restores the checkpoint's active thunks, reviews and actionable-feedback view, archives drafts, preserves historical batches and acknowledgments, and increments the epoch. `GoalDetail.actionable_feedback` distinguishes requests in the restored state from abandoned historical requests. Unacknowledged historical deliveries remain available along with the restore control event; agents reconcile against the current goal state. Creating a checkpoint requires its source to equal the exact conflict-checked composition of active thunks. Opening a restored workspace is separate and never overwrites existing files.

## Current boundaries

- Rust inline comments are supported; other files support ordinary edits and sidebar feedback. Standalone attachment uses the documented code-line fallback, not a full Rust syntax tree.
- Overlapping reviewer edits to the same file are conservatively rejected, including disjoint hunks; consolidate them before publication. Source composition uses exact patch application and requires explicit reconciliation on conflicts.
- Source capture double-reads allowlisted files to detect changes; pause workspace writers for a coherent capture. It cannot guarantee an atomic snapshot of a concurrently changing filesystem. Patch submission against a known tree avoids that ambiguity.
- Source import/capture results are provisional Git objects until referenced by a successful draft/submission transaction. Do not run aggressive Git pruning between preparing objects and submitting them.
- `state.json` is a cached projection checked against transaction headers at startup. `repair` replays every transaction, compares every cached state, and verifies retained objects; startup does not perform a full replay. Unknown schemas fail with upgrade-required; format migrations are not yet defined.
- Inbox reads return all matching durable deliveries; only queue/history have pinned pagination. Live notifications are process-local hints. No automatic polling worker, frontend or agent command-line submission wrapper is included; use the typed client or HTTP API.
- The service trusts the local filesystem and Git installation. Bearer capabilities protect the loopback API, not repository contents from other local processes. There is no remote sync, automatic merge, or automatic execution of validation commands.
