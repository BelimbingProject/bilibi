# Security facade

**Use it for:** existing callers only: an older facade over Tenancy, Session and Authz.

**Not for:** new code (use `base/tenancy`, `base/session` and `base/authz` directly).

`src/lib.rs` re-exports `Scope`, `Sessions` and the Authz restrictions and keeps `Security::guard` for its existing callers.
