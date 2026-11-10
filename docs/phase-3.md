# Phase 3: delegation and guard rails

Status on 2026-11-10. The "done when" from [PLAN.md §11](PLAN.md#11-phases):
*A worker spawns a reviewer pane and acts on its review.*

## Done

| Piece | Where | State |
|---|---|---|
| Child assignments: `spawn --role --task [--mode]` under a ticket, never more autonomous than the parent; the child runs in a new tab of the ticket's workspace, same worktree, with its task in the brief | `src/core/tickets.rs::spawn_inner`, `src/executor.rs` | engine tested; live run pending |
| Reports addressed to an assignment (`SUPERAGENT_ASSIGNMENT`); a child's final report ends the child and reaches the parent as a message; only the root's `ready` ends the ticket | `src/core/assignments.rs` | tested |
| `watch <assignment>`: blocks until the child's final report | `src/cli.rs` | written |
| Observer follows every running assignment, root and children; `max_workers` counts them all | `src/executor.rs` | written |
| Guard shims for autonomous mode: `git`, `gh`, `bb`, `rm` on the worker's PATH call `herdr-superagent guard`, which holds risky actions back as a `guard-<tool>` checkpoint (force or default-branch push, `reset --hard`, `clean -f`, branch delete, `pr merge`, `repo delete`, `rm -r` outside the worktree) and passes everything else to the real tool | `src/guard.rs` | rules tested |
| The superagent skill (`skills/superagent/SKILL.md`), installable with `npx skills add ribeirorod/herdr-superagent --skill superagent`; applies only under `SUPERAGENT_TICKET`/`SUPERAGENT_ROLE` and takes precedence over herdr's skill | `skills/superagent/` | written |
| Sidebar: `view set` installs an Agents view (this profile's agents plus anything blocked, by attention); `view snippet` prints the purple `$sa_role` / `$sa_task` / `$sa_stage` styling for herdr's config | `src/cli.rs` | written; live run pending |

## Not in this phase

- Fallbacks on MCP availability per task (PLAN §3) wait for packs (phase 4),
  which declare what a task needs.
- The view is opt-in (`view set`), not applied by the daemon: it replaces the
  whole Agents list, which is a person's choice.

## Live test

```bash
herdr-superagent --profile test submit "Add input validation to scripts/smoke-restart.sh and have a reviewer check it" --workflow software-dev --project ~/src/sa-scratch
```

Expect the worker to plan, get approval, implement, then run
`herdr-superagent spawn --role reviewer --task "…"`; a `reviewer` tab opens
in the ticket's workspace; the dashboard shows `· 2 agents`; the reviewer's
`report --state ready` ends its tab's assignment and lands in the worker's
pane as a message; the worker acts on it and reports. Then
`--mode autonomous` on a second ticket, and ask the worker to
`git push origin main`: the shim records a `guard-git` checkpoint instead.
