# Phases 4 to 6 and the live-test gaps

What was added after phase 3, and how far each part has been run.

## The gaps a live round found

| Gap | What changed |
|---|---|
| The CLI needs `HERDR_SOCKET_PATH` | Outside a pane it looks for herdr's socket in herdr's config and runtime directories and uses the first one that answers. |
| The session never leaves calibration | When `calibrated` turns true the daemon sends the session the routing brief. |
| "Don't do the work yourself" is only an instruction | The session starts without the harness's subagents (`harness_flags.session`). |
| Workers look at one branch | The brief has a section on every branch, open pull requests, uncommitted changes in your checkout, and memory. |
| A ticket for a repo is created without it | A request that names a profile repository gets it as its project; `route accept --project`; tickets without a repository are told every checkout is read-only. |
| A plan cannot be revised at its checkpoint | The worker posts the same checkpoint again; `request-changes` sends it back. |
| A ticket cannot be renamed | `retitle`; the workspace label and the record follow. |
| The history is thin | Reports, notes, spawns, change requests and renames are in `events.jsonl`. |
| Workers split work out of sight | Workers start without the harness's subagents (`harness_flags.worker`); `spawn` is the only way. |
| The skill may not be installed | `start` writes the skill into each harness's own skills directory; no `npx`. |
| Global settings leak in | `harness_flags.isolate_settings`, a calibration question. |
| The first prompt arrives late | No blind wait: the prompt is tried at once and retried while the harness boots. |
| A suggested reply looks typed | Prompt suggestions are switched off in session and worker panes; briefs say approval only arrives as a message. |
| Closing a repo workspace needs `--group` | herdr's own rule for your repo workspace. `tidy` closes the ended tickets' workspaces and removes their clean worktrees first, which lifts it. |
| `cleanup` fails on a stale pane | Stale panes and workspaces are skipped. |

Three defects surfaced while writing the end-to-end test and are fixed: a
spawned child never received its brief; the guard shims called themselves
without end; an approved guard checkpoint held the same command back again.

## Phase 4: packs and sources

- `packs/` holds the built-in packs; `src/packs.rs` loads them and the
  profile's own. A workflow gives the root role, the steps, spawned roles and
  required checkpoints. The ticket records `pack` and `required_checkpoints`.
- The keyword scorer routes when `routing = "never"`, when no session exists,
  or after the session let a candidate lapse twice.
- `src/sources.rs`: `github`, `command`, `session`. A missing program falls
  back to the session. Items are inferred requests: accepted only with
  `--rule source:<id>`.

## Phase 5: memory

- `src/importer.rs`: Claude Code, Codex and Pi transcripts into entries
  (`memory import`, `--dry-run`, `--project`, `--all`, `--since-days`).
- `src/embed.rs`: hybrid search (full text and vectors, reciprocal rank
  fusion). Embedders: `hash` (built in), `ollama`, `command`; local only.

## Phase 6: cockpit

- `src/cockpit.rs`: ratatui. Sections *needs you*, *to route*, *running*,
  *queued*, *done today*; the selection's checkpoint, question, report,
  workflow and branch below; keys and mouse for approve, changes, note,
  retitle, retry, stop, open; accept or ignore a request to route.
- `dashboard --plain` and `--once` keep the text list.

## What has been run, and where

| Part | Unit tests | End to end (fake herdr) | Live herdr |
|---|---|---|---|
| Launch, brief, checkpoints, approve | yes | yes | yes (phases 2 and 3) |
| Revise, request-changes, retitle | yes | yes | no |
| Spawned reviewer, child brief and report | yes | yes | no |
| Guard shims and the one-time permit | yes | yes | no |
| Session messages and the routing switch | yes | yes | no |
| Packs, scorer, required checkpoints | yes | yes | no |
| Sources | yes | yes (`command`, `session`) | no (`github` needs `gh`) |
| Importer and semantic search | yes | yes | no (run on synthetic transcripts) |
| Cockpit | yes | in a pty against a real daemon | no |
| `workspace.rename`, `agent.focus`, `--disallowedTools` | - | - | no: names taken from herdr's and Claude's docs |
| Restart smoke test (`scripts/smoke-restart.sh`) | - | - | never run |

A herdr binary cannot be built in the environment this was written in, so
everything in the last column is for the next live round.
