# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Correct entries that work proves wrong; add new ones only by deliberate maintainer choice, never as routine task output.

## Media

- Store and serve files through `Media` (`base/media.store`, `README.md`), not hand-rolled `std::fs` writes or your own download handler: it owns the private-disk rules, libmagic type checks, tenant scoping and the signed route, and none of them can be bolted on afterwards.
- Build stream headers with `StreamFile::delivery` (`src/response.rs`); only raster images may render inline, so never set `content-type` from stored metadata yourself.
- A call that writes bytes takes a plain `&mut Client` because files and rows cannot share a rollback; use `register_original`/`register_derivative` inside your own `MutationTransaction` only for bytes you already placed.
- Private files for your records go through `upload_attachment`/`submit_attachments`/`download_attachment` (`src/attachments.rs`), never `register_original` with `NewAsset::private`, a stored asset id or your own download route: only the attachment path rechecks the tenant and your `SubjectOwner` (a `Subject` contribution, `src/owner.rs`) on every access and keeps submitted evidence immutable.
- A background job that receives files (mail import, connector) calls the same attachment API with its `SystemAuthentication::for_system` scope, never `register_original` or a person's scope; your `SubjectOwner` must refuse principals it does not expect (README, "Stored by a system principal").
- Store the attachment ULID (`public_id`), not an asset id; call `submit_attachments` inside your own transaction so a rollback undoes the binding, and send bytes with `StreamFile::delivery(true)`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
