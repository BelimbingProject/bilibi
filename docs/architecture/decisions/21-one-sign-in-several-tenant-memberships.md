# 21 One portable identity, tenant-bound memberships and SSO

**Document Type:** Architecture decision record
**Status:** Accepted; implemented
**Agents:** codex/gpt-5
**Scope:** Staff identity across tenant boundaries
**Last Updated:** 2026-11-22

## Decision

**One installation-owned portable identity may have one separate staff membership in each tenant, while every customer SSO proof remains bound to the tenant, provider and company that issued it.**

- `user_identities` holds the installation-wide sign-in address, password, verification and second-factor enrollment. It has no customer-administered home membership.
- A tenant-owned `users` row is one membership and remains the actor in that tenant. Customer administrators manage its Person link, desks, roles, status and removal, never the shared identity or credentials.
- A new portable identity is activated through a one-time invitation where the holder chooses the password. A customer administrator cannot choose or replace it, change the shared email, reset shared 2FA or delete the identity. Tenant privacy and retention affect membership data only.
- Every pending and admitted staff sign-in records immutable authentication authority: `portable`, or `tenant_sso` with its tenant, provider and company. Rotation and second-factor completion preserve it; ordinary application code cannot promote it.
- Portable authority may enter another membership, subject to that target's existing factor and company-SSO policy. Tenant SSO authority may operate only in its issuing tenant.
- Selecting another tenant from a tenant-SSO session starts target-bound reauthentication. The target accepts portable proof or its own enabled SSO, then replaces the old session. An impersonated session cannot enter another tenant.
- Removing any membership removes only that membership. Shared identity deletion, recovery and final retention belong to the authenticated holder and installation operator.
- Customer portal accounts remain tenant-bound identities and never become portable staff memberships.

The dedicated first-owner invitation remains defence in depth. An identity first created through SSO can establish portable proof through verified-mail recovery before entering a tenant with no suitable SSO.

## Why

Tenant authority assumes that the actor account belongs to the tenant in every scoped foreign key, authorization decision, directory lookup and audit row. Separate membership rows preserve that boundary while one portable identity avoids duplicate passwords and authenticators for people who work in several customer tenants.

Giving one membership's customer administrator control of the shared credential root would make that customer an undeclared trust root for every other membership. Likewise, accepting tenant A's SSO in tenant B would let A's administrator cross a customer boundary. Durable proof provenance and target reauthentication place the visible friction exactly at that boundary.

## Rejected alternatives

- **Home-tenant credential administration.** Choosing one customer as the shared identity's authority does not isolate customers; it lets that customer take over or destroy every membership.
- **Target policy without removing customer credential control.** Provenance alone is incomplete while a customer can replace the portable password, email or factor.
- **One credential or identity per tenant.** This restores duplicate passwords and authenticator entries, makes email sign-in ambiguous and abandons one-sign-in switching.
- **Cross-tenant grants on one account.** A foreign account would become an actor in a tenant that does not own it, breaking scoped references, directory naming and audit attribution.
- **Trust groups between tenants.** They expose customer relationships and add a second cross-customer trust model without being needed for the secure default.
- **One tenant per browser tab.** This would put tenant identity into every route, live socket, download and background operation, and allow replay of record addresses under the wrong tenant.

## Consequences

- Credential changes revoke the portable identity's sessions across memberships. Membership removal and customer erasure never mutate the shared identity.
- Password, SSO and pending-factor admission must pass and preserve an explicit `AuthenticationAuthority`; custom sign-in adapters never insert or rewrite session rows directly.
- Identity-provider extensions return authenticated external identities as before, but Core User always marks their browser proof tenant-bound. An extension cannot claim portable authority or switch tenants.
- SCIM `userName` and contact facts are tenant provisioning data, not authority to change the installation-wide sign-in address.
- Fresh databases apply `base-session-011-authentication-authority` and `core-user-033-portable-identity-authority`. Development databases created with the former home-authority schema are disposable and must be recreated; there is no conversion or compatibility path.
