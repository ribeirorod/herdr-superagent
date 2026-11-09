# herdr-superagent

A [herdr](https://github.com/herdrdev/herdr) plugin: one **superagent**
session that routes your requests into tickets, runs workers in herdr panes
and lets you review at checkpoints. Harness-agnostic (Claude Code, Codex and
Pi first), model-agnostic, one profile per context (work, personal, …).

The design is in [docs/PLAN.md](docs/PLAN.md). This repository is at
**phase 2**: the engine, the daemon, profiles with calibration, the state
repo with its lease, and the executor that runs workers in herdr panes. See
`docs/phase-*.md` for what works today and what is next.

## Install

```bash
herdr plugin install ribeirorod/herdr-superagent   # or: herdr plugin link /path/to/checkout
herdr-superagent start --profile work
```

The build step downloads the prebuilt binary for your platform (or builds it
with `cargo` when no release asset exists) and links it into `~/.local/bin`.
`herdr-superagent` must be on `PATH`: workers call it, and herdr uses it to
resume the daemon after a server restart.

## What works today

Inside herdr, `herdr-superagent start --profile work` creates a
`superagent · work` workspace with the cockpit layout (the superagent session
on the left, the daemon on the right), starts the superagent in your harness
(`--harness claude|codex|pi`, `--model …`) and briefs it. On a profile's first
start the brief is the **calibration** interview: the superagent runs
`herdr-superagent doctor`, asks how you work, and writes the profile through
`herdr-superagent profile set`, which validates every change, shows a diff and
keeps history for `profile undo`. Keys that affect how much review a request
gets (`modes`, `repos`, `limits`, `routing`) can only be changed by you, with
`--yes`, and every change replays your last 50 requests to show whose mode
would change.

With `state_repo.kind = "remote"`, the profile directory is a checkout of a
private git repo: tickets are written as `ticket.md`, `ticket.json` and
`events.jsonl`, committed in batches and pushed. One machine runs a profile at
a time (a lease, renewed every five minutes); `start` on another machine pulls
the records, takes the lease and rebuilds the local database.

```text
herdr-superagent submit "Fix the flaky login test" --workflow software-dev --mode checkpoint
herdr-superagent submit "Research pgvector vs sqlite-vec"    # waits for routing
herdr-superagent candidates
herdr-superagent route <request> accept --title "Compare vector stores" --mode autonomous
herdr-superagent tickets
# in a worker pane (SUPERAGENT_TICKET is set):
herdr-superagent checkpoint plan --summary "Plan: reproduce, fix retry, add test"
herdr-superagent report --state progress --summary "need input" --question "Keep the old API?"
# you:
herdr-superagent approve t-b395b8 --comment "go"
herdr-superagent status
```

Accepted tickets become **workers**: a worktree (branch `sa/<ticket>`) or a
workspace, the tier's harness started with the mode's flags, a brief that
carries the request, the mode rules and the project's memory. Workers report
with `herdr-superagent report`, stop at `checkpoint`s, and end with a session
summary (`memory add`) that later tickets of the same project find with
`memory search`. The cockpit's right pane is a live task list.

## Testing a checkout in your herdr

```bash
git clone https://github.com/ribeirorod/herdr-superagent ~/src/herdr-superagent
cd ~/src/herdr-superagent
cargo build --release
mkdir -p bin ~/.local/bin
rm -f bin/herdr-superagent && cp target/release/herdr-superagent bin/   # replace, never overwrite (macOS kills an overwritten binary)
ln -sf "$PWD/bin/herdr-superagent" ~/.local/bin/herdr-superagent   # must be on PATH
herdr plugin link ~/src/herdr-superagent                            # from a herdr pane
herdr-superagent --debug start --profile test
```

After pulling: `cargo build --release && rm -f bin/herdr-superagent && cp
target/release/herdr-superagent bin/ && herdr-superagent ensure`. The daemon
notices the new version and restarts itself in its pane. On macOS never `cp`
over the existing binary: the kernel caches its code signature and kills every
launch with `killed`; replace the file or run
`codesign --force --sign - bin/herdr-superagent`.

## Debugging

`--debug` (or `HERDR_SUPERAGENT_DEBUG=1`) makes every process log at debug
level: each herdr request and response, each daemon call, each effect, the
state reports to herdr and the daemon's own decisions. Logs go to one file per
process kind under `~/.local/share/herdr-superagent/profiles/<profile>/logs/`
(`daemon.log`, `cli.log`, `ensure.log`), rotated at 20 MB.

```bash
herdr-superagent --profile test logs --follow             # tail everything
herdr-superagent --profile test logs --process daemon --lines 500
herdr-superagent --profile test debug-bundle              # one tar.gz to share
```

The bundle holds the logs, `daemon.json`, `profile.toml`, `calibration.md`,
`doctor`, `status`, all tickets, the herdr session snapshot and the plugin
list, plus the `HERDR_*`/`SUPERAGENT_*` environment. It never includes
credentials or MCP configs.

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
herdr plugin link .          # inside herdr, with bin/herdr-superagent built
```

State lives under `~/.local/share/herdr-superagent/profiles/<profile>/`,
config under `~/.config/herdr-superagent/profiles/<profile>/profile.toml`.
`HERDR_SUPERAGENT_STATE_DIR` and `HERDR_SUPERAGENT_CONFIG_DIR` override both
for throwaway runs; `HERDR_SUPERAGENT_LOG` sets the log filter.

## License

MIT. The engine re-implements, in Rust, the kept part of DeepSeek's
[dsh-superagent prototype](https://github.com/deepseek-ai/dsh-superagent-prototype)
(MIT), using its tests as the specification; see [docs/rejected.md](docs/rejected.md)
for what was left out and why.
