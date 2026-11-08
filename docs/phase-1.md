# Phase 1: start, profile, state repo

Status on 2026-11-08. The "done when" from [PLAN.md §11](PLAN.md#11-phases):
*First start interviews me and writes `work/profile.toml`; `stop` on one
machine and `start` on another opens the same profile.*

## Done

| Piece | Where | State |
|---|---|---|
| `start --profile --harness --model [--take-over]`: lease, reindex, cockpit, `agent.start` in the session pane, one-line prompt pointing at the brief | `src/session.rs` | written; **not yet run inside herdr** (no herdr binary in this sandbox) |
| Full profile schema: tools, repos, model tiers with fallbacks, modes with rules and cap, routing, memory, state repo | `src/profile.rs` | tested |
| `profile set key=value …` with validation, diff, history, `undo`, the protected set (`modes`, `repos`, `limits`, `routing` need `--yes`), and the replay of the last 50 requests showing whose mode would change | `src/profile.rs`, `src/cli.rs` | tested, exercised end to end |
| Daemon hot-reloads a changed profile and tells open tickets whose mode the new cap no longer allows | `src/daemon.rs` | exercised |
| Calibration: `doctor` inventory (harnesses, MCP servers per harness, repos, herdr), the calibration skill and the session brief | `src/calibration.rs`, `skills/superagent/` | `doctor` exercised; the interview itself needs a live harness |
| State repo: profile dir is the checkout; `ticket.md` + `ticket.json` + `events.jsonl` per ticket; batched commits (≤ 1/min) and push; `reindex` rebuilds the database | `src/staterepo.rs` | tested, exercised with a bare remote |
| Lease: fast-forward-only push, 15-minute expiry, 5-minute renew, `--take-over`; the daemon refuses to start on a profile another machine holds | `src/staterepo.rs`, `src/daemon.rs` | tested with two checkouts of one remote |

## Verified locally (no herdr)

With a bare git repo as the remote and two "machines" (two state and config
directories, `HERDR_SUPERAGENT_MACHINE=alpha|beta`):

1. `doctor` lists the installed harnesses with their MCP servers.
2. `profile set description=… tools.code_host=bitbucket repos[+].path=…` is
   **refused** because `repos` is protected; the same with `--yes` saves and
   prints the diff and the new version.
3. `profile set state_repo.kind=remote state_repo.remote=…` turns the profile
   directory into a checkout; the daemon takes the lease (`status` shows
   `lease alpha until …`).
4. Tickets are written to `tickets/<date>-<slug>-<id>/`; `sync` commits and
   pushes; `git log` shows `lease: alpha takes profile`, `superagent: sync`,
   `superagent: stop`, `lease: release`.
5. `profile set modes.cap=checkpoint --dry-run` shows the diff **and** the
   autonomous research ticket whose mode would drop to `checkpoint`.
6. On beta, `daemon` is refused while alpha holds the lease, with alpha's
   name and the lease expiry. After alpha's `stop`, beta's daemon rebuilds its
   database from the remote (`rebuilt the database from the state repo
   tickets=2`) and lists the same tickets.

## Not yet run

- **Everything that needs herdr**: `start` (workspace, `agent.start`,
  `agent.prompt`), the calibration interview in a live harness, and the
  restart smoke test from phase 0. The sandbox cannot build herdr because the
  proxy blocks Zig's dependency hosts (`deps.files.ghostty.org`,
  `codeberg.org`), and the environment change could not be approved from the
  mobile app. Allow those two hosts from a desktop session and run
  `scripts/smoke-restart.sh` first.
- **Secret redaction** before commit (PLAN §8a): ticket records carry titles,
  summaries and notes only, but no redaction pass exists yet. Phase 2 adds it
  with the session summaries.

## Decisions taken while building

- **`ticket.json` beside `ticket.md`.** The plan says plain text, never the
  database. `ticket.md` is for people; `ticket.json` is the full record so
  `reindex` is lossless. Both are text and reviewable.
- **`start` takes the lease, the daemon renews it.** `start` runs in your
  shell, so a refused lease is a plain error with the other machine's name.
  The daemon acquires only when no live lease exists, renews every five
  minutes, and refuses to start on a profile another machine holds.
- **Profile changes reach the daemon two ways.** `profile set` asks the daemon
  to reload; the daemon also watches the file's mtime, so a hand edit or a
  pull from another machine is picked up within a tick.
- **The replay uses the proposed mode.** A request's own `--mode` is rare;
  the superagent proposes the mode at routing. The replay therefore takes the
  mode from the ticket's mandate when the request started one.
- **`claude mcp list` is the source for Claude's MCP servers**, with
  `~/.claude.json` as fallback; Codex reads `~/.codex/config.toml`; Pi's
  layout is reported as unknown until verified on a real install.

## Next: phase 2

Workers in worktrees (`worktree.create`, `agent.start` with the tier's
harness, the brief), `report`/`checkpoint`/`approve` end to end, turn-end
fallback with `completion_seq`, `max_workers` and the queue, the events
subscription, recovery, session summaries and the per-project FTS memory, and
the read-only task list in the cockpit.
