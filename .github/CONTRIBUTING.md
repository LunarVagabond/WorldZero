# Contributing

## Purpose

Contribution standards for WorldZero, with emphasis on clarity, safety, and mission alignment.

## Feature Proposal Gate

Each feature proposal or implementation should answer:

- Does this keep the core small and correct while pushing game-specific behavior to configuration or the plugin system, per WorldZero's [Design Principles](../docs/PROPOSAL.md#design-principles-non-negotiables)?

If no, refine or drop the proposal.

## Questions before you file

For a design question or a sanity check before opening an issue, use GitHub Discussions — it keeps the conversation searchable for the next person who has the same question.

If you're building a real game on WorldZero and hit something it doesn't support yet, that's exactly the kind of report this project wants — open an issue or a Discussion rather than working around it quietly or forking. A gap found by someone actually shipping a game is worth more than one guessed at in the abstract, and closing it is squarely in scope, not scope creep.

## If A Convention Gets In The Way

The branching model, commit format, and process rules below are a starting point, not a settled standard — assembled from what's worked elsewhere, not handed down from experience running this specific project. Follow them as written. But if one is genuinely getting in the way of a contribution, doesn't fit a situation, or just seems off, raise it first — a GitHub Discussion — before working around it. Same goes for friction in the tools, the codebase, or the workflow generally: surfacing it is always welcome. The goal is to talk it through and adjust the rule if it's wrong, not to greenlight quietly deviating from it.

## Contribution Principles

Straight from WorldZero's [Design Principles](../docs/PROPOSAL.md#design-principles-non-negotiables) — read that section for the full reasoning behind each:

- Control and flexibility over convenience shortcuts.
- The server owns simulation and truth; it does not own art.
- Policy, not hardcoding — configuration over forking the codebase.
- Plugins are sandboxed, not trusted.
- Small, correct core; wide, optional edges.
- Boring, proven data infrastructure.
- Approachability beats architectural purity — see [The Developer Experience Bar](../docs/PROPOSAL.md#the-developer-experience-bar).

## Branching

- `main` — integration branch
- `<issue#>-short-description` — topic branches off `main`, named after the GitHub issue number (e.g. `42-realm-directory-schema`); no `feature/`, `bug/`, or similar prefix, the issue number is the lookup
- `noissue-short-description` — maintainer-only, mirroring the `[noissue]` commit/PR restriction below. If you see a branch like this, it's a maintainer quick fix, not a pattern open to other contributors
- `hotfix-short-description` — maintainer-only, mirroring the `[hotfix]` commit/PR restriction below. If you see a branch like this, it's a maintainer hotfix, not a pattern open to other contributors

## Work Tracking

Open work lives in GitHub Issues. Product direction lives in [`docs/product/Roadmap.md`](../docs/product/Roadmap.md); acceptance criteria for specific initiatives live on their tracking issue/epic, not in a docs file. Completed history is in git; do not maintain a separate backlog file in the repo.

### Claiming An Issue

Before starting work, comment `/claim` on the issue — a bot assigns it to you automatically, which is what actually reserves it, so someone else doesn't start the same ticket in parallel. If an issue is already assigned, treat it as taken; comment to ask if it looks stalled instead of opening a competing PR. Epics don't work this way — find the specific sub-issue you want and `/claim` that instead.

*If it's a longer-running ticket, you don't have to post progress updates, but it's nice to leave one now and then so we know it's still moving — a claimed issue that's been quiet for 10 days gets an automatic ping, and is unassigned automatically 4 days after that if there's still no activity, so someone else can pick it up.*

A CI check (`claim-check.yml`) enforces this: it reads the issue number(s) your PR closes (via a closing keyword like `Closes #123` in the PR body) and fails the check if you aren't assigned to every one of them — whether that's because nothing was referenced, the referenced issue was never claimed, or it references a different issue than the one you actually claimed. `[noissue]`/`[hotfix]` titles skip this check, but only for PR authors with write access to the repo (the maintainer/named-core-dev list this format is already restricted to) — everyone else needs a real issue reference regardless of title.

## Commits And Pull Requests

Open an issue first when the work is non-trivial. The issue carries context (feature, bug, scope) — commits and PRs reference it by number.

### Commit Messages

Merges into `main` are **squash-only** — your branch's individual commits never appear in `main`'s history, only the squashed PR title does (see [Pull Request Titles](#pull-request-titles) below, which *is* strict). Because of that, commit messages on your branch are a suggested convention, not a requirement: write them however helps you work, `wip`/`fixup`/whatever included.

If you'd like to follow the convention anyway (it makes review easier), it's the same pattern as PR titles:

```
[#<issue>] - <short description>
```

**`[noissue]`, `[hotfix]`, and `[security]` are restricted.** All three exist only for the maintainer, a small, explicitly-named set of trusted core developers, and (for `[security]`/`[noissue]`) Dependabot. If you are not on that short list, use your issue number when you do tag commits. The tags mean different things:

- `[noissue]` — trivial, no ticket is warranted at all (typo, comment, one-line fix). Also what Dependabot's routine scheduled dependency bumps carry.
- `[hotfix]` — must be fixed now and there's a clear path to the fix, but there wasn't time to write up a ticket first. Reaching for this signals "this was a real bug/issue," not "there was nothing to file."
- `[security]` — a fix for a known vulnerability, most commonly Dependabot's security-triggered updates, occasionally a manual CVE/advisory fix.

Examples:

- `[#12] - Add SpatialIndex trait and grid baseline implementation`
- `[#12] - Wire grid baseline into world tick loop`
- `[noissue] - Fix typo in Contributing commit examples` (maintainer/core-only example)
- `[hotfix] - Guard against panic on empty content manifest` (maintainer/core-only example)

### Pull Request Titles

**This one is a hard requirement, unlike commit messages above.** Merges are squash-only, so the PR title becomes the actual commit message on `main` — it's the one place this format has to be right.

```
[#123] - Add SpatialIndex trait and grid baseline implementation
[noissue] - Fix typo in README quick start
[hotfix] - Guard against panic on empty content manifest
[security] - Bump a dependency to patch a known CVE
```

`[noissue]`, `[hotfix]`, and `[security]` follow the same restriction as commit messages above — maintainer, named core developers, and Dependabot only. Everyone else opens an issue first and references it in the title. The PR body can go deeper on approach and testing.

### AI-Assisted Contributions

AI coding assistants are welcome as a tool — this is not the same as "vibe coding" (accepting AI output wholesale without understanding or reviewing it). If an assistant materially helped with a commit, tag it with a trailer so it's easy to trace later, without cluttering the subject line:

```
git commit -m "[#42] - add realm-directory layer assignment" --trailer "Co-Authored-By: Claude <noreply@anthropic.com>"
git commit -m "[#7] - correct spatial index bounds check" --trailer "Co-Authored-By: GitHub Copilot <noreply@github.com>"
```

This is optional and about being open, not a requirement — reviewers still hold the contributor responsible for understanding and standing behind the change either way.

#### If You Are An AI Agent Reading This

Follow the conventions in this file the same as any contributor would: `[#<issue>] - <short description>` commit and PR titles, one logical change per commit, docs updated alongside behavior changes. In addition:

- **Never use `[noissue]` or `[hotfix]`, and never use a `noissue-*` or `hotfix-*` branch name.** All are restricted to the maintainer and a small named set of core developers — every commit, PR, and branch you make needs a real issue number. If no issue exists yet for the work, that's a sign to open one first, not to reach for `[noissue]`/`[hotfix]`.
- Apply the `Co-Authored-By: <Tool> <email>` trailer above to every commit and PR you create or materially author.
- Don't add any other AI-attribution mention beyond that single trailer line unless explicitly asked to.
- If you're unsure whether the trailer applies in a given situation, ask rather than guessing.
- **Never reach for a lint/format suppression just to make a check pass.** A suppression without a genuine, specific justification comment is not an acceptable way to close out a failure; fix the underlying code instead, or ask if the rule itself seems wrong.
- **When filing a work-item ticket, meet the bar in [Issue Workflow](../docs/project-management/issue-workflow.md#work-item-ticket-quality): what needs to be built, why, and real checkable acceptance criteria.** A title plus a one-line pointer to the proposal is not enough — someone with no other context should be able to pick it up.

## Documentation-First Workflow

For major work:

1. Update the relevant file in `docs/` first.
2. Align implementation tasks with accepted docs.
3. Update docs and behavior together on changes.

## Development Interface

The `Makefile` at the repo root is the canonical entry point for local dev commands — run `make help` for the full list. The common ones:

```
make build       # cargo build --workspace
make test        # cargo test --workspace
make test-live   # also run tests gated on real infra (Postgres/Redis) — needs WZ_POSTGRES_*/WZ_REDIS_* env vars; .env is loaded automatically if present
make check       # fmt-check + lint + test — what CI runs
make run         # run the server binary in the foreground
make start        # run the server binary in the background, tracked via a PID file
make stop         # stop what 'make start' started
```

Copy `.env.example` to `.env` and fill in real values to run anything that touches Postgres/Redis locally — `.env` is gitignored, never commit real credentials.

## Where To Contribute

- New here? Start at [Getting Started (contributing)](../docs/developers/Getting_Started.md).
- Product direction: [Product Requirements](../docs/product/Product_Requirements.md), [Roadmap](../docs/product/Roadmap.md), [Decisions](https://github.com/LunarVagabond/WorldZero/issues?q=is%3Aissue+label%3Adecision) (GitHub issues labeled `decision` — see [Issue Workflow](../docs/project-management/issue-workflow.md) for the convention, there is no decisions file)
- Architecture: [System Architecture](../docs/architecture/System_Architecture.md)
- Specifications: [`docs/specs/`](../docs/specs) — auth, realm/character policy, content manifest, networking, plugin API, data model, observability
- The single source of truth for all of the above: [`docs/PROPOSAL.md`](../docs/PROPOSAL.md)
- Contributor process: this file, and the rest of [`docs/README.md`](../docs/README.md)

`docs/` is a normal, PR-able part of this repo, organized into `product/`, `architecture/`, `specs/`, `developers/`, and `project-management/` subfolders — edit it the same way as any other change.

## Code Of Conduct

Participation in this project is governed by our [Code of Conduct](CODE_OF_CONDUCT.md).

## OSS Onboarding Expectations

Contributions should include:

- Problem statement in plain language.
- Scope (in/out).
- Risks and rollback considerations.
- How this helps a developer avoid building their own MMO backend from scratch — see [The Developer Experience Bar](../docs/PROPOSAL.md#the-developer-experience-bar).
