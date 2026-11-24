# ADR 35: Server release proof follows installation trust

**Document Type:** Architecture Decision Record
**Status:** Accepted
**Agents:** codex/gpt-5 (author)
**Scope:** Proof accepted before a Bilibi server installs a release artifact
**Last Updated:** 2026-11-24

## Context

**An adopter chooses how to build and release its application; Bilibi verifies the result against what that installation was set up to trust.**

Bilibi currently accepts only GitHub artifact attestations from one repository, its `.github/workflows/release.yml` workflow and a protected setup tag. That proof works for public application repositories, but a private repository on an ordinary GitHub plan cannot persist the attestation. The build, boot and packaging can pass while every documented server install path still refuses the resulting archive. This makes a GitHub product feature and billing plan part of Bilibi's release trust boundary and excludes private customer compositions.

The replacement must keep the properties that matter: the proof names the exact archive, the operator's repository, the release workflow and the tag; verification finishes before any archive content is unpacked or executed; and an old valid release cannot be replayed as a normal update. The server may use its repository-scoped GitHub App to fetch a private release, but transport authentication is not release authenticity. The alternative proof must remain verifiable with no network.

Private repository and asset names are sensitive. A design that submits them to the public Sigstore Rekor log would disclose them outside the operator's repository, even if the archive itself stayed private.

This decision resolves [issue 886](https://github.com/BelimbingProject/bilibi/issues/886).

## Decision

**Each installation pins one release-proof policy before its first release, and every normal install must satisfy that policy without fallback.**

The supported policies are:

| Policy | Trust anchor | Use |
| --- | --- | --- |
| GitHub attestation | The repository's immutable GitHub ID, release workflow and setup-tag rule | Existing public build path |
| Operator signature | An Ed25519 public key, repository name and immutable ID, workflow identity and setup-tag floor | Private or public builds from any CI |

An installation configured for an operator signature does not fall back to a GitHub attestation when its signature is absent or invalid. An installation configured for GitHub attestations does not accept an operator signature until its owner deliberately replaces the policy. This makes the selected trust boundary explicit and prevents proof-method downgrade.

### Operator-signed proof

The release carries a sidecar DSSE envelope whose Ed25519 signature covers the envelope's payload type and exact proof-statement bytes. The statement has a Bilibi-specific payload type and a closed, versioned schema containing at least:

- the SHA-256 digest of the complete artifact;
- the repository name and immutable ID pinned by the installation;
- the workflow identity pinned by the installation;
- the exact `setup-vX.Y.Z` tag;
- the artifact kind and target; and
- the signing-key fingerprint.

The verifier rejects unknown or duplicate statement fields, a non-canonical setup tag, more than one subject, and more than one signature. It verifies the complete artifact's digest, the signature and every pinned identity before opening the archive. The signed statement is the proof; `release.json` and `SHA256SUMS` inside the archive remain consistency and content checks, not trust anchors.

The private key never goes to the server. The operator keeps it in the protected release workflow, a signing service or equivalent custody that represents the workflow named in the trust policy. The signature proves that this key signed the workflow claim; Bilibi cannot prove that a CI job really ran if the operator lets another process use the same private key. A separate release key with access limited to that workflow keeps this claim meaningful.

The proof envelope can travel beside a GitHub release asset fetched through the server's GitHub App, or beside an archive uploaded to Software. Signature verification uses only the artifact, envelope and locally pinned public key. It never contacts GitHub, Sigstore, Rekor or another transparency service and publishes no repository or asset name.

### Trust ownership

Initial server setup records the proof policy before the first release is admitted. For operator signatures it records the public key fingerprint, repository name and immutable ID, workflow and minimum setup tag. For GitHub attestations it records the repository ID, workflow and minimum setup tag. The initial bootstrap is the only time no Bilibi installation owner exists to authorize that write.

After bootstrap, only the installation owner may replace the policy, repository, workflow or public key, and every replacement requires fresh 2FA. There is no environment-variable override, automatic key discovery, repository-name lookup or ordinary settings-editor path. The change is audited and preserves the release high-water mark.

Software shows the current proof policy read-only: proof kind, repository, workflow, public-key fingerprint when applicable, minimum tag, highest admitted tag, and when and by whom the policy was last changed. The private key and repository access credential are never accepted or shown there.

Trust state and its high-water mark live outside release folders and database backups, so installing a release, switching back or restoring a backup cannot replace or rewind them. Release code may read the current record but may change it only through the owner-authorized, fresh-2FA operation.

### Replay and rollback

Normal installation accepts only a setup tag strictly newer than the installation's highest admitted tag. The installation records the admitted tag and artifact digest before the artifact can become a new independent install. A second request for the same tag and digest may only resume the same durable admission or report that the same artifact is already current; it may not create another install. The same tag with different bytes, the same or an older tag after another release was admitted, and a tag below the setup floor are refused before unpacking.

Undo remains the deliberate rollback door. It uses a release already admitted and retained on that server, requires the existing owner review and fresh 2FA, restores a backup when required, and never lowers the release high-water mark. A release process that needs corrected bytes publishes a new setup tag rather than moving or rebuilding an old tag.

## Options considered

| Option | Decision |
| --- | --- |
| Require GitHub artifact attestations for every release | Rejected. It makes private installation depend on the adopter's GitHub plan and keeps GitHub Actions as the only practical build path. |
| Export a GitHub attestation bundle and verify it offline | Rejected as the sole answer. It would improve offline verification only after GitHub produced and persisted the attestation, which is the unavailable feature in the reported private-repository case. |
| Use Sigstore keyless signing with public Rekor | Rejected for private builds. It can publish repository, workflow and artifact metadata to a public transparency log and retains an external identity and availability dependency. A private transparency service would add another operated security system. |
| Trust a checksum, GitHub release download, GitHub App or owner's 2FA approval | Rejected. These prove bytes arrived unchanged or that a person approved an action; they do not prove which release authority produced those bytes. |
| Pin an operator signing key and signed release statement | Accepted. It works offline, is independent of repository visibility, GitHub plan and CI vendor, and keeps trust under the installation owner. It adds key-custody and rotation duties. |

## Consequences

- Public application builds can keep the existing GitHub-attestation path.
- Private application builds can use the same GitHub App for listing and download while authenticating the artifact with an operator signature.
- Upload verification no longer needs network access under the operator-signature policy.
- A missing or invalid proof always fails closed; the server never chooses another proof method on the caller's behalf.
- Key loss or compromise requires an owner-authorized replacement with fresh 2FA; replacing a key does not reopen older tags.
- Operators must keep the private signing key outside Bilibi and restrict it to the release authority named in the policy.
- An operator signature provides no public transparency or third-party timestamp. The server's pinned key, immutable proof statement, audit history and replay record are the evidence Bilibi relies on.
- The implementation may replace the current release-trust storage and contracts directly. Bilibi has no production users, so it needs no compatibility path, backfill or dual-write period.

Runtime proof must demonstrate that a private-style build signed by the pinned operator key installs with signature verification offline. It must also demonstrate refusal, before unpacking, of a tampered artifact, a signature from another key, each wrong repository/workflow/tag binding, the same tag with different bytes, and an older previously valid release replayed after a newer one.
