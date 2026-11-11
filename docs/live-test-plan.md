# Live test plan (for the agent on the machine with herdr)

Everything below has passed unit tests and an end-to-end run against a fake
herdr. None of it has run in a live herdr since phase 3. Work through it in
order; each step says what to do and what must be true. Report per step:
pass, or fail with the output and `herdr-superagent --profile test logs
--lines 200`. On any failure also attach `debug-bundle`.

## 0. Update

```bash
cd ~/src/herdr-superagent
git stash list && git status -sb          # fa06811 is local only: rebase it, do not drop it
git pull --rebase
cargo build --release && cargo test
rm -f bin/herdr-superagent && cp target/release/herdr-superagent bin/ && codesign --force --sign - bin/herdr-superagent
herdr-superagent --profile test ensure
```

- [ ] `cargo test` passes on macOS (86 unit, 5 end to end, about a minute).
- [ ] The daemon and the dashboard restart by themselves into the new build.
- [ ] If the rebase of fa06811 conflicts in `skills/superagent/calibration.md`
      or `src/cli.rs`, keep both intents and say what you resolved.

## 1. Names taken from documentation, never run

These are the riskiest lines: a wrong name fails silently or loudly.

- [ ] **`--disallowedTools Task,Agent`**: start the session, ask it to
      "use a subagent to list the files here". It must say it has none.
      If Claude rejects the flag at start, report the exact error and set
      `profile set harness_flags.session='{}' harness_flags.worker='{}'`.
- [ ] **`CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION=false`**: an idle worker pane
      shows no grey suggested reply.
- [ ] **`--setting-sources project,local`**: `profile set
      harness_flags.isolate_settings=true`, start a ticket; the worker does
      not use the global output style and the task-ledger hook does not run.
      Check login and MCP servers still work. Then set it back if they do not.
- [ ] **`workspace.rename`**: `retitle <id> "…"`; the workspace label in the
      sidebar changes. If not, the log has `workspace.rename not available`.
- [ ] **`agent.focus`**: in the cockpit press `o` on a ticket; herdr jumps to
      its workspace and pane.

## 2. Outside herdr

- [ ] In a terminal that herdr did not start: `herdr-superagent --profile
      test status` works. If it says herdr was not found, report where
      `herdr.sock` really is (`find ~ -name herdr.sock 2>/dev/null`).

## 3. Calibration hands over to routing

Use a fresh profile: `herdr-superagent start --profile gaptest`.

- [ ] The brief arrives within about 10 seconds of the harness being ready,
      not 90.
- [ ] Every command the session prints for you carries `--profile gaptest`.
- [ ] It asks about global settings (step 3c), memory (4b) and sources (4c).
- [ ] After `calibrated=true` a `[superagent daemon]` message arrives and the
      session reads the routing brief. Then type a request: it must `submit`
      and `route`, not investigate.

## 4. A feature ticket with a reviewer

In the `test` profile, with a repository listed under `repos`:
type "Add a --version flag to <repo name>" to the session.

- [ ] The ticket has the repository as `project` without you naming a path
      (`ticket <id>`), `pack: software-dev@1/feature`, checkpoints `plan`
      and `implemented`.
- [ ] The worker's brief has "Look at all of it before you plan", and its
      plan says which branches and pull requests it looked at.
- [ ] At the plan checkpoint the session tells you, unasked, in one or two
      lines. It does not approve.
- [ ] `note <id> "only the CLI, not the library"`: the worker posts `plan`
      again and `ticket <id>` shows the new summary.
- [ ] `request-changes <id> "…"` then `approve <id>` both reach the worker.
- [ ] The worker runs `spawn --role reviewer`: a tab `reviewer` appears in
      the same workspace, on the review model, **and starts working on its
      own** (it never received its brief before this build).
- [ ] The reviewer's report arrives in the worker's pane; the reviewer tab
      title ends with `ready`.
- [ ] `report --state ready` is refused until `implemented` is approved.
- [ ] After the end the session tells you the result with the branch.
- [ ] `events.jsonl` of the ticket (state repo) has reports, the note, the
      spawn and the change request.

## 5. Guard shims

`submit "Push a one-line README fix straight to main" --workflow general
--mode autonomous --project <scratch repo with a remote>`

- [ ] The worker's `git push origin main` does not run; the ticket shows
      checkpoint `guard-git`; the pane does not hang or spawn processes
      without end (`ps aux | grep -c "guard git"` stays small).
- [ ] After `approve`, the same command runs once; a second identical push
      is held again.
- [ ] `git status` and `git push -u origin sa/<id>` pass straight through.

## 6. Cockpit

- [ ] The right pane is the interactive list; no flicker; fits a narrow split.
- [ ] `j`/`k`, mouse click and wheel move the selection; the lower panel
      follows.
- [ ] `a` asks, `y` approves; `c` and `n` take text; `t` retitles; `s` asks
      then stops; `?` shows keys; Ctrl-C does not close the pane.
- [ ] Kill the daemon pane's process: the header says the daemon is not
      running; `ensure` brings it back and the list returns.

## 7. Sources

```bash
herdr-superagent --profile test profile set 'sources[+]={ id = "issues", kind = "github", repo = "<owner/name>", query = "assignee:@me", project = "<repo path>", workflow = "bugfix" }' --yes
herdr-superagent --profile test sources poll && herdr-superagent --profile test sources
```

- [ ] `sources` shows `via gh issue list …` and `ok … N items`.
- [ ] The items are in `candidates` marked `from source`; the session is
      told at once and routes or ignores each with `--rule source:issues`.
- [ ] A second poll adds nothing.
- [ ] Add a `kind = "session"` source with `instructions`: the session is
      asked to look through its MCP servers and submits with `--source`.

## 8. Memory

```bash
herdr-superagent --profile test memory import --dry-run
```

- [ ] The list shows real sessions of the profile's repositories with
      sensible titles, for Claude and, if present, Codex and Pi. Report the
      counts per skip reason. Zero found for a harness you do use means its
      transcript format differs: attach the first 5 lines of one file with
      contents redacted.
- [ ] `memory import`, then `memory show <id>` of one entry: goal, follow-ups,
      outcome, files. No secrets (grep for a token you know is in a session).
- [ ] Run it again: 0 imported, all unchanged.
- [ ] `profile set memory.semantic=true`; `memory search "<a word form not
      in any entry>" --project <path>` returns the related entry.
- [ ] The next ticket in that repository has imported sessions in the
      "Memory" section of its brief.
- [ ] With a state repo: `sync`, then check `memory/<project>/sessions/*.md`
      exist and hold no transcript.

## 9. Tidy and restart

- [ ] `tidy --older-than-hours 0`: ended tickets' workspaces close, clean
      worktrees are removed (`git worktree list`), branches remain, a dirty
      worktree is kept and named.
- [ ] `cleanup` with a closed pane in between does not error.
- [ ] `scripts/smoke-restart.sh` (never run anywhere): after a herdr server
      restart the daemon pane comes back and running workers are re-attached.
