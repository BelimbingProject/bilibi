//! Running-database proof of report subjects (#877): a report source declares
//! the record its rows are about (`TableDecl::subject`), and a saved report
//! then counts rows about records merged since under their survivor, and
//! gives a reader only the rows whose subject they may read (record reach on
//! the subject's own table). Cases about accounts stand in for a Domain's
//! records about its customers. Run on an empty disposable database; it
//! migrates, installs the walkthrough fixture, compares every answer with
//! plain SQL and exits non-zero when a check does not hold.
//!
//! BILIBI_REPORT_SUBJECTS=disposable cargo run -p bilibi-web --example report-subjects
use bilibi_authz::{
    grants::{CreatedRole, Grants},
    roles::NewRole,
    CapabilityDefinition, Reach, RoleDefinition, VocabularyContribution, VOCABULARY,
};
use bilibi_base::reports::{
    Definition, Draft, FilterDef, GroupingDef, Kind, MeasureDef, ReportError, Reports, RunInput,
    RunResult,
};
use bilibi_database::{pool, Db, MutationBoundary};
use bilibi_grid::{Bounds, Field, FieldType, GridContribution, PageSpec, Source, TableDecl};
use bilibi_module_registry::{Contribution, Layer, Module, Provenance};
use bilibi_records::{Access, RecordKind, RecordOwner, Records};
use bilibi_tenancy::{ProofClient, ProofFuture, Scope};
use serde_json::{json, Value};
use std::collections::BTreeMap;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const MODULE: &str = "extensions/report_subjects";
const ACCOUNTS: &str = "reference.accounts.view";
const CASES: &str = "reference.cases.view";
const ACCOUNT: &str = "report_subjects.account";

inventory::submit! {
    Module {
        id: MODULE, layer: Layer::Extensions,
        dependencies: &["base/database", "base/grid", "base/records", "base/authz", "base/tenancy"],
        migrations: &[("extensions-report-subjects-001-cases",
            "CREATE TABLE report_subject_accounts (id bigint PRIMARY KEY, tenant_id bigint NOT NULL REFERENCES tenants(id), name text NOT NULL, owner_user_id bigint, removed boolean NOT NULL DEFAULT false); \
             CREATE TABLE report_subject_cases (id bigint PRIMARY KEY, tenant_id bigint NOT NULL REFERENCES tenants(id), title text NOT NULL, account_id bigint, minutes integer NOT NULL, opened_on date NOT NULL); \
             CREATE INDEX report_subject_cases_account_idx ON report_subject_cases(tenant_id, account_id);")],
        services: &[],
    }
}
// The manager reads every account; the viewer reads cases and runs reports,
// and reaches accounts only through the role the walkthrough gives them.
inventory::submit! { Contribution::new(VOCABULARY, MODULE, Provenance { module: MODULE, source: file!(), line: line!() }, &VocabularyContribution {
    domains: &["reference"], verbs: &["view"],
    capabilities: &[CapabilityDefinition::tenant(ACCOUNTS).with_reach(), CapabilityDefinition::tenant(CASES)],
    roles: &[
        RoleDefinition { code: "manager", name: "manager", description: "Walkthrough fixture role", grant_all: false, capabilities: &[ACCOUNTS, CASES, "reports.view", "reports.author", "reports.share"] },
        RoleDefinition { code: "viewer", name: "viewer", description: "Walkthrough fixture role", grant_all: false, capabilities: &[CASES, "reports.view"] },
    ],
    system_principals: &[],
}) }
static GRID: GridContribution = GridContribution {
    record_fields: &[],
    tables: &[
        TableDecl::new(
            "report_subject_accounts",
            "Accounts",
            ACCOUNTS,
            Source::tenant("SELECT id, name, owner_user_id FROM report_subject_accounts WHERE tenant_id = $1 AND NOT removed"),
            "id",
            &[
                Field::new("id", "ID", FieldType::Integer),
                Field::new("name", "Name", FieldType::String),
                Field::new("owner_user_id", "Owner", FieldType::Integer),
            ],
        )
        .label_field("name")
        .owner("owner_user_id")
        .record_types(&[ACCOUNT]),
        TableDecl::new(
            "report_subject_cases",
            "Cases",
            CASES,
            Source::tenant("SELECT id, title, account_id, minutes, opened_on FROM report_subject_cases WHERE tenant_id = $1"),
            "id",
            &[
                Field::new("id", "ID", FieldType::Integer),
                Field::new("title", "Title", FieldType::String),
                Field::new("account_id", "Account", FieldType::Integer),
                Field::new("minutes", "Minutes", FieldType::Integer),
                Field::new("opened_on", "Opened", FieldType::Date),
            ],
        )
        .label_field("title")
        .time_field("opened_on")
        // Each case is about one account.
        .subject("account_id", ACCOUNT),
    ],
    links: &[],
};
bilibi_grid::contribute!("extensions/report_subjects", "report_subjects.grid", &GRID);

/// The account owner's read rule: a live account of the reader's tenant that
/// their reach on the accounts capability admits.
struct AccountRecords;
static OWNER: AccountRecords = AccountRecords;
impl RecordOwner for AccountRecords {
    fn authorize<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        id: &'a str,
    ) -> ProofFuture<'a, Access> {
        Box::pin(async move {
            let Ok(id) = id.parse::<i64>() else {
                return Ok(Access::NotFound);
            };
            let Some(row) = client
                .query_opt(
                    "SELECT owner_user_id FROM report_subject_accounts WHERE tenant_id=$1 AND id=$2 AND NOT removed",
                    &[&scope.tenant(), &id],
                )
                .await?
            else {
                return Ok(Access::NotFound);
            };
            Ok(
                if bilibi_authz::reach_admits(client, scope, ACCOUNTS, row.get(0), None).await? {
                    Access::Readable
                } else {
                    Access::Forbidden
                },
            )
        })
    }
}
bilibi_records::declare!(
    "extensions/report_subjects",
    RecordKind::new(ACCOUNT, "report_subject_accounts", &OWNER)
);

type Failure = Box<dyn std::error::Error>;

struct Checks {
    failed: usize,
}
impl Checks {
    fn check(&mut self, label: &str, ok: bool, detail: impl std::fmt::Display) {
        if !ok {
            self.failed += 1;
        }
        println!("[{}] {label} -> {detail}", if ok { "PASS" } else { "FAIL" });
    }
}

async fn login(app: &bilibi_web::App, email: &str) -> Result<Scope, Failure> {
    let login = app
        .user
        .login(email, bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    Ok(app.security.guard(&login.token).await?)
}

fn draft(name: &str, kind: Kind, definition: Definition) -> Draft {
    Draft {
        name: name.into(),
        description: String::new(),
        record_type: "report_subject_cases".into(),
        kind,
        definition,
    }
}

/// A summary's lines as `account -> (stored id, cases, minutes)`, and its
/// total `(cases, minutes)`. A line without an account is keyed `-`.
type Lines = BTreeMap<String, (String, i64, i64)>;
fn summary(result: &RunResult) -> (Lines, (i64, i64)) {
    let RunResult::Summary { lines, total, .. } = result else {
        return (Lines::new(), (-1, -1));
    };
    let minutes = |values: &[Value]| values.first().and_then(Value::as_i64).unwrap_or(0);
    let lines = lines
        .iter()
        .map(|(keys, _, rows, values)| {
            let key = &keys[0];
            (
                key["label"].as_str().unwrap_or("-").to_owned(),
                (
                    key["stored_id"].as_str().unwrap_or("-").to_owned(),
                    *rows,
                    minutes(values),
                ),
            )
        })
        .collect();
    (lines, (total.0, minutes(&total.1)))
}
fn expected(lines: &[(&str, &str, i64, i64)]) -> Lines {
    lines
        .iter()
        .map(|(label, id, rows, minutes)| {
            ((*label).to_owned(), ((*id).to_owned(), *rows, *minutes))
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    if std::env::var("BILIBI_REPORT_SUBJECTS").as_deref() != Ok("disposable") {
        return Err("set BILIBI_REPORT_SUBJECTS=disposable on an empty disposable database".into());
    }
    bilibi_base::link();
    bilibi_core::link();
    let db: Db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_database::migrate(&db, &bilibi_web::modules()?).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    // Ada Manager (1) reads every account. Lee Viewer (2) is the reader whose
    // reach is "mine". Each has a desk in company 1.
    db.get().await?.batch_execute(
        "INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(4,1,'alex_admin','Alex Admin','human'),(11,1,'lee_viewer','Lee Viewer','human') ON CONFLICT DO NOTHING; \
         UPDATE users SET person_id=4 WHERE tenant_id=1 AND id=4; UPDATE users SET person_id=11 WHERE tenant_id=1 AND id=2; \
         INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,4,1),(1,11,1) ON CONFLICT DO NOTHING; \
         SELECT setval(pg_get_serial_sequence('persons','id'),(SELECT max(id) FROM persons));",
    ).await?;
    let app = bilibi_web::App::new(db.clone(), "http://127.0.0.1:43877")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let reports = app.services.get::<Reports>(bilibi_base::reports::SERVICE)?;
    let records = app.services.get::<Records>(bilibi_records::SERVICE)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let admin = login(&app, "admin@example.test").await?;
    let manager = login(&app, "manager@example.test").await?;
    let viewer = login(&app, "viewer@example.test").await?;
    let mut checks = Checks { failed: 0 };

    // Northwind was entered twice; the duplicate (2) is Ada's, the record
    // that survives (1) is Lee's. Contoso is Ada's, Fabrikam is Lee's. One
    // case names no account, and tenant 2 has an account and a case of its own.
    db.get().await?.batch_execute(
        "INSERT INTO report_subject_accounts(id,tenant_id,name,owner_user_id) VALUES \
         (1,1,'Northwind',2),(2,1,'Northwind Ltd',1),(3,1,'Contoso',1),(4,1,'Fabrikam',2),(5,2,'Other tenant account',5); \
         INSERT INTO report_subject_cases(id,tenant_id,title,account_id,minutes,opened_on) VALUES \
         (1,1,'Late delivery',1,10,CURRENT_DATE),(2,1,'Wrong invoice',2,20,CURRENT_DATE),(3,1,'Missing part',2,30,CURRENT_DATE), \
         (4,1,'Broken seal',3,40,CURRENT_DATE),(5,1,'No reply',3,50,CURRENT_DATE),(6,1,'Slow refund',4,60,CURRENT_DATE), \
         (7,1,'General question',NULL,70,CURRENT_DATE),(8,2,'Other tenant case',5,80,CURRENT_DATE)",
    ).await?;

    let by_account = Definition {
        groupings: vec![GroupingDef {
            column: "account_id".into(),
            bucket: None,
        }],
        measures: vec![MeasureDef {
            column: "minutes".into(),
            agg: "sum".into(),
        }],
        ..Definition::default()
    };
    let write = async |draft: &Draft| -> Result<i64, ReportError> {
        let mut client = db
            .get_detached("the walkthrough's own checking connection")
            .await
            .map_err(bilibi_database::Error::from)?;
        let mut tx = boundary
            .begin(&mut client, manager.mutation_context("reports.create"))
            .await?;
        let id = reports.create(&mut tx, &manager, draft).await?;
        reports.share(&mut tx, &manager, id, 1, true, &[]).await?;
        tx.commit().await?;
        Ok(id)
    };
    let report = write(&draft("Cases by account", Kind::Summary, by_account)).await?;
    let listed = write(&draft(
        "Cases",
        Kind::Rows,
        Definition {
            columns: vec!["title".into(), "account_id".into()],
            ..Definition::default()
        },
    ))
    .await?;
    let run = async |scope: &Scope| -> Result<(Lines, (i64, i64)), Failure> {
        let run = reports
            .run(scope, report, &RunInput::default())
            .await
            .map_err(|e| format!("run: {e:?}"))?;
        Ok(summary(&run.result))
    };

    println!("== Merged duplicates count under the survivor");
    let before = run(&manager).await?;
    checks.check(
        "before the merge the duplicate is a line of its own",
        before
            == (
                expected(&[
                    ("Northwind", "1", 1, 10),
                    ("Northwind Ltd", "2", 2, 50),
                    ("Contoso", "3", 2, 90),
                    ("Fabrikam", "4", 1, 60),
                    ("-", "-", 1, 70),
                ]),
                (7, 280),
            ),
        format!("{before:?}"),
    );
    // The owner's merge: the duplicate is removed and redirected to the
    // survivor in one audited transaction. No case row is rewritten.
    let mut client = db.get().await?;
    let mut tx = boundary
        .begin(&mut client, admin.mutation_context("report_subjects.merge"))
        .await?;
    records.redirect(&mut tx, &admin, ACCOUNT, "2", "1").await?;
    tx.sql()
        .execute(
            "UPDATE report_subject_accounts SET removed=true WHERE tenant_id=1 AND id=2",
            &[],
        )
        .await?;
    tx.commit().await?;
    drop(client);
    let stored: i64 = db
        .get()
        .await?
        .query_one(
            "SELECT count(*) FROM report_subject_cases WHERE tenant_id=1 AND account_id=2",
            &[],
        )
        .await?
        .get(0);
    let merged = run(&manager).await?;
    checks.check(
        "after the merge both records are one line under the survivor, and the total is unchanged",
        stored == 2
            && merged
                == (
                    expected(&[
                        ("Northwind", "1", 3, 60),
                        ("Contoso", "3", 2, 90),
                        ("Fabrikam", "4", 1, 60),
                        ("-", "-", 1, 70),
                    ]),
                    (7, 280),
                ),
        format!("{merged:?}; {stored} cases still store the merged id"),
    );
    let drill = RunInput {
        as_rows: true,
        drill: vec![FilterDef {
            column: "account_id".into(),
            test: "equals".into(),
            value: json!("1"),
            ask: false,
        }],
        ..RunInput::default()
    };
    let behind = reports
        .run(&manager, report, &drill)
        .await
        .map_err(|e| format!("drill: {e:?}"))?;
    let RunResult::Rows { total, rows, .. } = &behind.result else {
        return Err("a drill-down lists rows".into());
    };
    checks.check(
        "drilling into the survivor lists the cases stored under either id",
        *total == 3 && rows.len() == 3,
        format!("{total} cases"),
    );

    println!("== A reader gets only the rows whose subject they may read");
    let none = run(&viewer).await?;
    checks.check(
        "a reader with no grant on accounts sees only the case without an account",
        none == (expected(&[("-", "-", 1, 70)]), (1, 70)),
        format!("{none:?}"),
    );
    let readers = match grants
        .create_role(
            &admin,
            &NewRole {
                company_id: Some(1),
                name: "Account readers".into(),
                code: "account_readers".into(),
                description: "Disposable report subjects proof".into(),
            },
        )
        .await?
    {
        CreatedRole::Created(id) => id,
        CreatedRole::Refused(errors) => return Err(format!("create role: {errors:?}").into()),
    };
    grants
        .set_role_capability(&admin, readers, ACCOUNTS, Some(Reach::Own))
        .await?;
    grants.assign_role(&admin, 2, readers, true).await?;
    let own = run(&viewer).await?;
    checks.check(
        "reach mine: the survivor's line holds every merged case, the account they do not reach is no line, and no line is Restricted",
        own == (
            expected(&[
                ("Northwind", "1", 3, 60),
                ("Fabrikam", "4", 1, 60),
                ("-", "-", 1, 70),
            ]),
            (5, 190),
        ),
        format!("{own:?}"),
    );
    let plain: (i64, i64) = {
        let row = db.get().await?.query_one(
            "SELECT count(*), COALESCE(sum(c.minutes),0)::bigint FROM report_subject_cases c WHERE c.tenant_id=1 AND (c.account_id IS NULL OR EXISTS (SELECT 1 FROM report_subject_accounts a WHERE a.tenant_id=1 AND NOT a.removed AND a.owner_user_id=2 AND a.id=COALESCE((SELECT r.to_id::bigint FROM base_records_redirects r WHERE r.tenant_id=1 AND r.kind=$1 AND r.from_id=c.account_id::text), c.account_id)))",
            &[&ACCOUNT],
        ).await?;
        (row.get(0), row.get(1))
    };
    checks.check(
        "the reader's total equals plain SQL over the same rows",
        own.1 == plain,
        format!("{:?} = {plain:?}", own.1),
    );
    let rows = reports
        .run(&viewer, listed, &RunInput::default())
        .await
        .map_err(|e| format!("rows: {e:?}"))?;
    let RunResult::Rows { total, rows, .. } = &rows.result else {
        return Err("a rows report lists rows".into());
    };
    let named: Vec<String> = rows
        .iter()
        .map(|(_, cells)| {
            format!(
                "{}={}",
                cells[0].as_str().unwrap_or("-"),
                cells[1].as_str().unwrap_or("-")
            )
        })
        .collect();
    checks.check(
        "a list of cases leaves out the cases about that account and names a merged account as its survivor",
        *total == 5
            && named
                == [
                    "Late delivery=Northwind",
                    "Wrong invoice=Northwind",
                    "Missing part=Northwind",
                    "Slow refund=Fabrikam",
                    "General question=-",
                ],
        format!("{total}: {named:?}"),
    );
    let hidden = RunInput {
        as_rows: true,
        drill: vec![FilterDef {
            column: "account_id".into(),
            test: "equals".into(),
            value: json!("3"),
            ask: false,
        }],
        ..RunInput::default()
    };
    let answer = reports
        .run(&viewer, report, &hidden)
        .await
        .map_err(|e| format!("hidden: {e:?}"))?;
    checks.check(
        "asking for the account they do not reach by its id finds nothing",
        matches!(answer.result, RunResult::Rows { total: 0, .. }),
        "0 cases",
    );
    let keys = bilibi_grid::page_keys(
        &db,
        &viewer,
        "report_subject_cases",
        &PageSpec {
            filters: Vec::new(),
            sorts: Vec::new(),
            timezone: "UTC".into(),
            offset: 0,
            limit: 100,
            bounds: Bounds::default(),
        },
    )
    .await?;
    checks.check(
        "Grid's row page agrees: cases 1, 2, 3, 6 and 7",
        keys.keys == [json!(1), json!(2), json!(3), json!(6), json!(7)] && keys.total == 5,
        format!("{:?}", keys.keys),
    );

    println!("== Wider reach and other tenants");
    grants
        .set_role_capability(&admin, readers, ACCOUNTS, Some(Reach::All))
        .await?;
    let all = run(&viewer).await?;
    checks.check(
        "reach all: the reader sees what the manager sees, never another tenant's case",
        all == merged,
        format!("{all:?}"),
    );
    // A redirect of the same id in another tenant changes nothing here.
    db.get()
        .await?
        .execute(
            "INSERT INTO base_records_redirects(tenant_id,kind,from_id,to_id) VALUES(2,$1,'4','5')",
            &[&ACCOUNT],
        )
        .await?;
    let still = run(&manager).await?;
    checks.check(
        "another tenant's merge of the same id is not followed",
        still == merged,
        format!("{still:?}"),
    );

    if checks.failed > 0 {
        return Err(format!("{} check(s) failed", checks.failed).into());
    }
    println!("PASS all report subject checks");
    Ok(())
}
