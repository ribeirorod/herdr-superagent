# Phase 0: the skeleton

Status on 2026-11-08. The "done when" from [PLAN.md §11](PLAN.md#11-phases):
*`herdr plugin link .` + `herdr-superagent start` opens the workspace; core
tests pass; restart smoke test recorded.*

## Done

| Piece | Where | State |
|---|---|---|
| Rust crate, one binary with CLI, daemon and plugin actions | `src/` | builds, clippy clean |
| Plugin manifest with build, startup hook and actions | `herdr-plugin.toml` | written; `[[build]]` fetches the release asset or builds with cargo |
| `ensure`: workspace + cockpit layout + daemon pane, version check, restart | `src/ensure.rs` | written; **not yet run inside herdr** (see below) |
| Daemon: lock by socket, IPC, effect queue, herdr registration with `resume_argv`, recovery | `src/daemon.rs` | runs; exercised locally over its socket |
| SQLite store with savepoint transactions and the durable effect queue | `src/store/sqlite.rs` | tested |
| Engine core: Request → Evaluation → Decision → Ticket + Mandate → Assignment → Report; stages; checkpoints; retry, restart, resume | `src/core/` | 49 tests, ported from the prototype's `core-engine.spec.ts` where kept |
| Profile file and the `Policy` it yields (modes, cap, routing timeout, pause) | `src/profile.rs` | minimal; calibration is phase 1 |
| CI (fmt, clippy, test on Linux and macOS) and release builds for four targets | `.github/workflows/` | written; first run on push |

## Verified locally (no herdr)

Run with `HERDR_SUPERAGENT_STATE_DIR` pointing at a scratch directory:

1. `daemon` starts, a second `daemon` for the same profile is refused.
2. `submit --workflow` creates a ticket at once; `submit` without one leaves a
   candidate that `candidates` lists and `route … accept` turns into a ticket;
   a repeated `submit --id` is a duplicate.
3. `checkpoint` moves the ticket to `person/review`; `approve` queues the
   continuation; `report --question` moves it to `person/answer`;
   `report --state ready --question x` is refused; `report --state ready`
   ends it as completed.
4. `stop` exits the daemon; a new daemon lists the same tickets.

## Not yet run: the restart smoke test

The plan asks for proof that `pane.report_agent` with `resume_argv` brings
the daemon back after a herdr server restart. This sandbox has no herdr
binary (the vendored libghostty-vt needs Zig 0.16, which the sandbox cannot
download), so the test is written but **not executed**:

```bash
herdr --session sa-smoke            # a disposable session, herdr-superagent on PATH
sh scripts/smoke-restart.sh         # ensure → record pid/pane → server stop → start → wait
```

Expected: `PASS: herdr resumed the daemon in pane w1:p2` with a new pid. If
herdr instead leaves the pane at a shell, the fallback is a `[[startup]]`
hook, which `ensure` already is, so the daemon still comes back at server
start; what would be lost is only the pane identity.

Run this first when a herdr install is at hand, and record the result here.

## Decisions taken while building

- **Checkpoints are a ticket field, not a report state.** `checkpoint` also
  records a `waiting` report on the assignment, so an interrupted worker at a
  checkpoint keeps its session (the prototype's "settled" rule) and the stage
  table reads `person/review` ahead of the report.
- **The assessor is an inbox.** Candidates wait in the store for `candidates`
  and `route`; phase 2 delivers them to the superagent session with
  `agent.prompt` and starts the deadline on delivery (`candidate_delivered`).
- **One daemon thread.** The engine, the store and the IPC run on one thread
  with `Rc`/`RefCell`; connections are handled inline. The superagent does
  not need concurrency inside the daemon, and this keeps the store simple.
- **Effects keep their place, not their payload.** A re-queued effect keeps
  its `seq` and bumps a `generation`, so running the stale payload cannot
  delete the newer one (the prototype's test "keeps an effect's place").
- **Upgrade = `exec`.** `ensure` asks a daemon of another version to
  `shutdown {restart: true}`, and the daemon `exec`s the binary on PATH in the
  same pane, so herdr's pane and resume command stay valid.
- **Socket path length.** State lives under `~/.local/share`, but a state
  directory over ~90 bytes falls back to `$TMPDIR/herdr-superagent-<user>/`
  for the socket (Unix socket paths are capped at 104–108 bytes).

## Next: phase 1

`start --profile --harness --model` that runs `agent.start` in the session
pane; calibration; `profile set/undo` with the protected set and mode dry-run;
the per-profile state repo with ticket files and the fast-forward lease
(PLAN §8a).
