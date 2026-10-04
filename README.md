# unmv

Ever run `mv a b` and immediately wish you hadn't?

Press Up, Ctrl+A, type `un`, and press Enter:

```sh
unmv a b
```

`unmv` examines the filesystem and attempts to reverse the move using its
original positional arguments. It requires no history log or replacement for
`mv`. It supports macOS/BSD `mv`, GNU `mv`, and filesystem moves made by
PowerShell's `Move-Item`.

## Install

Requires Rust 1.98 or newer:

```sh
cargo install --locked --path .
```

Or build with `cargo build --release` and run `target/release/unmv` (on Windows,
`target/release/unmv.exe`). The no-overwrite rename implementation comes from a
pinned revision of [renamore](https://github.com/mevanlc/renamore).

## Usage

```text
unmv [OPTIONS] SOURCE... DESTINATION
```

Use the original source paths and destination, relative to the same working
directory as the original move. Quote paths as usual; use `--` before operands
beginning with a dash.

```sh
unmv old-name new-name
unmv first second destination-directory
unmv --dry-run 'old name' 'new name'
unmv -- 'old name' '-new-name'
```

For one source, `unmv` considers both a rename (`b` back to `a`) and a move into
an existing directory (`b/a` back to `a`). A single plausible, safe interpretation
runs automatically. If both are plausible, an interactive terminal presents an
explicit choice; Enter cancels. Select a mode for scripts or when interpretation
cannot be determined safely:

```sh
unmv --mode rename a b
unmv --mode into-dir a b
```

Multiple sources require a directory container. A trailing destination separator
can mean either a plain directory rename or a directory container for BSD/GNU
moves; it does not resolve that ambiguity. If one plausible interpretation fails
preflight, `unmv` requires an explicit mode rather than
silently choosing the other.

| Option | Purpose |
| --- | --- |
| `--dry-run` | Preview operations and required flags; never prompt or modify files. Ambiguous previews show both choices and exit unsuccessfully. |
| `--mode rename\|into-dir` | Select the original move's interpretation. `rename` accepts one source. |
| `--mv-kind bsd\|gnu\|powershell` | Explicitly choose move semantics when shell inspection cannot recognize the command. |
| `--allow-cross-volume-delete` | Authorize verified copies followed by permanent deletion of copied originals. |
| `--allow-metadata-loss` | Permit specifically reported metadata or hard-link preservation losses. |
| `--yes` | Skip cross-volume confirmation. Required risk flags and ambiguity resolution still apply. |
| `--shell-integration bash\|zsh\|pwsh` | Print an optional shell helper. |

Successful same-volume execution is silent on stdout. Selected previews go to
stdout; ambiguous choices, context notices, confirmations, and errors go to
stderr. Exit codes are `0` for success, `1` for refused/failed operations or a
cancelled choice, `2` for argument errors, and `130` for Ctrl+C.

## Shell context

A standalone executable cannot inspect its parent's live aliases and functions.
It reports unverified shell/PATH hints and uses conservative positional inference.
Unknown detected wrappers require `--mv-kind` before execution.

The optional helpers inspect the current shell's command resolution each time
`unmv` runs, without executing alias or function bodies. They recognize known
native executables, simple aliases with layout-neutral flags such as `mv -i`,
and PowerShell's `Move-Item` cmdlet. Arbitrary functions, wrappers, and aliases
that change the operand layout require an explicit `--mv-kind` override.
Inspection describes the **current** command, not proof of what ran earlier.

Load the helper in the shell where you intend to use it:

```sh
# Bash
eval "$(unmv --shell-integration bash)"

# Zsh
eval "$(unmv --shell-integration zsh)"
```

```powershell
# PowerShell 7 or Windows PowerShell 5.1
unmv --shell-integration pwsh | Out-String | Invoke-Expression
```

Helpers install only an `unmv` function. They do not edit shell profiles or wrap
`mv`, and capture the executable's current absolute path. In PowerShell, they
also resolve FileSystem PSDrive paths and source arrays for `Move-Item`.

## Filesystem behavior

The whole batch is checked before changes begin. Restored names must be absent,
including dangling symlinks; parent directories must already exist. Duplicate or
overlapping mappings, mounted directory roots, unsupported copy types, and
unsafe preflight conditions are refused. Publication uses native exclusive
renames. There is no overwrite, directory merge, or check-then-overwrite fallback.

Same-volume restores rename existing entries and retain their metadata and hard
links. Cross-volume restores require explicit authorization:

```sh
unmv --dry-run /original/a /other-volume/b
unmv --allow-cross-volume-delete /original/a /other-volume/b
# Non-interactive execution requires --yes as well.
```

Cross-volume execution inventories regular files, directories, and symlinks,
copies into private staging directories on the restoration volumes, compares
payload bytes, and verifies metadata. All restores are published before any
copied original is deleted. Original entries are rechecked and removed leaf by
leaf; source directories are removed only when empty. Special entries, nested
mounts, and unknown Windows reparse points are refused. PowerShell's profile
excludes cross-volume directory moves that `Move-Item` cannot have performed.

Inspected modes, access/modification times, ownership, ACLs, extended attributes,
and native file attributes are preserved and checked where supported. Internal
hard links are preserved when restored to the same volume. Loss of connections
to external hard links, inaccessible metadata, or destination incompatibilities
is reported and requires `--allow-metadata-loss`. Windows SACL/audit inspection
can require privileges; `unmv` reports inaccessible security metadata and does not
enable privileges. macOS resource forks and Windows alternate data streams are
payload: missing or differing contents cannot be waived as metadata loss.
Resource forks on non-regular macOS entries are refused.

Inode identity, change/creation times, sparse allocation, compression layout,
and other filesystem storage details are not promised across volumes. Windows
flushes file data but has no general directory fsync equivalent here.

Failures and cancellation can leave partial restores or retained originals.
Errors list published restore paths and remaining source paths; failed staging
cleanup reports the staging directory. Inspect those paths before attempting
recovery. There is no automatic rollback or power-failure transaction, and path
rechecks cannot provide isolation from arbitrary concurrent filesystem changes.

## Scope

This is inference from the filesystem as it exists now. It cannot recover a
destination overwritten by the original move, reconstruct earlier contents, or
prove that a move happened. Original `mv` flags, shell glob reconstruction,
pipelines, and non-filesystem PowerShell providers are outside v1. Supply literal
operands rather than replaying an original wildcard or flag-bearing command.

## Development

```sh
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo nextest run
cargo build --release
```

Cross-volume tests use `/dev/shm` on Linux. Elsewhere, set `UNMV_TEST_VOLUME` to
a writable directory on a second filesystem. Set
`UNMV_REQUIRE_CROSS_VOLUME_TESTS=1` to fail when that fixture is unavailable;
otherwise such tests report the missing fixture and return early. Shell tests
exercise installed Bash, Zsh, and PowerShell runtimes. CI provisions a second
filesystem and runs native Linux, macOS, and Windows tests.
