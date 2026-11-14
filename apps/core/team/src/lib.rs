//! Shared teams: a name, a kind, an optional parent team and company, leads
//! and members (staff accounts). Contract: README.md.
mod access;
mod facade;
mod members;
mod operations;
mod ports;
mod records;
mod tree;
mod web;
use bilibi_authz::{CapabilityDefinition as C, RoleDefinition, VocabularyContribution, VOCABULARY};
use bilibi_module_registry::{
    contributions::{
        Access, CompanyScope, Method, PageWrites, Pending, Route, Seed, SeedAction, SeedMode,
        PAGE_WRITES, ROUTES, SEEDS,
    },
    Contribution, Layer, Module, Service,
};
use bilibi_picklists::{PickList, Start};
pub use facade::{
    Checked, FieldErrors, LeadAbove, Member, Membership, Team, TeamInput, TeamQuery, TeamRow,
    Teams, MAX_BATCH, MAX_DEPTH, MAX_MEMBERS,
};
pub use members::{MemberChange, MemberPlan, MemberStep};
use std::sync::Arc;
pub const SERVICE: &str = "core/team.teams";
pub const VIEW: &str = "teams.view";
pub const MANAGE: &str = "teams.manage";
/// The tenant's names for kinds of team (crew, pod, squad). Core keeps the
/// plain word "team".
pub const KINDS: &str = "core.team.kind";
pub(crate) const PAGES: &str = "core/team.pages";
pub(crate) const LIST_PATH: &str = "/teams";
macro_rules! provenance {
    () => {
        bilibi_module_registry::Provenance {
            module: "core/team",
            source: file!(),
            line: line!(),
        }
    };
}
pub(crate) use provenance;
inventory::submit! {Module{
    id:"core/team",layer:Layer::Core,
    dependencies:&["base/agent_api","base/authz","base/database","base/datetime","base/grid","base/locale","base/menu","base/module_registry","base/picklists","base/ports","base/records","base/tenancy","base/ui","core/company","core/user"],
    migrations:&[("core-team-001-teams",include_str!("../migrations/001-teams.sql"))],
    services:&[
        Service{id:SERVICE,factory:|s|{let teams=Arc::new(Teams{picklists:s.get(bilibi_picklists::SERVICE)?,authentication:s.get("base/tenancy.authentication")?});operations::bind(s.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),teams.clone())?;Ok(teams)}},
        Service{id:PAGES,factory:|s|Ok(Arc::new(bilibi_ui::page::PageService::<bilibi_tenancy::Scope,bilibi_database::Error>(Arc::new(web::Pages{db:s.get::<bilibi_database::Db>("base/database.pool")?.as_ref().clone(),teams:s.get(SERVICE)?,users:s.get("core/user.accounts")?,picklists:s.get(bilibi_picklists::SERVICE)?,parties:s.get("base/tenancy.parties")?,records:s.get(bilibi_records::SERVICE)?,contracts:s.get(bilibi_agent_api::page::SERVICE)?,views:s.get("base/grid.page_views")?,display:s.get("base/datetime.display")?,locales:s.get("base/locale.locales")?}))))},
    ],
}}
inventory::submit! {Contribution::new(VOCABULARY,"core/team",provenance!(),&VocabularyContribution{
    domains:&["teams"],verbs:&["view","manage"],capabilities:&[C::tenant(VIEW),C::tenant(MANAGE)],
    roles:&[RoleDefinition{code:"tenant_owner",name:"Tenant Owner",description:"Contributed system role; reconciled explicitly",grant_all:false,capabilities:&[VIEW,MANAGE]}],system_principals:&[],
})}
// `teams.manage` is the authority to choose staff for a team.
bilibi_user::declare_staff_assignment!("core/team", bilibi_user::StaffAssignment::new(MANAGE));
bilibi_picklists::declare!(
    "core/team",
    PickList::new(KINDS, "Team kind", &[Start::new("team", "Team").locked()]).default("team")
);
inventory::submit! { Contribution::new(SEEDS, "core/team.picklists", provenance!(), &Seed {
    version: "core-team-picklists-v1", mode: SeedMode::Production,
    action: SeedAction::Service { service: bilibi_picklists::SEEDS_SERVICE, operation: "reconcile" },
}) }
macro_rules! route{($id:literal,$path:expr,$operation:literal)=>{inventory::submit!{Contribution::new(ROUTES,$id,provenance!(),&Route{company_scope:CompanyScope::Tenant,method:Method::Get,path:$path,service:PAGES,operation:$operation,access:Access::Capability(VIEW),framed:true})}};}
route!("core/team.list", LIST_PATH, "list");
route!("core/team.record", "/teams/{id}", "record");
inventory::submit! {Contribution::new(PAGE_WRITES,PAGES,provenance!(),&PageWrites{service:PAGES,contracts:&[
    ("create",operations::CREATE),("save_field",operations::UPDATE),("add_members",operations::ADD_MEMBERS),
    ("confirm_remove",operations::REMOVE_MEMBERS),("set_lead",operations::SET_LEAD),
],pending:Pending::None})}
// Staff dragged onto the Teams tile, or sent by "Add to team" on the users
// list, open the same add-members review as "+ Add members".
bilibi_records::drop_target!(
    "core/team",
    bilibi_records::DropTarget {
        id: web::ADD_TARGET,
        service: PAGES,
        contract: operations::ADD_MEMBERS,
        capability: MANAGE,
        path: LIST_PATH,
        label: "Add to team",
        accepts: &["user"],
        max: facade::MAX_BATCH,
    }
);
bilibi_menu::contribute!(
    "core/team",
    bilibi_menu::MenuItem::page("teams", "Teams", LIST_PATH)
        .under("core")
        .icon("user-group")
        .requires(bilibi_menu::Requirement::Capability(VIEW))
        .order(75)
);
