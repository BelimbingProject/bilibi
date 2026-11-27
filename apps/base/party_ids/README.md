# Party IDs

**Use it for:** the typed IDs of people, organisations and affiliations passed between modules.

**Not for:** authority: an ID proves nothing until its owner resolves it in a sealed `Scope`.

Shared wire identities only (`src/lib.rs`). An ID is never tenant membership or authorization proof; the owning Core provider resolves it in a freshly authorized scope (`apps/base/tenancy/README.md`).
