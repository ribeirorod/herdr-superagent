# Prototype concepts left out (for now)

The engine keeps the prototype's model where herdr needs it and drops what
only a chat-and-GitHub bot needs. Like the prototype's own `docs/001`, this
table says what was rejected and why, so it is not re-litigated by accident.

| Prototype concept | Decision | Why |
|---|---|---|
| **Threads** (consistency boundary per conversation) | replaced by **Request** | You type requests; there is no conversation to debounce. A request evaluates alone. |
| **Subscriptions** and fan-out to tickets | dropped | Tickets receive requests only when the superagent attaches them. |
| **Correlation** across threads | dropped | No threads. |
| **Channels**, **Replies**, the outbox | dropped | Results go to the superagent session and the cockpit, not to a chat. Sources (phase 4) get their own reply port. |
| **Admission table**, bot lists, `observe` mode | deferred to phase 4 | They gate sources (Jira, GitHub, mail). Until then the gate is: your requests are explicit, a source's are inferred. |
| **Follow-up owners**, hand-back, `acked` | simplified | A human request that an ended ticket never received goes back to evaluation (`let_go`); workers get `handback` in phase 3. |
| **Follows** / `followsDone` (PR merged ends the ticket) | deferred to phase 4 | Needs a source that reports thread closes. |
| **Trigger read tracking** (`readAt`, "took into a step") | dropped | herdr does not expose a harness's step boundaries; the turn-end fallback uses `completion_seq` instead. |
| **Evals as batches** per thread | one evaluation per request | Same deadline, requeue and `unavailable` rules; no batch. |
| **Feishu**, GitHub watcher, web panel, locales | dropped | herdr is the UI; sources are packs. |
| **DSH sessions** as the executor | replaced by herdr panes | `worktree.create` / `agent.start` / `agent.prompt` in phase 2. |

Kept, with the prototype's tests as the spec: the Ticket / Mandate /
Assignment / Report model, the stage table, held automatic inputs, retry,
restart with a recovery note, resume under the same mandate, the question
semantics of a report, decision idempotence, deadline and requeue, pause, the
durable effect queue, and the store design (JSON aggregates in SQLite with a
few indexed columns).

## Decided later

| Question | Decision | Why |
|---|---|---|
| **sqlite-vec** for semantic memory | not used; vectors are blobs compared in process | The extension would have to be compiled and shipped per platform next to a bundled SQLite. A project holds hundreds to a few thousand entries, where a linear cosine scan takes well under a millisecond. Revisit above about 50,000 entries per project. |
| **A hosted embedding API** | refused by validation | Embedding sends the text. `memory.endpoint` must be local. |
| **An LLM to summarise imported sessions** | extractive only | It would cost tokens per session and could invent. The first request, the follow-ups, the last answer and the changed files are what a later ticket searches for. |
| **The harness's own subagents** (Claude's Task/Agent tool) | off for the session and for workers | They are invisible to herdr, the dashboard and the person; in the session they turn routing into doing the work. `harness_flags.session` and `.worker` can re-enable them. |
| **Auto-accepting source items** | never | An issue or a mail is outside text. It becomes work only when routing names the source as its rule; the scorer fallback applies to the person's own requests only. |
| **Workflow steps driven by the engine** (one assignment per step) | steps are the root worker's plan | The worker keeps its context across steps and spawns only the steps of another role. The engine enforces what must not be skipped: the required checkpoints. |
| **Removing dirty worktrees on tidy** | kept | `git worktree remove` without force; unpushed or uncommitted work is never deleted for tidiness. |
| **Rewriting `~/.claude/settings.json`** to keep global styles and hooks out | not done | `--setting-sources project,local` per launch does it without touching the person's files. |
| **claude-mem as the memory system** (2026-11-11) | a retrieval backend you can switch to, never a replacement for the record | `memory.backend = "claude-mem"` leaves retrieval to claude-mem in the worker's harness: the brief carries no memory of its own, names the project to search, and worktrees are keyed by their repository. Ticket summaries are still written, because they are the ticket's record and what moves between machines, which claude-mem does not do. Running both for the same worker is the one setup to avoid: two memories in the context and an observer call per tool event. With `builtin`, keep claude-mem out of worker panes (`harness_flags.isolate_settings`, or `harness_flags.env = { CLAUDE_MEM_OBSERVER_ENABLED = "false" }`). |
| **Importing sessions for a project that has curated notes** | notes win | A note is one checked fact; an imported session is raw extraction. Beside each other they compete in search and the weaker one adds noise. `memory.notes` removes the project's imported sessions and keeps the importer off it. |
