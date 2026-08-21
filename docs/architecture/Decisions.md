# Decisions

Two layers, not one, and they're not interchangeable:

## The pre-code baseline: `docs/PROPOSAL.md`'s Decision Log

Everything decided during the design phase, before any Rust code existed, lives in [`docs/PROPOSAL.md`](../PROPOSAL.md)'s Decision Log — language, license, storage, transport, plugin architecture, and so on. That table is a frozen historical record of the initial design pass. It doesn't get new rows added casually going forward; it's the baseline everything else builds on.

## Going forward: one GitHub issue per decision, labeled `decision`

New architecture/design decisions made after initial commit are tracked as GitHub Issues, not as new rows bolted onto the proposal's table or a separate `DECISIONS.md` file. This scales better once there's real discussion, review, and history attached to a decision than a markdown file ever does — decisions become searchable, timestamped, linkable, and connected to the PRs/issues that came from them.

**The convention:**

- **Each decision is its own issue** — never just the `decision` label slapped onto whatever issue happened to contain the discussion.
- **Use this shape for the issue body:**

  ```
  Title: Use PostgreSQL as the primary database

  Labels: decision, crate: character   (pick the relevant crate label(s), or none if it's cross-cutting)

  Context:
  Why this decision needs making at all.

  Decision:
  What was actually decided.

  Why:
  - Reason
  - Reason
  - Reason

  Alternatives considered:
  - Option A
  - Option B

  Consequences:
  - What this commits us to
  - What gets harder or easier as a result

  Related:
  #<issue/PR>
  ```

- **Open + `decision` label = still being discussed. Closed + `decision` label = decided.** Close the issue the moment the decision is made — that state transition *is* the signal, not a comment saying "decided."
- **Never rewrite a past decision issue.** If a decision changes later, open a new issue and write `Supersedes #<old issue>` in it (and go back and note `Superseded by #<new issue>` on the old one). This preserves the history, which is the entire point of keeping a decision log in the first place — the old reasoning stays visible even after it's no longer current.

The whole log is then just this query, at any time: **`is:issue label:decision`**.

## When this stops being enough

A formal ADR directory (`docs/adr/0001-*.md`) becomes worth the overhead once decisions need to live and version alongside the code itself, get reviewed through PRs rather than issue comments, or the project needs to survive a migration away from GitHub. None of that applies yet — revisit if it starts to.
