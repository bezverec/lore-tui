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

Three views, switched with `tab` or `1` `2` `3`:

- **Status** — staged, unmerged and unstaged changes, with the file detail beside the list.
- **History** — the revision log, with message, author, date and parents per revision.
- **Repositories** — what lives on the server this working copy points at, with per
  repository description, default branch, creator and creation date. The repository the
  current working copy belongs to is marked `●`.

Actions available from the status view: stage, unstage, commit, push and sync. Files can be
marked with `space` and staged or unstaged as a batch; with nothing marked the action applies
to the selected file. Push and sync ask for confirmation first. The bottom pane streams the
events of whatever is running.

| Key | Action |
| --- | --- |
| `tab`, `1` `2` `3` | switch between status, history and repositories |
| `j` `k` `↑` `↓`, `g` `G`, `PgUp` `PgDn` | move the selection |
| `space` | mark the file for the next stage or unstage |
| `s` | stage the marked files, or the selected one when nothing is marked |
| `S` | scan the working copy and stage everything |
| `u` | unstage the marked files, or the selected one when nothing is marked |
| `c` | commit the staged revision (opens a message prompt) |
| `p` | push to the remote |
| `y` | sync to the branch tip |
| `r` | refresh the status, or the repository list in the browser |
| `R` | refresh the history |
| `esc` | clear the marks, or dismiss the status message |
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
  reads the local working copy — so branch information is limited to the repository you are
  standing in.
- **Diffs.** The status view reports which files changed, not what changed inside them.
- **Concurrent operations.** One Lore call runs at a time; the rest queue behind it.

## How it works

See [docs/architecture.md](docs/architecture.md) for the event model, the threading, and the
Lore behaviours worth knowing about before extending this.

## License

MIT, matching upstream Lore. See [LICENSE](LICENSE).

Lore itself is a trademark and project of Epic Games; this is an independent tool and is not
affiliated with or endorsed by Epic Games.
