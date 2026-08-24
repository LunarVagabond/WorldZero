# About WorldZero

## The problem

Building an MMO used to require a small studio's worth of backend engineering before a single player ever saw the game: authentication, character persistence, world simulation, netcode, sharding, chat, matchmaking — all before "is this game fun" could even be tested. State sync, authoritative movement validation, cross-shard chat, session management, character persistence: each one is deceptively hard, and getting any of them wrong produces the exploits and desync bugs that kill a young MMO's reputation permanently.

That cost is a large part of why the genre has gone quiet. Indie and solo developers now have the tools to build almost everything a game needs — engines, art pipelines, netcode libraries for session-based multiplayer — except the one layer that makes an MMO an MMO: a persistent, authoritative, many-player world that survives a restart. Without infrastructure that removes that cost, only well-funded studios can afford to attempt the genre, and most of the rest either give up partway through the backend or never start.

## Why this project exists

This is a personal one as much as a technical one. MMOs used to be a real category on a storefront — a shelf full of persistent worlds people actually lived in for years — and that's thinned out to a handful of aging giants and the occasional big-studio release. Not because the audience went away, but because the backend cost priced almost everyone else out of trying. WorldZero is a bet that if that cost gets removed, some of that shelf comes back — not from WorldZero itself (this isn't a game, and it never will be), but from whoever builds on it.

That's the whole point of releasing this as OSS rather than keeping it as a private tool: it's meant to be a contribution to games that don't exist yet, built by developers who'd rather spend their time on what makes their game distinctive than on reinventing realm topology. Every real system in this repo exists so someone else's dream MMO ships faster than it otherwise would have.

## What that means for how this project runs

Developers are understandably protective of owning the systems they build — that instinct is exactly why so many small teams end up hand-rolling their own auth, their own netcode, their own persistence layer, instead of reaching for something existing. WorldZero's answer to that instinct isn't "trust us, we've got it" — it's designing the core to be small and boring in the places that don't need to be creative (accounts, sharding, storage), and wide open in the one place that does (gameplay logic, entirely yours, via the plugin system — see [Design Principles](../PROPOSAL.md#design-principles-non-negotiables)).

That also means this project is only as good as the community actually building games on it. If WorldZero doesn't yet support something your game needs, that's expected at this stage, not a dead end — say so. A gap reported by someone actually trying to ship a game is worth more than a feature guessed at in the abstract, and closing that gap is exactly the kind of contribution this project is built to absorb. See [Contributing](../../.github/CONTRIBUTING.md) for how to raise it, or open a GitHub Discussion first if you just want to think it through.

## The bar

Not "a skilled team could use this" — several existing projects already clear that bar (see [Prior Art & Positioning](../PROPOSAL.md#prior-art--positioning)). The bar is a developer's gut reaction in the first few minutes of looking at this: *"thank goodness I don't have to build this part, I just need to do XYZ,"* not *"I could just do this myself."* See [The Developer Experience Bar](../PROPOSAL.md#the-developer-experience-bar) for what that demands concretely.

## What WorldZero is and isn't

**WorldZero is:** OSS (Apache-2.0), self-hostable, MMO-genre-specific (realms/shards/layers/character policy as first-class concepts), engine- and client-agnostic, and extensible through a sandboxed WASM plugin system.

**WorldZero is not:** a game engine, a rendering/asset pipeline, an authoring tool, or (at least initially) a hosted SaaS. See [What This Project Is Not](../PROPOSAL.md#what-this-project-is-not-non-goals) for the full list.
