# Documentation

The single source of truth for this project is **[PROPOSAL.md](PROPOSAL.md)** — read that first. Everything else in this folder is either a thin, focused extract of one piece of it, or will eventually hold real implementation-level content once there's code to document. If anything here ever contradicts the proposal, the proposal wins until the proposal itself is updated.

## Layout

- **[product/](product)** — the pitch, roadmap, and requirements: what WorldZero is, why it exists, and what "done" looks like
- **[architecture/](architecture)** — system-level architecture and service boundaries. Decisions live directly as GitHub issues labeled `decision`, not a file — see [`project-management/issue-workflow.md`](project-management/issue-workflow.md) for the convention, or query `is:issue label:decision` on the repo directly
- **[specs/](specs)** — one focused spec per subsystem (auth, realm/character policy, content manifest, networking, plugin API, data model, observability)
- **[developers/](developers)** — the onboarding path: getting started, day-to-day development, release process
- **[project-management/](project-management)** — how contributions, issues, and releases actually flow
