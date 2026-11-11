# Backlog

Decided to do later. Each item says what is missing and the shape agreed so far.

## Task systems: Jira, Bitbucket, GitHub issues (2026-11-11)

Today a ticket that came from a source remembers the item's key and URL
internally and nothing else. Wanted: tickets linked both ways with the task
system a profile uses.

- **Links on the ticket**: a list of `{ system, key, url }`, filled from the
  source item, added by hand (`link <ticket> <url>`), shown in the ticket, the
  cockpit and the worker's brief (so branch and commits name the issue).
- **Create the external ticket** for a request that was typed, when asked.
- **Write back**, off by default and per profile: a comment on the linked
  issue at ticket start, plan approval and result, with the branch or pull
  request. Status transitions behind a second switch, per source.
- **Close the loop**: a merged pull request or closed issue ends the ticket,
  and the reverse.
- **Ask first**: a per-source setting so the superagent proposes each new item
  to the person, batched as a digest, instead of judging it alone.
- **First-class Jira and Bitbucket sources**, beside the generic `command`
  and `session` kinds.

Open decisions: whether the superagent may write to company trackers at all
(comments appear under the person's name), and whether access goes through
the harness's MCP servers and CLIs (no credentials stored) or a direct API.

## Smaller

- claude-mem as a read-only source queried with an explicit project (PLAN §7).
- A cheap-model summarising pass for imported sessions, optional.
- Changing the mode of a ticket that is already running.
