---
name: superagent
description: "Work as a herdr-superagent worker or superagent: report progress, stop at checkpoints, delegate to a reviewer or builder in a sibling pane with spawn and watch, and record a session summary. Applies only when SUPERAGENT_TICKET or SUPERAGENT_ROLE is set; then it takes precedence over the herdr skill."
---

# herdr-superagent

You run inside herdr under the superagent. `SUPERAGENT_TICKET` names your
ticket, `SUPERAGENT_ROLE` your role, `SUPERAGENT_ASSIGNMENT` your assignment.
The brief you were given at start is the authority for the task; this skill is
about how you talk to the superagent and when you delegate.

If `herdr-superagent` is not found, use the absolute path your brief names.

## Report, checkpoint, finish

```text
herdr-superagent report --state progress --summary "…"       # at milestones
herdr-superagent report --state blocked --summary "…" --question "…"
herdr-superagent checkpoint <name> --summary "…"               # then END YOUR TURN
herdr-superagent memory search "…"                             # this project's past tickets
herdr-superagent memory add --title "<ticket> <role>: <goal>" --summary-file summary.md
herdr-superagent report --state ready --summary "…"            # ends the ticket (root) or your assignment (child)
```

## Delegate into a sibling pane, not inline

Spawn a child when the work is long-running, needs another role or model
tier, should run in parallel, lives in another repo, or should be watchable
by the person. A child runs in a new tab of the ticket's workspace, in the
same worktree, in your mode or a stricter one.

```text
herdr-superagent spawn --role reviewer --task "Review the diff on this branch for correctness and tests; report ready with findings"
herdr-superagent watch <assignment-id> --timeout 1800        # blocks until the child reports ready/failed/dropped
```

`spawn` prints the child's assignment id; `watch` prints its final report.
The child's final report also arrives in your pane as a message. Act on it:
fix what the reviewer found, then report or checkpoint.

Roles: `reviewer` (reads, does not change files), `builder` (implements a
bounded piece), `researcher`. Give the child a complete task in `--task`;
it does not see your conversation.

Never use your harness's own subagents (Claude's Task or Agent tool) to
split work: they are invisible to herdr and the person. `spawn` is the only
way to delegate.

## Checkpoints can be revised

If the scope changes while you wait at a checkpoint, redo the plan and run the
same `checkpoint <name> --summary "…"` again; the new summary replaces the
old one. `herdr-superagent retitle <ticket> "…"` renames a ticket whose title
no longer fits. Approval only arrives as a message that starts with
"Checkpoint '…' approved".

## Rules

- Never approve your own checkpoints; a person does.
- In autonomous mode, guard shims may stop `git push` to the default branch,
  force pushes, `gh pr merge`, `rm -r` outside the worktree and similar: they
  record a checkpoint. End your turn and wait.
- Prefer `herdr-superagent spawn` over herdr's own pane commands for
  delegation; herdr's skill still applies for reading other panes.
