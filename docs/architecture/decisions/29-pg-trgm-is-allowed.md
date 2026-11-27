# PostgreSQL's `pg_trgm` extension is allowed

**Document Type:** Architecture decision record
**Status:** Accepted
**Agents:** amp/opus-5.5 (author)
**Scope:** Which PostgreSQL extensions a module's migration may enable
**Last Updated:** 2026-11-15

## Context

**Some reads match part of a value, and a btree index cannot serve them.** Core Consent's Suppressions list searches `lower(address) LIKE '%…%'`, and the privacy locate of every store matches a phone's digits anywhere in a stored number (`base/personal_data`, `Subject::patterns`). Without an index, each read scans the whole table, and Consent's dispatch log grows by one row per elective send (#604). [ADR 06](./06-query-time-record-search.md) chose built-in full-text search and said that `pg_trgm`, the Platform's first extension, would need its own decision.

## Decision

**A module's migration may enable `pg_trgm` with `CREATE EXTENSION IF NOT EXISTS pg_trgm` and index with `gin_trgm_ops` where a read matches inside a value.** The owner agreed on 2026-11-15.

- `pg_trgm` ships in PostgreSQL's contrib, is trusted since PostgreSQL 13 (a database owner may create it without superuser) and every managed PostgreSQL offers it.
- `IF NOT EXISTS` lets several modules enable it; none drops it.
- Write the index to the expression the query already uses, so the query reads it without change.
- Any other extension still needs its own decision.

## Rejected alternatives

- **A digits-only generated column with a btree index:** a btree serves a prefix, not `%digits%`, so the locate would still scan.
- **Changing the match to prefixes or exact values:** it would miss a number stored with a country code the requester did not type, which privacy requests must find.

## Consequences

- Each trigram index costs write time and space; add one only for a read that matches inside a value.
- A trigram index does not include the tenant, so a common pattern is matched across tenants before the tenant predicate filters it, as ADR 06 says of its GIN index.
- A pattern shorter than three characters gains nothing from a trigram index.

## Status

Used by:

- Core Consent: the privacy locate's address match and the Suppressions search (`apps/core/consent/migrations/005-search-and-retention.sql`).
- Core Person, Core Affiliation and Core Company: Contacts and Companies search (`apps/core/person/migrations/008-search-trigram.sql`, `apps/core/affiliation/migrations/004-search-trigram.sql`, `apps/core/company/migrations/013-search-trigram.sql`, PR #711).
- Core Document: title matches in Base Search for typos and parts of words (`apps/core/document/migrations/015-title-trigrams.sql`; see [ADR 06](./06-query-time-record-search.md)).
