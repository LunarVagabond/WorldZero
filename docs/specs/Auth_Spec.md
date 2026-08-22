# Auth Spec

Corresponds to [Auth Provider Architecture](../PROPOSAL.md#auth-provider-architecture) in the proposal.

## Provider trait

```rust
#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// Verifies provider-specific credentials and returns the account they
    /// identify. `Credentials` is a provider-agnostic bag (see below) — a
    /// provider rejects any shape it doesn't recognize with a normal error,
    /// not a panic.
    async fn verify_credentials(&self, credentials: &Credentials) -> Result<AccountId>;

    /// Issues a session for an already-verified account.
    async fn issue_session(&self, account_id: AccountId) -> Result<Session>;
}
```

Two required methods, nothing else — matches the proposal's "at minimum: credential verification and session issuance." No password hashing, OAuth redirect handling, or any other provider-specific mechanics live on the trait itself; that's entirely inside each implementor.

`Box<dyn AuthProvider>` is how the crate holds "whichever provider this deployment configured" — the trait takes no generic parameters and returns no `Self`, so it stays object-safe and swappable at runtime/config time (config picks a provider by name at startup), not just via compile-time generics.

### `Credentials`

A provider-agnostic bag, not a fixed struct with named fields — different providers need different shapes (username+password vs. an OAuth authorization code), and the trait can't hardcode one without leaking provider-specific concerns into itself:

```rust
pub struct Credentials(serde_json::Value);
```

A provider's `verify_credentials` deserializes the shape it expects out of the `Value` and returns a `common::Error` (not a panic) if the shape doesn't match what that provider needs.

### `Session`

```rust
pub struct Session {
    pub token: String,       // opaque, not a JWT — see "Session token format" below
    pub account_id: AccountId,
    pub expires_at: time::OffsetDateTime,
}
```

## Password hashing: Argon2id

The default provider hashes passwords with **Argon2id** (via the `argon2` crate), not bcrypt or a hand-rolled scheme:

- Winner of the 2015 Password Hashing Competition, and OWASP's current recommendation for new applications.
- Memory-hard by design — resists GPU/ASIC-accelerated cracking in a way bcrypt (fixed, small memory cost) doesn't as well.
- The "id" variant (vs. Argon2i/Argon2d) is the general-purpose recommendation: hybrid resistance to both side-channel and GPU cracking attacks.
- Rust has a mature, actively maintained implementation (`argon2` crate, part of the RustCrypto org) — no need to shell out or vendor a C library.

Passwords are never logged or stored in plaintext anywhere, including in error messages — a failed verification returns a generic "invalid credentials" error, not "password did not match hash for user X."

## Session token format

**Opaque random tokens, server-side session state in Redis — not a self-contained JWT.**

- A session token is 32 bytes of CSPRNG output, base64url-encoded (no padding). It carries no embedded data of its own.
- The token is a Redis key (`session:<token>`) mapping to the `AccountId` and issued-at time, with Redis's own `EXPIRE` as the mechanism for expiry — this is exactly the "ephemeral/hot storage" role Redis already has in the proposal's Technology Stack table (session cache), not a new pattern.
- **Expiry:** 24 hours from issuance, fixed per session (not sliding) for v0 — a session simply stops working 24h after login and the client re-authenticates. Sliding/refresh-token behavior is real but not needed for the Phase 1 vertical slice; revisit if a real deployment shows 24h is the wrong number or session churn from re-login becomes a UX problem worth solving.
- **Revocation** is a single Redis key delete — the reason an opaque server-side token was chosen over a JWT. A JWT would need either short expiry + refresh tokens (more moving parts) or a revocation-list side channel to support "log this session out now" (password change, admin action, ban) — both more machinery than this needs for v0, and self-hosters running one Redis instance already get this for free.

## Worked example: a second provider

To prove the interface holds up for more than the shipped default, sketch (not implemented) what an OAuth provider (e.g. Discord) would look like against the same trait:

```rust
pub struct DiscordProvider { /* client id/secret, http client, session issuer */ }

impl AuthProvider for DiscordProvider {
    async fn verify_credentials(&self, credentials: &Credentials) -> Result<AccountId> {
        // 1. Deserialize `{ "code": "...", "redirect_uri": "..." }` out of `credentials`.
        // 2. Exchange the code with Discord's token endpoint, fetch the Discord user id.
        // 3. Look up an account linked to (provider="discord", provider_user_id) —
        //    a small linking table, not a new concept in `account`'s own schema.
        // 4. First-time login: create the account + link row in the same transaction
        //    (OAuth has no separate "register" step the way username/password does —
        //    this is *why* registration isn't on the trait: it's not universal).
    }

    async fn issue_session(&self, account_id: AccountId) -> Result<Session> {
        // Identical mechanics to the default provider — session issuance doesn't
        // vary by provider, so this is expected to just delegate to the same
        // shared session-issuing helper the username/password provider uses.
    }
}
```

This confirms the trait is sufficient as-is: `Credentials` absorbs the shape difference (auth code vs. username/password), account creation-on-first-login is a provider-internal concern rather than a trait requirement, and `issue_session` is genuinely provider-independent — providers are expected to share one session-issuing implementation via composition (a `SessionManager` helper each provider holds), not by duplicating Redis logic per provider.

## Default provider: username/password

- `register(username, password) -> Result<AccountId>` — provider-specific, not on `AuthProvider` (registration isn't universal across providers, see above). Rejects a duplicate username with a specific "username already taken" error, not a generic failure.
- `verify_credentials` expects `{ "username": "...", "password": "..." }` in the `Credentials` bag; wrong password or nonexistent username both return the same generic "invalid credentials" error (not distinguishable to a caller — don't leak which one was wrong).
- `issue_session` delegates to the shared Redis-backed session issuer described above.
