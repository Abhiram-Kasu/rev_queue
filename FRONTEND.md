# Frontend implementation handoff

Build the native GPUI review application described in [DESIGN.md](DESIGN.md).
This document is the implementation brief for a future coding agent. The Rust
backend exists; the GPUI frontend does not. Keep code comments sparse, explaining
only non-obvious invariants. Use small, coherent commits throughout implementation.

## Start here

Read DESIGN.md sections 2–7, 9–13, and 18 for product semantics and shared model
intent. Read [README.md](README.md) for actual runtime behavior and known limits.
Then inspect these implementation files before writing integrations:

- [src/model.rs](src/model.rs): authoritative serialized types and commands.
- [src/client.rs](src/client.rs): current asynchronous HTTP client.
- [src/api.rs](src/api.rs): routes, named-route request types, response envelopes.
- [src/main.rs](src/main.rs): daemon startup, runtime credentials, CLI arguments.
- [src/review.rs](src/review.rs): supported marker syntax and byte mapping.
- [tests/http_api.rs](tests/http_api.rs) and
  [tests/backend_flow.rs](tests/backend_flow.rs): working API/workflow examples.

The design's Rust snippets are illustrative. Import the actual model types rather
than copying them or expecting every design field to exist. Existing backend
limits are part of the integration contract until deliberately changed and tested.
Keep domain validation in the backend; the UI can explain its results.

## Deliverable and boundaries

Deliver a working macOS-first desktop application that can connect to the local
daemon, select a goal, review its queue, edit a thunk snapshot, save drafts, publish
feedback, and inspect replacement/history/checkpoint state. Use GPUI, with domain
records shared through the existing Rust library.

Do not build a browser frontend, replace Git storage, introduce SQLite, merge code
into the user's branch, or implement agent orchestration as part of this work.
Do not start by rewriting the backend into many crates. A small desktop workspace
member depending on the existing `rev_queue` library is a reasonable default;
choose the smallest package layout that keeps GPUI out of backend modules. Preserve
the existing backend executable and commands. If extracting a lightweight shared
crate later becomes useful, make that an isolated refactor with compatibility tests.

First perform a GPUI/editor integration spike: verify a compatible dependency
revision, native build requirements, text input/IME, selection, undo/redo, and a
large-file rendering approach. Consult current official GPUI documentation and
maintained editor-component sources before choosing versions. Do not assume GPUI
itself provides a complete code editor. Record the selected approach and build
instructions in README.md. Avoid committing a throwaway editor experiment as the
final user-facing app.

## Runtime and client integration

Start the backend separately for the first version:

```sh
cargo run -- serve /absolute/path/to/source-repository 127.0.0.1:3000
```

The service prints its credential-file path. The file is
`revqueue-runtime.json` in the repository's canonical common Git directory, which
can differ from `<worktree>/.git` for linked worktrees. Use a user-selected path or
resolve the common directory correctly. Authenticate the desktop with the reviewer
token, never the supervisor-agent token. Do not log tokens, put them in review
history, or display them in ordinary errors. The file currently contains actor and
token records, not the full design-level session descriptor or server address;
collect the loopback endpoint separately.

Use `rev_queue::client::Client`. It supplies `goals`, `goal`, `queue`, `preview`,
`execute`, and generic JSON `get`/`post` helpers. The helpers prepend `/v1/`; pass
paths such as `goals/{id}`, without another `/v1`. Add typed methods for missing
queries rather than spreading string paths and serde details across views.

Current gaps to address in the client layer:

- Cursor-aware queue/history queries and encoded query parameters.
- Thunk details, draft recovery, source patch preparation, and anchor reattachment.
- Authenticated binary blob fetching: the JSON helper cannot decode source bytes.
- SSE subscriptions, if implemented. Bounded polling is acceptable initially.

Use `execute` with `GoalCommand` for mutations so the UI does not need a separate
method for every named route. Preserve exact `CommandEnvelope` values across an
uncertain retry. Creation uses absent head/epoch; existing-goal commands supply
the exact current head and epoch. Draft saves also check their expected edition.

Own an explicit async runtime/client bridge. Tokio-dependent HTTP work must run
within a Tokio runtime; GPUI tasks do not automatically provide one. Send results
back through GPUI's supported entity/update APIs. Keep network, Git, parsing, and
large diff work off the UI thread. The desktop must not acquire the backend's
writer lock or construct a second `Service` merely to read source.

## Screens and local state

Use the three-pane layout in DESIGN.md: queue, code/diff, feedback. Add a compact
connection/goal selector and persistent draft-save/publish status. Start with these
local entities, importing all domain types:

| Entity | Owns |
| --- | --- |
| Application | Connection, runtime/client bridge, goal list, selected goal |
| Goal review | GoalDetail/version, queue pages, filters, selected thunk, pending writes |
| Review editor | Exact base, draft identity/edition, file buffers, selection, undo history |
| Feedback panel | Selected anchor/thread, draft sidebar feedback, marker intent toggles |
| Publish dialog | Frozen preview, diagnostics, affected thunk list, submission status |
| History/checkpoints | Pinned history pages, checkpoint selection, restore preview |

Keep rendering state separate from protocol objects. Do not serialize GPUI
`Entity` handles, task handles, editor undo stacks, or connection credentials.
`TextRange` offsets are UTF-8 bytes; editor-native character/display positions
must be converted against the exact blob. Preserve source bytes/newlines. Show
binary or non-editable source honestly instead of lossy conversion.

Display backend queue status, blockers, confidence rationale, validation reports,
and unresolved feedback. Approval is for one exact thunk and never means merge.
Stale approved work must remain visibly stale. Graph views supplement the queue;
they are not required to navigate ordinary review.

Treat `GoalDetail.actionable_feedback` as authoritative after restore. Historical
published requests can remain visible for audit without being current work.

## Implement this vertical slice first

1. Connect to a running daemon and load an existing goal/queue using actual client
   models. Show useful empty, loading, disconnected, and error states.
2. Select a thunk, fetch `ThunkDetail`, and display its exact structured diff with
   file navigation, old/new line numbers, and before/after blob content.
3. Open a copy of the after-tree in the review editor. Editing must not change the
   agent's worktree or mutate an existing thunk.
4. Add a Rust `// #! instruction`, make an ordinary code edit, and save a draft.
5. Preview and publish. Show extracted feedback, cleaned code edits, diagnostics,
   and invalidated thunks. Refresh only after a confirmed receipt.
6. Have the existing backend/API test setup submit a replacement. Show its new
   iteration, history, and reset approval state.

Complete this real end-to-end flow before expanding graph visualization or polish.
Use a temporary/demo repository for fixtures; do not seed application review refs
into this implementation repository merely to develop screenshots.

## Draft and publication details

To save editor contents, generate a patch against the exact immutable base and
call `POST /v1/goals/{goal}/source/patch` with `api::PatchInput`. Save the returned
annotated snapshot through `GoalCommand::SaveDraft` with `ReviewDraftInput` and
the expected draft edition. Import the actual response type from the route/client;
do not guess its JSON shape. Snapshot preparation alone is not a durable draft:
the successful SaveDraft transaction retains its objects.

Debounce saves and show dirty/saving/saved/failed states. Serialize saves and other
writes per goal; autosave commits also change the goal head. A close, navigation,
or publish action must flush pending edits or offer an explicit keep/discard choice.
Recover saved drafts from the reviewer-only drafts endpoint. A consumed/discarded
draft ID cannot be reused: create a new draft to continue editing.

Do not duplicate the backend extraction algorithm in the UI. Lightweight marker
highlighting is fine, but preview results decide which feedback and cleaned edits
will be published. Expose marker intent overrides, including opting literal
`#!` examples out. Preserve ordinary comments and unsupported-language edits.
Published markers become feedback decorations rather than source text in the
cleaned result. Use retained `DraftExtraction` maps even when there was no actual
reviewer edit. Ambiguous/orphaned anchors require visible explanation.

Flush saves before calling `Client::preview`. Freeze returned digest, version,
draft editions, and original `PreviewPublish`. Publish using
`GoalCommand::PublishFeedback` with that exact request/digest, expected preview
head, and epoch. Do not silently recompute and publish different contents after
a conflict; obtain and show a new preview. Pause local autosave during this final
preview/publish boundary; concurrent agent writes can still invalidate it.

Scope and invalidation are separate controls: a note may have no invalidation
roots, while actual code edits must invalidate their target thunks. Present
goal-wide impact clearly. Approval, feedback publication, and request resolution
must remain separate actions.

Current backend limits to surface:

- Rust markers only; standalone markers attach to the next code line, not a full
  syntax node. EOF produces a file-level anchor/diagnostic.
- Multiple edited drafts touching the same file require consolidation, even when
  their hunks are disjoint. Show the conflict instead of pretending publication worked.
- Source preparation objects are provisional until committed by a draft/submission.
  Save promptly; do not perform aggressive pruning during this flow.
- Source files/blob requests have a 16 MiB limit; provide a clear oversized-file state.

## Concurrency and failure behavior

- Tag reads with goal/thunk selection and a local request generation. Ignore late
  results for an obsolete selection. Preserve dirty buffers on background refresh.
- Queue/history cursors pin a head. Continue a page set with its returned cursor;
  restart pagination when deliberately refreshing to current state.
- On version, epoch, draft, or preview conflict, refetch authoritative state and
  reconcile. Never retry changed intent with an old command ID.
- A dropped response may follow a successful write. Keep the pending envelope
  and retry identically to retrieve its receipt. Do not create duplicate feedback.
- SSE is a refresh hint, not state or delivery acknowledgment. Reconnect by
  refetching durable data; bounded polling can provide the same initial behavior.
- On restore or externally published changes, retain unsaved buffers in a recovery
  view instead of applying them automatically against the new state.
- No background task may overwrite local work or reset a branch to resolve errors.

## Checkpoints and history

Add checkpoint/history UI after the core review loop. Compose active source through
the backend before creating a checkpoint; do not use the last selected thunk's
after-tree as the whole goal. Use the actual checkpoint command with a new ID.

Restore appends history and advances the epoch. Show the checkpoint source and
review state being restored. Opening its files creates a new worktree using the
backend workspace endpoint with a nonexistent destination; it does not overwrite
the current agent/user workspace. Expose materialization failures independently of
successful review-state restoration.

## Verification and completion

Add meaningful controller/client integration tests for save ordering, uncertain
write retries, outdated read responses, conflict recovery, preview invalidation,
UTF-8 anchor conversion, marker-only drafts, and restore with dirty buffers. Reuse
temporary Git/API fixtures instead of mocking away all backend semantics.

Manually verify native typing, IME, selection, undo/redo, keyboard navigation,
scrolling, diff alignment, draft recovery after restart, and disconnection states.
Check a file large enough to exercise virtualization; the app must stay responsive.
Test an ordinary user-facing workflow with a real daemon and temporary repository.

Preserve backend checks: `cargo test --all-targets`, `cargo fmt --check`, and
`cargo clippy --all-targets -- -D warnings`. Once a workspace is added, document
and run checks for all applicable members; desktop builds may need native system
dependencies. Record commands actually run and material platform limitations.

Finish with updated launch instructions, selected dependency revisions, keyboard
controls, and any unresolved gaps. A frontend is complete only when the real
draft-to-preview-to-publication loop works and survives conflicts/restart, not
when a static queue or editor mockup renders.
