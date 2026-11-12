//! Core Portal (issue 137, ADR 16): each tenant's customer portal
//! site, the invitations that turn a Person into a customer account, the
//! staff page that manages both, and the portal's own home and account pages.
//! The host owns the portal's credential pages and asks this facade which
//! tenant an address names. Contract:
//! `README.md`.
mod delivery;
mod facade;
mod invitation_privacy;
mod operations;
mod panel;
mod portal;
mod privacy;
mod registration;
mod registration_limits;
mod web;

pub use bilibi_user::CustomerAccount;
pub use facade::{
    address_shaped, AcceptError, Checked, FieldError, Invitation, InvitationInput, LogoChange,
    OpenInvitation, PortalService, Site, SiteInput, Status, TITLE_MAX,
};
pub use registration::{RegisterError, Registration, RegistrationStatus, COMPANY_MAX};

use bilibi_authz::{CapabilityDefinition, RoleDefinition, VocabularyContribution, VOCABULARY};
use bilibi_menu::{MenuItem, Requirement, PORTAL_ROOT};
use bilibi_module_registry::{
    contributions::{
        Access, CredentialPath, Embed, EmbedContext, Method, PageWrites, Pending, Route,
        CREDENTIAL_PATHS, EMBEDS, PAGE_WRITES, ROUTES,
    },
    Contribution, Layer, Module, Provenance, Service,
};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, ValueType, DEFINITIONS};
use bilibi_tenancy::{PortalScope, Scope};
use serde_json::json;
use std::sync::Arc;

use registration_limits::{
    PENDING_DEFAULT, PENDING_KEY, PORTAL_DAY_DEFAULT, PORTAL_DAY_KEY, PORTAL_MINUTE_DEFAULT,
    PORTAL_MINUTE_KEY, RECIPIENT_DAY_DEFAULT, RECIPIENT_DAY_KEY, RECIPIENT_MINUTE_DEFAULT,
    RECIPIENT_MINUTE_KEY, SENDER_DAY_DEFAULT, SENDER_DAY_KEY, SENDER_MINUTE_DEFAULT,
    SENDER_MINUTE_KEY,
};

/// The registry service of the facade.
pub const SERVICE: &str = "core/portal.access";
/// The staff page.
const PAGES: &str = "core/portal.pages";
/// The portal home and account pages.
const PORTAL_PAGES: &str = "core/portal.portal";

/// See the portal site and the invitations.
pub const VIEW: &str = "admin.customer-access.view";
/// Set the portal site, invite customers and revoke invitations.
pub const MANAGE: &str = "admin.customer-access.manage";
/// A customer's own portal home and account.
pub const PORTAL_ACCOUNT: &str = "portal.account.view";
/// The portal role every invited customer is given.
pub const PORTAL_ROLE: &str = "portal_customer";

/// How many days an invitation link works.
pub const INVITATION_DAYS_KEY: &str = "customer_portal.invitation_days";
/// How many days a request for an account waits for its address to be
/// confirmed (installation-wide: it is read before anyone is signed in).
pub const REGISTRATION_DAYS_KEY: &str = "customer_portal.registration_days";

pub const STAFF_PATH: &str = "/customer-access";
pub const HOME_PATH: &str = "/portal";
pub const ACCOUNT_PATH: &str = "/portal/account";
/// The customer's authenticator page and step-up prompt. Core User names
/// the paths, because its authenticator sends a customer who owes a step-up
/// to them.
pub const TWO_FACTOR_PATH: &str = bilibi_user::PORTAL_TWO_FACTOR_PATH;
pub const CONFIRM_PATH: &str = bilibi_user::PORTAL_CONFIRM_PATH;

pub const MIGRATION: (&str, &str) = (
    "core-customer_access-001-portal",
    include_str!("../migrations/001-portal.sql"),
);
const SITE_ENABLED_MIGRATION: (&str, &str) = (
    "core-customer_access-002-site-enabled",
    include_str!("../migrations/002-site-enabled.sql"),
);
const LOGO_MIGRATION: (&str, &str) = (
    "core-customer_access-004-site-logo",
    include_str!("../migrations/004-site-logo.sql"),
);
const DELIVERY_MIGRATION: (&str, &str) = (
    "core-customer_access-005-delivery",
    include_str!("../migrations/005-delivery.sql"),
);
const PERSONAL_DATA_MIGRATION: (&str, &str) = (
    "core-portal-006-personal-data",
    include_str!("../migrations/006-personal-data.sql"),
);
const REGISTRATION_LIMITS_MIGRATION: (&str, &str) = (
    "core-portal-007-registration-limits",
    include_str!("../migrations/007-registration-limits.sql"),
);
const INVITATION_PERSONAL_DATA_MIGRATION: (&str, &str) = (
    "core-portal-008-invitation-personal-data",
    include_str!("../migrations/008-invitation-personal-data.sql"),
);
const REGISTRATION_MIGRATION: (&str, &str) = (
    "core-customer_access-003-registration",
    include_str!("../migrations/003-registration.sql"),
);

macro_rules! provenance {
    () => {
        Provenance {
            module: "core/portal",
            source: file!(),
            line: line!(),
        }
    };
}
/// Recorded when staff approve a request for an account; see the README's
/// Events.
pub const REGISTRATION_APPROVED: &str = "core/portal.registration_approved";
inventory::submit! { Contribution::new(bilibi_events::EVENTS, REGISTRATION_APPROVED, provenance!(), &bilibi_events::EventType { version: 1 }) }
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "core/portal.accept-invitation", provenance!(), &CredentialPath { path: "/portal/accept/{site}/{token}" }) }
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "core/portal.verify-registration", provenance!(), &CredentialPath { path: "/portal/verify/{site}/{token}" }) }

inventory::submit! {
    Module {
        id: "core/portal", layer: Layer::Core,
        dependencies: &["base/module_registry", "base/database", "base/events", "base/tenancy", "base/authz", "base/settings", "base/mail", "base/media", "base/notifications", "base/ui", "base/agent_api", "base/menu", "base/party_ids", "base/datetime", "base/locale", "base/records", "base/personal_data", "base/privacy", "core/company", "core/person", "core/affiliation", "core/user"],
        migrations: &[MIGRATION, SITE_ENABLED_MIGRATION, REGISTRATION_MIGRATION, LOGO_MIGRATION, DELIVERY_MIGRATION, PERSONAL_DATA_MIGRATION, REGISTRATION_LIMITS_MIGRATION, INVITATION_PERSONAL_DATA_MIGRATION],
        services: &[
            Service { id: SERVICE, factory: |services| {
                let desk = delivery::Desk {
                    db: services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
                    mutations: services.get("base/database.mutations")?,
                    authentication: services.get("base/tenancy.authentication")?,
                    notifications: services.get("base/notifications.sink")?,
                };
                delivery::bind(desk.clone())?;
                let access = Arc::new(PortalService {
                    delivery: desk,
                    db: services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
                    mutations: services.get("base/database.mutations")?,
                    users: services.get("core/user.accounts")?,
                    grants: services.get("base/authz.grants")?,
                    authentication: services.get("base/tenancy.authentication")?,
                    mailer: services.get("base/mail.mailer")?,
                    media: services.get(bilibi_media::SERVICE)?,
                    settings: services.get("base/settings.store")?,
                    events: services.get(bilibi_events::SERVICE)?,
                    origin: std::env::var("APP_ORIGIN").unwrap_or_else(|_| "http://localhost:3000".into()).trim_end_matches('/').to_owned(),
                });
                operations::bind(access.clone())?;
                Ok(access)
            } },
            Service { id: PAGES, factory: |services| Ok(Arc::new(bilibi_ui::page::PageService::<Scope, bilibi_database::Error>(Arc::new(web::Pages {
                db: services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
                access: services.get(SERVICE)?,
                persons: services.get(bilibi_person::SERVICE)?,
                records: services.get(bilibi_records::SERVICE)?,
                affiliations: services.get(bilibi_affiliation::SERVICE)?,
                contracts: services.get(bilibi_agent_api::page::SERVICE)?,
                display: services.get("base/datetime.display")?,
                locales: services.get("base/locale.locales")?,
            })))) },
            Service { id: PORTAL_PAGES, factory: |services| Ok(Arc::new(bilibi_ui::page::PageService::<PortalScope, bilibi_database::Error>(Arc::new(portal::Pages {
                db: services.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),
                access: services.get(SERVICE)?,
                users: services.get("core/user.accounts")?,
            })))) },
        ],
    }
}

// A mail adapter's bounce report for an invitation's email lands here.
inventory::submit! { Contribution::new(bilibi_mail::OBSERVERS, "core/portal.invitation_delivery", provenance!(), &bilibi_mail::Observer {
    kind: delivery::REFERENCE_KIND, observe: delivery::observe,
}) }

inventory::submit! { Contribution::new(VOCABULARY, "core/portal", provenance!(), &VocabularyContribution {
    domains: &["portal"],
    verbs: &[],
    capabilities: &[
        CapabilityDefinition::tenant(VIEW),
        CapabilityDefinition::tenant(MANAGE),
        CapabilityDefinition::portal(PORTAL_ACCOUNT),
    ],
    roles: &[
        RoleDefinition { code: "tenant_owner", name: "Tenant Owner", description: "Contributed system role; reconciled explicitly", grant_all: false, capabilities: &[VIEW, MANAGE] },
        RoleDefinition { code: PORTAL_ROLE, name: "Portal customer", description: "Every customer an invitation admits: its own portal home and account, and its own contact details", grant_all: false, capabilities: &[PORTAL_ACCOUNT, bilibi_person::PORTAL] },
    ],
    system_principals: &[],
}) }

inventory::submit! { Contribution::new(DEFINITIONS, INVITATION_DAYS_KEY, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: &[ScopeKind::Tenant], default: || json!(7), nullable: false, encrypted: false,
    minimum: Some(1.0), maximum: Some(60.0), validator: None,
    editor: Some(Editor { group: "Customer portal", label: "Invitation link lifetime (days)", help: "How long the link in a customer invitation works. A new invitation to the same address replaces the old link.", capability: Some(MANAGE), reveal_duration_ms: 5_000 }),
})) }

inventory::submit! { Contribution::new(DEFINITIONS, REGISTRATION_DAYS_KEY, provenance!(), &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: &[ScopeKind::Global], default: || json!(3), nullable: false, encrypted: false,
    minimum: Some(1.0), maximum: Some(30.0), validator: None,
    editor: Some(Editor { group: "Customer portal", label: "Account request confirmation (days)", help: "How long the link that confirms a request for a customer account works. An unconfirmed request is dropped after that.", capability: Some(MANAGE), reveal_duration_ms: 5_000 }),
})) }

macro_rules! registration_limit {
    ($key:expr, $default:expr, $label:expr, $help:expr) => {
        inventory::submit! { Contribution::new(DEFINITIONS, $key, provenance!(), &Setting::Definition(Definition {
            value_type: ValueType::Integer, scopes: &[ScopeKind::Global], default: || json!($default), nullable: false, encrypted: false,
            minimum: Some(1.0), maximum: Some(100_000.0), validator: None,
            editor: Some(Editor { group: "Customer portal", label: $label, help: $help, capability: Some(MANAGE), reveal_duration_ms: 5_000 }),
        })) }
    };
}

registration_limit!(SENDER_MINUTE_KEY, SENDER_MINUTE_DEFAULT, "Account requests per sender per minute", "The most new customer account requests one network address may start in a rolling minute, across every server.");
registration_limit!(SENDER_DAY_KEY, SENDER_DAY_DEFAULT, "Account requests per sender per day", "The most new customer account requests one network address may start in a rolling day, across every server.");
registration_limit!(RECIPIENT_MINUTE_KEY, RECIPIENT_MINUTE_DEFAULT, "Account requests per email per minute", "The most new customer account requests one email address may receive in a rolling minute, across every server.");
registration_limit!(RECIPIENT_DAY_KEY, RECIPIENT_DAY_DEFAULT, "Account requests per email per day", "The most new customer account requests one email address may receive in a rolling day, across every server.");
registration_limit!(PORTAL_MINUTE_KEY, PORTAL_MINUTE_DEFAULT, "Account requests per portal per minute", "The most new customer account requests one customer portal may accept in a rolling minute, across every sender and server.");
registration_limit!(PORTAL_DAY_KEY, PORTAL_DAY_DEFAULT, "Account requests per portal per day", "The most new customer account requests one customer portal may accept in a rolling day, across every sender and server.");
registration_limit!(PENDING_KEY, PENDING_DEFAULT, "Pending account requests per portal", "The most unapproved customer account requests one portal may hold. Staff are notified when this or a portal request-rate limit is reached.");

inventory::submit! { Contribution::new(ROUTES, "core/portal.staff", provenance!(), &Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant,
    method: Method::Get, path: STAFF_PATH, service: PAGES, operation: "portal",
    access: Access::Capability(VIEW), framed: true,
}) }
inventory::submit! { Contribution::new(ROUTES, "core/portal.home", provenance!(), &Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant,
    method: Method::Get, path: HOME_PATH, service: PORTAL_PAGES, operation: "home",
    access: Access::Portal(PORTAL_ACCOUNT), framed: false,
}) }
inventory::submit! { Contribution::new(ROUTES, "core/portal.two-factor", provenance!(), &Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant,
    method: Method::Get, path: TWO_FACTOR_PATH, service: PORTAL_PAGES, operation: "two_factor",
    access: Access::Portal(PORTAL_ACCOUNT), framed: false,
}) }
inventory::submit! { Contribution::new(ROUTES, "core/portal.two-factor-confirm", provenance!(), &Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant,
    method: Method::Get, path: CONFIRM_PATH, service: PORTAL_PAGES, operation: "confirm",
    access: Access::Portal(PORTAL_ACCOUNT), framed: false,
}) }
inventory::submit! { Contribution::new(ROUTES, "core/portal.account", provenance!(), &Route { company_scope: bilibi_module_registry::contributions::CompanyScope::Tenant,
    method: Method::Get, path: ACCOUNT_PATH, service: PORTAL_PAGES, operation: "account",
    access: Access::Portal(PORTAL_ACCOUNT), framed: false,
}) }

// The Person record's portal-access panel; it writes through the same
// contracts as the staff page below.
inventory::submit! { Contribution::new(EMBEDS, "portal.person-portal-access", provenance!(), &Embed {
    slot: "person.portal-access", context: EmbedContext::Record { kind: "core/person" },
    service: PAGES, operation: "person_portal_access", access: Access::Capability(VIEW),
}) }

// Every staff write goes through the contracts agents call.
inventory::submit! { Contribution::new(PAGE_WRITES, PAGES, provenance!(), &PageWrites {
    service: PAGES,
    contracts: &[
        ("save_site", operations::SITE_SET),
        ("invite", operations::INVITE),
        ("confirm_revoke", operations::REVOKE),
        ("resend", operations::INVITE),
        ("confirm_revoke_access", operations::ACCOUNT_REVOKE),
        ("restore_access", operations::ACCOUNT_RESTORE),
        ("invite", operations::APPROVE),
        ("confirm_reject", operations::REJECT),
    ],
    pending: Pending::Events { events: &["store_logo"], reason: "Storing a logo image puts its bytes into Media outside a contract; the site save that uses it is the site.set contract." },
}) }
// The portal pages read, except the authenticator page, which writes through
// the User facade like the staff authenticator page does.
inventory::submit! { Contribution::new(PAGE_WRITES, PORTAL_PAGES, provenance!(), &PageWrites {
    service: PORTAL_PAGES,
    contracts: &[],
    pending: Pending::Every { reason: "The authenticator page enrols and moves the customer's own authenticator through the User facade, as the staff page does; no contract yet." },
}) }

// A contact dropped on the page, or carried by "Non-local actions", opens the same
// invitation review as "+ Customer"; the review commits the invitation
// contract (one Person at a time, as the invite dialog takes).
bilibi_records::drop_target!(
    "core/portal",
    bilibi_records::DropTarget {
        id: web::INVITE_TARGET,
        service: PAGES,
        contract: operations::INVITE,
        capability: MANAGE,
        path: STAFF_PATH,
        label: "Invite as customer",
        accepts: &["person"],
        max: 1,
    }
);

bilibi_menu::contribute!(
    "core/portal",
    MenuItem::page("admin.portal", "Customer portal", STAFF_PATH)
        .under("admin")
        .icon("user-group")
        .requires(Requirement::Capability(VIEW))
        .order(57)
);
bilibi_menu::contribute!(
    "core/portal",
    MenuItem::page("portal.home", "Home", HOME_PATH)
        .under(PORTAL_ROOT)
        .icon("home")
        .requires(Requirement::Capability(PORTAL_ACCOUNT))
        .order(0)
);
bilibi_menu::contribute!(
    "core/portal",
    MenuItem::page("portal.account", "My account", ACCOUNT_PATH)
        .under(PORTAL_ROOT)
        .icon("user-circle")
        .requires(Requirement::Capability(PORTAL_ACCOUNT))
        .order(1000)
);
