# Phase 2: spin + review (v1)

Status on 2026-11-09. The "done when" from [PLAN.md §11](PLAN.md#11-phases):
*I run 4 tasks in parallel: a research task runs end to end, a feature stops
at its plan checkpoint, and I approve it from the task list.*

## Done

| Piece | Where | State |
|---|---|---|
| Executor: `launch` makes a worktree (`worktree.create`, branch `sa/<ticket>`) or a workspace, exports `SUPERAGENT_*` into the pane's shell, starts the tier's harness with the mode's flags, writes the brief | `src/executor.rs` | written; **needs a live herdr** |
| `deliver`: prompts the worker with the brief pointer and what waits for it; defers on `agent_not_ready`/`agent_blocked` | `src/executor.rs` | written; needs a live herdr |
| Queue: `limits.max_workers`; extra launches wait as `working/queued` | `src/executor.rs` | written |
| Observer / turn-end fallback: `agent.list` once per tick; `completion_seq` advance → `turn_ended`, and a `waiting` report plus a status request when the worker said nothing; `blocked` → report blocked; blocked → working → progress; `unknown` 5 min or pane gone → failed | `src/executor.rs::assess` | pure part tested |
| Recovery re-attaches a live worker pane instead of restarting it | `src/daemon.rs::recover` | written |
| Per-mode harness flags, user-editable (`[harness_flags]`) | `src/profile.rs` | tested |
| Worker brief with mode rules, notes, the CLI contract and the project's memory | `src/brief.rs`, `skills/superagent/worker.md` | tested |
| Memory: FTS5 index per project (repo slug), `memory add/search/show`, redaction, summaries in the state repo (`summaries/NN-<role>.md`, `memory/<project>/notes/`), reindex | `src/memory.rs`, `src/staterepo.rs` | tested, exercised |
| Cockpit v1: session (65 %) \| read-only dashboard; the daemon in its own tab | `src/dashboard.rs`, `src/ensure.rs` | dashboard exercised; layout needs a live herdr |

## Verified locally (no herdr)

- A summary added for a ticket is indexed, redacted (`token=…` masked),
  written to `tickets/<dir>/summaries/01-worker.md`, committed and pushed;
  a search in another project finds nothing.
- `dashboard --once` renders the sections from the daemon's tickets.
- Without herdr the `launch` effects defer and `status` shows them waiting.

## Not yet run (needs a live herdr)

The whole worker loop: launch, delivery, the observer, re-attach, the
cockpit layout with the dashboard pane. Expect the first live run to find
shape mismatches in `agent.list` fields or in the `worktree.create` result;
`--debug` logs every request and response, so a `debug-bundle` is enough to
fix them.

## Decisions taken while building

- **Polling instead of `events.subscribe`.** One `agent.list` per tick (2 s)
  covers all workers with one request and no long-lived connection in the
  single-threaded daemon. Events come back when the dashboard needs them.
- **Worker env through the shell.** `agent.start` takes no env, so `launch`
  types `export SUPERAGENT_TICKET=… PATH=…` into the new pane first.
- **The brief lives in the state dir**, not in the worktree, so it never
  lands in a commit.
- **Harness flags are profile data.** Defaults: Claude
  `--dangerously-skip-permissions`, Codex `--full-auto` in `autonomous`;
  nothing in `checkpoint` and `propose`. Pi has none until verified.
- **Reviewer role → `models.review`**, everything else → `models.hard`; the
  first entry whose harness resolves on the login shell's PATH wins.

## Next: phase 3

Delegation: the superagent skill, `spawn`/`watch`, builder + reviewer, guard
shims for autonomous mode, the sidebar view and `$sa_*` tokens.
