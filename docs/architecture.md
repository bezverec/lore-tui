# Architecture

Three modules, and the split between them is the whole design:

| Module | Responsibility |
| --- | --- |
| [`ops.rs`](../src/ops.rs) | Talks to Lore. Turns event streams into plain snapshots. |
| [`app.rs`](../src/app.rs) | State, key handling, the request queue. |
| [`ui.rs`](../src/ui.rs) | Draws `App`. Knows nothing about Lore. |

No Lore type reaches the drawing code. `ops` converts everything into `String`, `u64` and
`bool` on the way out, which keeps the rendering stable when Lore's internals move.

## The Lore call model

Every Lore entry point takes the same shape:

```rust
pub async fn status(
    globals: LoreGlobalArgs,
    args: LoreRepositoryStatusArgs,
    callback: LoreEventCallback,
) -> i32;
```

`LoreEventCallback` is `Option<Box<dyn Fn(&LoreEvent) + Send + Sync>>`. The call streams
`LoreEvent`s into it while it runs, and the outcome arrives as a `Complete` event carrying
a status code and an error detail — the `i32` return value is the same code.

Two things follow from this:

- **Results are accumulated, not returned.** A status call emits one
  `RepositoryStatusRevision` and then one `RepositoryStatusFile` per changed file. The
  callback pushes them into an `Arc<Mutex<…>>` that the caller drains afterwards.
- **Success has to be read off `Complete`.** Failures are also reported through a `Log`
  event at error level, which is how the CLI prints them, so `ops::callback` records both:
  the terminal status for control flow, the log text for the UI's log pane.

`ops::callback` wraps every per-operation handler with that shared behaviour — log events to
the UI buffer, `Complete` into an `Outcome`, everything else to the handler.

## Threading

`lore::runtime()` hands back a Tokio runtime handle, and the CLI drives calls with
`runtime().block_on(…)`. A TUI cannot block its render loop, so each operation runs on its
own thread and reports back over an `mpsc` channel:

```
key press → App::dispatch → queue → ops::spawn (thread) → mpsc → App::poll → redraw
```

Progress is visible before the operation finishes because the callback writes into a shared
log buffer (`Arc<Mutex<Vec<String>>>`) that the UI reads on every frame — separate from the
channel that delivers the final result.

**One at a time.** `App` keeps a `VecDeque<Request>` and starts the next one only when the
worker goes idle. Overlapping calls would race the snapshot being drawn, and Lore serializes
repository writes anyway. Status and history reloads are collapsed rather than queued twice,
so a commit that triggers a refresh while one is already pending does not run two identical
scans.

## Lore behaviours worth knowing

**Status only reports what is flagged dirty.** Without `scan`, Lore reads the tracked dirty
flags and nothing else, so edits made outside Lore are invisible. An empty file list would
then mean "nothing was flagged", which reads exactly like "nothing changed". This TUI always
scans, including on first load.

**Revision metadata arrives as separate events.** `history` emits a `RevisionHistoryEntry`
followed by `Metadata` events (`message`, `timestamp`, `committed-by`, …) that belong to the
entry emitted just before them. They are folded into the most recent entry as they stream
in — the same association the CLI makes.

**`LoreArray::from_vec(vec![])` is not the way to pass an empty array.** It allocates with a
zero-size layout. Use `LoreArray::default()`.

**Event data types are not re-exported.** `lore::interface` re-exports the argument structs
but not every event payload type, so a helper cannot always name its parameter. Where that
happens the conversion is inlined into the match arm, where the type is inferred.

**`repository::info` reports `created` in milliseconds**, although the field is documented as
seconds. Read as seconds it puts every repository in the year 58558.

**`lock::file_query` beats `lock::file_status` for a whole-repository view.** `file_status`
answers for paths you name, so painting the lock column would mean sending every path in the
list; `file_query` with empty branch, owner and path filters returns every lock on the branch
in one call. Releasing with an empty owner means "mine".

**`notification::subscribe` returns immediately, and the callback outlives it.** It registers
a subscription rather than blocking for the lifetime of the stream, so no long-running
request is needed and the queue keeps its one-at-a-time rule. Events keep arriving at the
callback afterwards because `LORE_CONTEXT` — the task-local carrying the event dispatcher,
and therefore the callback — is propagated into the background task that reads the stream.
The callback writes into a sink the UI drains per frame, not into anything tied to the
request that created it.

Notifications say that something moved, not what it moved to, so each kind turns into the
read that answers it: a push refreshes the status without a filesystem scan (the remote moved,
the working copy did not), a lock or unlock refreshes the lock list.

**The three merge resolutions take two different routes, and report two different events.**
Keeping one side (`merge_resolve_mine` / `_theirs`) stages that side's content and reports
`FileStageFile`; accepting the file as it stands (`merge_resolve`) only flags the node and
reports `BranchMergeResolveFile`. Counting one of them alone reports zero for the other.

**A merge in progress is not a flag on the status event.** It is inferred the way the CLI
infers it: a staged revision that differs from the current one, with an incoming revision
recorded.

**`branch::create` switches to the branch it creates.** There is no separate switch step to
take afterwards, and the branch list has to be re-read either way.

**Switching does not reset local changes.** `LoreBranchSwitchArgs::reset` would make the
incoming revision overwrite modified files; the branch view leaves it off, so a switch that
would lose work fails instead of succeeding quietly.

**The remote URL comes from the repository config.** `repository::config_get("remote_url")`
returns what the working copy was created against — no need to parse `.lore/config.toml`.

## The repository browser

`repository::list` returns only an id and a name per repository. Everything else needs a
`repository::info` call against `<remote>/<name>`, so detail is fetched lazily, and only
while nothing else is running. Scrolling a long list therefore issues no requests until the
selection settles — the queue does the throttling. Results are cached by name, failures
included, so a repository that cannot be read is not retried on every frame; `r` clears the
cache and reloads.

## Extending it

The natural next steps, in rough order of effort:

- **Diff between two revisions** — `file::diff` already takes a source and a target
  revision; the diff view leaves both empty, which Lore reads as "current revision" and
  "current filesystem state". Wiring the history view to it would give per-revision diffs.
- **Clone from the browser** — `repository::clone` streams progress events that map onto the
  existing log pane; the open question is where the working copy lands and what the TUI does
  afterwards.
- **Locks on files outside the status list** — the lock column is driven from the status
  list, so an unchanged file cannot be locked from the TUI. A dedicated lock view built on
  `lock::file_query` would cover it.
