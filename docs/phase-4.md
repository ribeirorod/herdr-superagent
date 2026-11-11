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

On 2026-11-11 the plugin ran inside a live herdr 0.9.5 on Linux (built from
source), with Claude Code 2.1 as the harness for the session, a worker and a
reviewer. Two tickets went through: one submitted from a shell, one typed to
the session.

| Part | Unit tests | End to end (fake herdr) | Live herdr 0.9.5 (Linux) |
|---|---|---|---|
| Socket discovery from a shell outside herdr | yes | - | yes |
| Launch in its own workspace and worktree, brief, plan checkpoint, approve | yes | yes | yes |
| Worker looks at branches, pull requests, your checkout, memory | - | brief text | yes: its plan listed each |
| Spawned reviewer: own tab, own brief, report back to the worker | yes | yes | yes |
| Workflow's required checkpoints, then `ready` ends the ticket | yes | yes | yes |
| Session start, routing brief, no permission prompts for the plugin's CLI | - | - | yes |
| A typed request: session submits and routes it with the repository | - | - | yes |
| Daemon tells the session: checkpoint, result; session does not approve | yes | yes | yes |
| `retitle` renames the workspace (`workspace.rename`) | yes | refusal path | yes |
| Daemon stop/start re-attaches to a live worker pane | - | - | yes |
| A rebuilt binary restarts daemon and cockpit | - | - | yes |
| A brief swallowed by a start-up screen is sent again | yes | - | yes |
| Cockpit in its pane; approve by key | yes | pty | yes |
| The skill is installed into the harness's skills directory | - | - | yes |
| `--disallowedTools Task,Agent`, `--allowedTools`, `--add-dir` accepted by Claude | - | - | yes |
| Summaries stored by the workers and found by `memory search` | yes | yes | yes |
| Guard shims and the one-time permit | yes | yes | no (Claude refuses its skip-permissions flag as root here) |
| Sources | yes | yes (`command`, `session`) | no |
| Importer on real transcripts, Ollama embedder | yes (synthetic) | yes (synthetic) | no |
| Calibration interview, `isolate_settings`, cockpit `o` (focus), mouse | partly | - | no |
| macOS, Codex and Pi as harnesses | CI (macOS) | CI (macOS) | no |
| Restart of the herdr server (`scripts/smoke-restart.sh`) | - | - | no |

Found and fixed during the live run:

- A fresh Claude shows a welcome screen that herdr reports as idle; the brief
  was typed into it and lost. The observer now sends the brief again when a
  worker sits idle without ever finishing a turn, and hands the ticket to the
  person after three tries.
- The session and the workers stopped at Claude's permission prompt for the
  plugin's own CLI and for reading their brief. Both are allowed at launch now
  (`--allowedTools` for the CLI, `--add-dir` for the state directory).
- A binary rebuilt from uncommitted changes kept its build id, so `ensure`
  left the old daemon running. The id now changes with the sources.
- The result message and the ticket line quoted the reviewer's report instead
  of the worker's own final one.
- The cockpit stayed on a finished ticket when a new one needed the person.
