# Security Policy

## Supported Versions

WorldZero is pre-implementation — there is no released version yet. Once there
is a first release, security fixes will target the latest code on `main`
until a stable release line is established.

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Instead, report it privately to the maintainers (see the repository's
GitHub Security Advisories tab, or contact a maintainer directly) with:

- A description of the vulnerability and its potential impact.
- Steps to reproduce (proof-of-concept code or commands are helpful).
- The WorldZero version/commit and deployment configuration you tested against.

We'll acknowledge your report as soon as we can and follow up with next
steps. Once a fix is available, we'll coordinate on disclosure timing and
credit you in the release notes if you'd like.

## Scope

There is no code yet, so this section is a placeholder. Once implementation
starts, expect this to call out the areas most relevant to a self-hosted
MMO backend specifically: the plugin sandbox boundary (WASM host functions),
the declared attribute schema validation path, and anything touching auth
or the transfer/gating system.
