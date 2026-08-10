# lore-tui

A terminal UI for [Lore](https://github.com/EpicGames/lore), the open source version control
system from Epic Games.

It links the `lore` client library directly rather than shelling out to the `lore` CLI, so
every operation is a library call whose events stream into the interface as they happen.

```
┌ lore ────────────────────────────────────────────────────────────────────────────────────┐
│path  /work/my-project                                                                    │
│branch main  revision 2  remote in sync (2)                                               │
│signature 87aa2102549c1e9bfd41919c71dc25ee70bac54615a32dc8ec0fe9be8ae5a9f3                │
└──────────────────────────────────────────────────────────────────────────────────────────┘
┌ Status ───────────────────────────────────────┐┌ Detail ────────────────────────────────┐
│Staged (2)                                     ││repository 019fc68515ba7281aed1ebac5588 │
│  A  README.md                                 ││2 staged · 0 unmerged · 1 unstaged      │
│  M  hello.txt                                 ││                                        │
│Unstaged (1)                                   ││Staged                                  │
│  M  assets/notes.txt                          ││README.md                               │
│                                               ││size 46 B                               │
└───────────────────────────────────────────────┘└────────────────────────────────────────┘
┌ Log ─────────────────────────────────────────────────────────────────────────────────────┐
│> commit                                                                                  │
│[info] Fragmenting files and updating tree hashes                                         │
│< commit ok                                                                               │
└──────────────────────────────────────────────────────────────────────────────────────────┘
 committed revision 2 (87aa2102549c)
```

## What it does

Four views, switched with `tab` or `1` to `4`:

- **Status** — staged, unmerged and unstaged changes, with the file detail beside the list.
- **History** — the revision log, with message, author, date and parents per revision.
- **Branches** — local and remote branches with their category, creator and latest revision.
  `enter` switches the working copy to the selected branch, `n` creates one and `m` merges
  one into the current branch. Creating also switches, which is Lore's behaviour rather than
  a shortcut taken here.
- **Repositories** — what lives on the server this working copy points at, with per
  repository description, default branch, creator and creation date. The repository the
  current working copy belongs to is marked `●`.

`d` opens a scrollable unified diff. In the status view that is the marked files, or the
selected one, as the working copy stands against the current revision; in the history view it
is what the selected revision changed, against its first parent. The diff sits outside the
view cycle, and `d`, `esc` or `q` returns to whichever list opened it.

File locks — the answer to binary assets that cannot be merged — are shown as an `L` column in
the status list, with the owner and time in the detail pane. `l` locks and `L` releases,
across the marked files or the selected one.

`h` lists the revisions that touched the selected file, with the action, the size at each
revision and a marker for merges. Like the diff, it is a detour that returns where it came
from — and it reads the selection rather than the marks, since one file's history is the only
thing it can mean.

`t` in the history view browses the files of the selected revision — the whole tree as it
stood then, not just what changed. `enter` descends into a directory, `esc` steps back up one
level and leaves the tree from the root.

`C` rewrites the message of the most recent revision, prefilled with the message it replaces.
Note that this produces a new revision signature, so amending something already pushed leaves
the local branch diverged from the remote. `D` throws away local changes, restoring the files
from the current revision; untracked files are left alone.

Merging runs from the branch view. A clean merge commits itself; a conflicted one leaves the
files in the status view's `Unmerged` section, marks the header `merge in progress`, and waits
for `o`, `t` or `x` per file — or `A` to abandon the whole thing. An ordinary commit finishes
it.

The TUI subscribes to server push notifications, marked by a `●` in the header. When someone
else pushes or locks a file, the header and the lock column follow along on their own — no
refresh key. A server without a notification service just leaves the `●` off.

Actions available from the status view: stage, unstage, commit, push and sync. Files can be
marked with `space` and staged or unstaged as a batch; with nothing marked the action applies
to the selected file. Push and sync ask for confirmation first. The bottom pane streams the
events of whatever is running.

| Key | Action |
| --- | --- |
| `tab`, `1`–`4` | switch between status, history, branches and repositories |
| `enter` / `n` / `m` | branches: switch to / create / merge the selected branch |
| `o` / `t` / `x` | conflict: keep mine / take theirs / accept the file as it is |
| `A` | abort the merge in progress |
| `j` `k` `↑` `↓`, `g` `G`, `PgUp` `PgDn` | move the selection |
| `space` | mark the file for the next action that takes files |
| `d` | diff: the marked files in status, the selected revision in history |
| `h` | the revisions that touched the selected file |
| `t` | history: browse the files of the selected revision |
| `l` / `L` | lock / unlock the marked files, or the selected one |
| `s` | stage the marked files, or the selected one when nothing is marked |
| `S` | scan the working copy and stage everything |
| `u` | unstage the marked files, or the selected one when nothing is marked |
| `c` / `C` | commit the staged revision / amend the last commit message |
| `D` | throw away local changes to the marked files, or the selected one |
| `p` | push to the remote |
| `y` | sync to the branch tip |
| `r` | refresh the list in view: status and locks, branches, or repositories |
| `R` | refresh the history |
| `esc` | leave a diff or tree, clear the marks, or dismiss the status message |
| `?` | key help |
| `q`, `ctrl-c` | quit |

## Usage

```bash
lore-tui [repository-path]
```

Without an argument it walks up from the working directory looking for a `.lore` (or `.urc`)
directory, the same way the Lore CLI resolves the repository you are standing in.

## Building

Requires a Rust toolchain with edition 2024 support and `protoc` — Lore's build scripts
generate gRPC code, and the build fails without it.

```bash
# Debian/Ubuntu
sudo apt-get install protobuf-compiler
cargo build --release
```

Two things in this repository exist purely to make Lore build as a dependency, and both are
easy to lose in a refactor:

- **`.cargo/config.toml`** carries `--cfg tokio_unstable` and `--cfg uuid_unstable`. Lore
  keeps these in its own `.cargo/config.toml`, which does not apply to projects that depend
  on it. Without them `lore-base` fails to compile.
- **`[patch.crates-io] quinn-proto`** in `Cargo.toml` repeats the patch Lore applies in its
  own workspace. A `[patch]` only takes effect for the workspace that declares it, so
  without repeating it here Lore's transport would build against upstream `quinn-proto`.

The `lore` dependency is pinned to a specific revision. Lore is versioned `0.8.7-nightly`,
is not published to crates.io, and makes no API stability promise, so an unpinned dependency
would break without warning.

## Tests

```bash
cargo test
```

The tests cover what can be checked without a server: how marks and the selection resolve into
the paths an action runs on, how the section headers are stepped over, how a dumped tree
becomes one directory's listing, and how a patch line is classified. They exist because each
of those is somewhere a bug already happened once.

Anything that talks to Lore is exercised by hand instead — see the note at the end of
[docs/architecture.md](docs/architecture.md).

## Docker

Building in a container avoids installing the Rust toolchain and `protoc` on the host:

```bash
docker build -t lore-tui .
```

Run it against a working copy, with a terminal attached:

```bash
docker run --rm -it -v /path/to/workspaces:/work -w /work/my-project lore-tui
```

If the Lore server also runs in Docker, put the container on the same network (`--network
<name>`) so the `lore://` URL in the working copy config resolves.

## Not included

- **Cloning.** The browser is read-only; it lists and inspects, it does not fetch.
- **Branches of other repositories.** Lore's `branch::list` has no URL parameter — it only
  reads the local working copy — so the branch view covers the repository you are standing
  in, not the ones in the browser.
- **Archiving or deleting branches**, and `branch::merge_into` — merging the current branch
  into another one, rather than the other way round.
- **Diffs between two arbitrary revisions.** `d` covers the working copy and a revision
  against its parent; picking any two revisions is not wired up, though `file::diff` takes
  them. A merge revision is shown against its first parent only.
- **Locking a file with no changes.** Locks are driven from the status list, which only holds
  files that differ from the current revision. Locking an untouched file needs the CLI.
- **Telling your locks from everyone else's.** Both render the same; the owner is in the
  detail pane. On a server without authentication every lock reads as `<unknown>`.
- **Concurrent operations.** One Lore call runs at a time; the rest queue behind it. Push
  notifications are the exception — they arrive outside the queue.

## How it works

See [docs/architecture.md](docs/architecture.md) for the event model, the threading, and the
Lore behaviours worth knowing about before extending this.

## Known advisories

Three advisories come with the Lore dependency tree and cannot be resolved here. They are
listed so nobody has to work that out from a dependency scanner:

| Crate | Advisory | Why it cannot be fixed in this repository |
| --- | --- | --- |
| `quinn-proto` 0.11.13 | GHSA-6xvm-j4wr-6v98, GHSA-4w2j-m93h-cj5j | Lore pins it exactly (`=0.11.13`) *and* replaces it with a vendored fork. A `[patch]` to 0.11.15 fails to resolve against the exact requirement. |
| `jsonwebtoken` 9.3.1 | GHSA-h395-gr6q-cpjc | Fixed in 10.3.0; Lore requires 9.x. |
| `opentelemetry_sdk` 0.31.0 | GHSA-w9wp-h8wv-79jx | Fixed in 0.32.1; Lore requires 0.31, and for a 0.x crate that is a breaking bump. |

Moving the pinned Lore revision forward does not help — upstream `main` carries the same
versions. The fixes have to land in Lore first.

What they mean for a client: the `quinn-proto` issues are denial of service and memory
exhaustion in the QUIC transport, reachable by a server you connect to, not remote code
execution. The `jsonwebtoken` type confusion affects the side that *validates* tokens, which
is the server. Telemetry is off in the client. None of this is a reason to be relaxed about
them, but they are inherited rather than introduced here.

## License

MIT, matching upstream Lore. See [LICENSE](LICENSE).

Lore itself is a trademark and project of Epic Games; this is an independent tool and is not
affiliated with or endorsed by Epic Games.
