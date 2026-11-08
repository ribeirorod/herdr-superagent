# herdr-superagent

A [herdr](https://github.com/herdrdev/herdr) plugin: one **superagent**
session that routes your requests into tickets, runs workers in herdr panes
and lets you review at checkpoints. Harness-agnostic (Claude Code, Codex and
Pi first), model-agnostic, one profile per context (work, personal, …).

The design is in [docs/PLAN.md](docs/PLAN.md). This repository is at
**phase 0** (the skeleton); see [docs/phase-0.md](docs/phase-0.md) for what
works today and what is next.

## Install

```bash
herdr plugin install ribeirorod/herdr-superagent   # or: herdr plugin link /path/to/checkout
herdr-superagent start --profile work
```

The build step downloads the prebuilt binary for your platform (or builds it
with `cargo` when no release asset exists) and links it into `~/.local/bin`.
`herdr-superagent` must be on `PATH`: workers call it, and herdr uses it to
resume the daemon after a server restart.

## What phase 0 gives you

Inside herdr, `herdr-superagent ensure` (run by the plugin's startup hook)
creates a `superagent · <profile>` workspace with the cockpit layout: the
superagent session pane on the left, the daemon pane on the right. The daemon
registers itself with herdr and keeps the profile's state in SQLite.

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

Workers are **not launched yet**: the engine records every launch, delivery
and stop as a durable effect, and phase 2 runs them in herdr panes. Until then
`status` lists the effects waiting for an executor.

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
