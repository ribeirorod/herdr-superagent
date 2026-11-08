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
