//! Database backups with PostgreSQL's own tools, run as the instance's own
//! role: `pg_dump` before the pause's first change, `pg_restore` for Undo and
//! for a release that fails its check after the database changed shape.
//! The connection details go to the tools through the `PG*` environment,
//! never as an argument another user could read in the process list.
use crate::{composition, jobs};
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tokio_postgres::Client;

/// The instance's database connection as libpq environment variables.
pub struct PgEnv(Vec<(&'static str, String)>);
impl PgEnv {
    pub fn from_url(url: &str) -> Result<Self, String> {
        let config: tokio_postgres::Config = url
            .parse()
            .map_err(|_| "DATABASE_URL is not a PostgreSQL connection string".to_owned())?;
        let mut vars = Vec::new();
        if let Some(tokio_postgres::config::Host::Tcp(host)) = config.get_hosts().first() {
            vars.push(("PGHOST", host.clone()));
        }
        if let Some(port) = config.get_ports().first() {
            vars.push(("PGPORT", port.to_string()));
        }
        if let Some(user) = config.get_user() {
            vars.push(("PGUSER", user.to_owned()));
        }
        if let Some(password) = config.get_password() {
            vars.push(("PGPASSWORD", String::from_utf8_lossy(password).into_owned()));
        }
        let dbname = config
            .get_dbname()
            .ok_or_else(|| "DATABASE_URL names no database".to_owned())?;
        vars.push(("PGDATABASE", dbname.to_owned()));
        Ok(Self(vars))
    }
    pub fn dbname(&self) -> &str {
        self.0
            .iter()
            .find(|(name, _)| *name == "PGDATABASE")
            .map_or("", |(_, value)| value.as_str())
    }
    fn command(&self, tool: &str) -> Command {
        let mut command = Command::new(tool);
        for (name, value) in &self.0 {
            command.env(name, value);
        }
        command
    }
}

/// Writes a custom-format dump to `file` and returns its size.
pub fn dump(pg: &PgEnv, file: &Path) -> Result<u64, String> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)
            .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    }
    let out = pg
        .command("pg_dump")
        .args(["--format=custom", "--no-owner", "--no-privileges", "--file"])
        .arg(file)
        .output()
        .map_err(|error| format!("could not run pg_dump: {error}"))?;
    if !out.status.success() {
        let _ = fs::remove_file(file);
        return Err(format!("pg_dump failed: {}", tail(&out.stderr)));
    }
    fs::metadata(file)
        .map(|meta| meta.len())
        .map_err(|error| format!("the backup {} was not written: {error}", file.display()))
}

/// What a restore carried over and could not.
pub struct Restored {
    pub carried: i64,
    pub problems: Vec<String>,
}
/// Replaces the database's contents with a dump: the software history, and
/// the audit rows of the software changes made after `since` (the instant
/// the dump was taken), are saved to `history_file` first, the `public` schema is dropped and made
/// again in bounded transactions, the dump is restored into it, and the
/// history is put back so the job doing this does not erase itself or the
/// record of who confirmed it. The service must be stopped first.
///
/// The file is what makes this safe to run again after a restart: when it
/// exists it is read instead of the database, which a cut-off restore left
/// empty. Once the dump is restored and the history carried back, the file
/// is marked restored, and a run that finds it so does nothing: the
/// database is not restored a second time over what was entered since. When
/// the restore fails and the job tables are gone, they are made again from
/// the file, so the failure can still be recorded. A clear or `pg_restore`
/// that fails is noted beside the file before that cleanup begins (see
/// [`noted_failure`]), and a restore with that note is never run again. When
/// the note cannot be written, nothing more is cleared ([`give_up`]).
pub async fn restore(
    client: &Client,
    pg: &PgEnv,
    dump: &Path,
    since: &str,
    history_file: &Path,
    target: Option<&Path>,
) -> Result<Restored, String> {
    let outcome = replace(client, pg, dump, since, history_file, target).await;
    let Err(why) = &outcome else {
        return outcome;
    };
    if jobs::history_tables_exist(client).await.unwrap_or(false) {
        return outcome;
    }
    let Some((_, saved)) = read_saved(history_file) else {
        return outcome;
    };
    match rebuild(client, &saved, false).await {
        Ok(()) => outcome,
        Err(more) => Err(format!("{why} {more}")),
    }
}
async fn replace(
    client: &Client,
    pg: &PgEnv,
    dump: &Path,
    since: &str,
    history_file: &Path,
    target: Option<&Path>,
) -> Result<Restored, String> {
    if restored(history_file) {
        return Ok(Restored {
            carried: 0,
            problems: Vec::new(),
        });
    }
    if let Some(why) = noted_failure(history_file) {
        return Err(why);
    }
    if !dump.is_file() {
        return Err(format!("the backup {} is missing", dump.display()));
    }
    let saved = match read_saved(history_file) {
        Some((_, saved)) => saved,
        None => {
            let mut saved = jobs::history(client, since).await.map_err(|error| {
                format!(
                    "could not read the software history: {}",
                    jobs::short(&error)
                )
            })?;
            let target = target.ok_or("The restore has no target release composition.")?;
            saved["target_composition"] = json!(composition::status(target)?);
            validate_restore(&saved)?;
            write_saved(history_file, dump, &saved)?;
            saved
        }
    };
    validate_restore(&saved)?;
    let kept = match keep_owner(client).await {
        Ok(kept) => kept,
        Err(why) => {
            return Err(match rebuild(client, &saved, false).await {
                Ok(()) => why,
                Err(more) => format!("{why} {more}"),
            });
        }
    };
    if let Err(why) = clear(client).await {
        return Err(give_up(client, &saved, history_file, why).await);
    }
    if let Err(why) = restore_with_owner(pg, dump, &kept) {
        return Err(give_up(client, &saved, history_file, why).await);
    }
    let (carried, problems) = jobs::restore_history(client, &saved)
        .await
        .map_err(|error| {
            format!(
                "the backup was restored but the software history was not carried back: {}",
                jobs::short(&error)
            )
        })?;
    jobs::unsettle_unrecorded(client).await;
    let _ = fs::rename(history_file, restored_name(history_file));
    Ok(Restored { carried, problems })
}
fn validate_restore(saved: &Value) -> Result<(), String> {
    let rows = saved["base_settings"]["rows"]
        .as_array()
        .ok_or("The restore record has no saved installation policy; restore refused.")?;
    let mode = match rows.as_slice() {
        [] => "shared",
        [row] => row["value"]
            .as_str()
            .ok_or("The saved installation policy is invalid.")?,
        _ => return Err("The saved installation policy is ambiguous.".into()),
    };
    let status = saved["target_composition"]
        .as_str()
        .ok_or("The restore record has no target composition; restore refused.")?;
    composition::admit(status, mode)
        .map(|_| ())
        .map_err(|why| format!("Restore refused: {why}"))
}
/// Clears `public` without asking PostgreSQL to lock every relation in one
/// transaction. Each generated statement runs in its own implicit
/// transaction on a plain [`Client`], so a large installation cannot exhaust
/// the shared lock table while it is being replaced. The durable history file
/// and dump already exist before this starts, so an interruption can safely
/// repeat the bounded clear and restore.
pub(crate) async fn clear(client: &Client) -> Result<(), String> {
    drop_each(
        client,
        "foreign keys",
        "SELECT format(
             'ALTER TABLE IF EXISTS %I.%I DROP CONSTRAINT IF EXISTS %I CASCADE',
             nsp.nspname, rel.relname, cst.conname
         )
         FROM pg_constraint AS cst
         JOIN pg_class AS rel ON rel.oid = cst.conrelid
         JOIN pg_namespace AS nsp ON nsp.oid = rel.relnamespace
         WHERE nsp.nspname = 'public' AND cst.conislocal AND cst.contype = 'f'
         ORDER BY cst.oid",
    )
    .await?;
    drop_each(
        client,
        "relations",
        "SELECT format(
             'DROP %s IF EXISTS %I.%I CASCADE',
             CASE rel.relkind
                 WHEN 'v' THEN 'VIEW'
                 WHEN 'm' THEN 'MATERIALIZED VIEW'
                 WHEN 'S' THEN 'SEQUENCE'
                 WHEN 'f' THEN 'FOREIGN TABLE'
                 ELSE 'TABLE'
             END,
             nsp.nspname,
             rel.relname
         )
         FROM pg_class AS rel
         JOIN pg_namespace AS nsp ON nsp.oid = rel.relnamespace
         WHERE nsp.nspname = 'public'
           AND rel.relkind IN ('r', 'p', 'v', 'm', 'S', 'f')
         ORDER BY
             rel.relispartition DESC,
             CASE rel.relkind WHEN 'v' THEN 0 WHEN 'm' THEN 1 WHEN 'S' THEN 3 ELSE 2 END,
             rel.oid",
    )
    .await?;
    client
        .batch_execute("DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public")
        .await
        .map_err(|error| {
            format!(
                "could not replace the cleared schema: {}",
                jobs::short(&error)
            )
        })
}

async fn drop_each(client: &Client, kind: &str, query: &str) -> Result<(), String> {
    let statements = client.query(query, &[]).await.map_err(|error| {
        format!(
            "could not list {kind} while clearing the database: {}",
            jobs::short(&error)
        )
    })?;
    for row in statements {
        let statement: String = row.get(0);
        client.execute(&statement, &[]).await.map_err(|error| {
            format!(
                "could not drop a {kind} object while clearing the database: {}",
                jobs::short(&error)
            )
        })?;
    }
    Ok(())
}
/// Ends a restore whose clear or `pg_restore` failed. The failure is noted
/// beside the saved history first, durably, and only then does the cleanup
/// clear what is left and make the job tables again. Until the caller has
/// recorded the failed step, the saved history still describes a restore to
/// run: a restart in that window finds the note and records this failure
/// instead of restoring again (`Installer::finish_cut_off_restores`).
///
/// When the note cannot be written, nothing more is cleared: the saved
/// history is renamed to its held name instead, which a restart also records
/// as failed and never restores, and only missing job tables are made again,
/// so the failure can be recorded with the dump to restore by hand.
async fn give_up(client: &Client, saved: &Value, history_file: &Path, why: String) -> String {
    let (why, wipe) = match note_failure(history_file, &why) {
        Ok(()) => (why, true),
        Err(more) => {
            let held = match hold(history_file) {
                Ok(()) => String::new(),
                Err(error) => format!(
                    " The saved history {} could not be held either: {error}",
                    history_file.display()
                ),
            };
            (
                format!("{why} {more} Nothing more was cleared.{held}"),
                false,
            )
        }
    };
    if !wipe && jobs::history_tables_exist(client).await.unwrap_or(false) {
        return why;
    }
    match rebuild(client, saved, wipe).await {
        Ok(()) => why,
        Err(more) => format!("{why} {more}"),
    }
}
/// After a restart, gives a restore that failed before it ([`noted_failure`])
/// its job tables and history again from the saved file, so the failure can
/// be recorded. The cut-off cleanup may have left the tables missing or half
/// refilled, and nothing in the database outlives the failure, so they are
/// always cleared and made again; a held history ([`give_up`]) was never
/// cleared after the failure, so its job tables are made again only when
/// they are missing. It never runs `pg_restore`.
pub async fn rebuild_failed(client: &Client, history_file: &Path) -> Result<(), String> {
    let held = is_held(history_file);
    if held && jobs::history_tables_exist(client).await.unwrap_or(false) {
        return Ok(());
    }
    let (_, saved) = read_saved(history_file).ok_or_else(|| {
        format!(
            "the saved history {} could not be read",
            history_file.display()
        )
    })?;
    rebuild(client, &saved, !held).await
}
/// After a restore that failed, gives the database back its job tables and
/// the history, so the job can record that it failed. `wipe` first clears
/// what the cut-off restore left behind.
async fn rebuild(client: &Client, saved: &Value, wipe: bool) -> Result<(), String> {
    let again = |error: tokio_postgres::Error| {
        format!(
            "The job record could not be made again: {}",
            jobs::short(&error)
        )
    };
    if wipe {
        clear(client).await?;
    }
    jobs::recreate_tables(client, saved).await.map_err(again)?;
    jobs::restore_history(client, saved).await.map_err(again)?;
    Ok(())
}

/// Reads the live owner's standing aside for the restore that replaces the
/// database: the receipt, the account's creation time, its sign-in row, its
/// authenticator and its sessions, as one JSON object (SQL `null` when there
/// is no owner yet). Call it before the database is cleared, and give the
/// result to [`restore_with_owner`].
pub(crate) async fn keep_owner(client: &Client) -> Result<Value, String> {
    // A restore can resume with the schema already cleared, and then there is
    // no live owner to keep.
    let present = client
        .query_one(
            "SELECT to_regclass('public.user_bootstrap') IS NOT NULL
                AND to_regclass('public.users') IS NOT NULL",
            &[],
        )
        .await
        .map_err(|error| format!("could not read the current owner: {}", jobs::short(&error)))?;
    if !present.get::<_, bool>(0) {
        return Ok(Value::Null);
    }
    client
        .query_one(
            "SELECT to_jsonb(o) FROM (
                 SELECT to_jsonb(b) AS receipt, u.created_at,
                     to_jsonb(i) AS identity,
                     (SELECT coalesce(jsonb_agg(to_jsonb(f)), '[]'::jsonb)
                        FROM public.user_two_factor f
                       WHERE to_regclass('public.user_two_factor') IS NOT NULL
                         AND (f.tenant_id, f.user_id) = (b.tenant_id, b.user_id)) AS two_factor,
                     (SELECT coalesce(jsonb_agg(to_jsonb(s)), '[]'::jsonb)
                        FROM public.sessions s
                       WHERE (s.tenant_id, s.user_id) = (b.tenant_id, b.user_id)) AS sessions
                 FROM (VALUES (1)) AS singleton(id)
                 LEFT JOIN public.user_bootstrap b ON b.id=singleton.id
                 LEFT JOIN public.users u ON u.tenant_id=b.tenant_id AND u.id=b.user_id
                 LEFT JOIN public.user_identities i ON i.id=u.identity_id
             ) o",
            &[],
        )
        .await
        .map(|row| row.get(0))
        .map_err(|error| format!("could not read the current owner: {}", jobs::short(&error)))
}

/// The owner standing the restore keeps: the live owner keeps being able to
/// sign in after the database is replaced. The receipt row itself is carried
/// forward, and so are the parts of that account the restored dump may have
/// rolled back: an inactive or deleted account is made active again, and a
/// lost password or authenticator stays as it is now, never as the dump had
/// it. Every pending ownership offer is cancelled, because a restored one
/// could be accepted on an old deadline. A restore that cannot keep the owner
/// usable is refused, with the terminal recovery named.
///
/// `owner` is what [`keep_owner`] read before the database was cleared: the
/// restore brings back the dump, but the owner kept here is who signs in after.
fn restore_with_owner(pg: &PgEnv, dump: &Path, owner: &Value) -> Result<(), String> {
    let mut restore = pg
        .command("pg_restore")
        .args(["--no-owner", "--no-privileges", "--file=-"])
        .arg(dump)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run pg_restore: {error}"))?;
    let mut sql = restore.stdout.take().expect("piped pg_restore output");
    let destination = pg
        .command("psql")
        .args([
            "--no-psqlrc",
            "--quiet",
            "--set=ON_ERROR_STOP=1",
            "--file=-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut destination = match destination {
        Ok(process) => process,
        Err(error) => {
            drop(sql);
            let _ = restore.wait();
            return Err(format!("could not run psql: {error}"));
        }
    };
    let mut input = destination.stdin.take().expect("piped psql input");
    let copied = input
        .write_all(
            format!(
                "\
        BEGIN;
        CREATE TEMP TABLE current_owner (receipt jsonb, created_at timestamptz,
            identity jsonb, two_factor jsonb, sessions jsonb) ON COMMIT DROP;
        INSERT INTO pg_temp.current_owner
            SELECT * FROM jsonb_to_record({}) AS x(
                receipt jsonb, created_at timestamptz, identity jsonb,
                two_factor jsonb, sessions jsonb);
    ",
                literal(owner)
            )
            .as_bytes(),
        )
        .and_then(|()| io::copy(&mut sql, &mut input).map(|_| ()));
    drop(sql);
    let archive = restore.wait();
    let applied = match (copied, archive) {
        (Ok(()), Ok(status)) if status.success() => input.write_all(
            b"
            DO $owner$
            DECLARE
                o jsonb;
                tenant bigint;
                owner bigint;
                recovery constant text := 'The backup does not contain the current owner account. Restore it by hand, or recover ownership from the server terminal with install-server.sh create-owner --recover.';
            BEGIN
                SELECT to_jsonb(k) INTO o FROM pg_temp.current_owner k;
                IF o->'receipt' IS NOT NULL AND o->'receipt' <> 'null'::jsonb THEN
                    tenant := (o->'receipt'->>'tenant_id')::bigint;
                    owner := (o->'receipt'->>'user_id')::bigint;
                    IF to_regclass('public.user_bootstrap') IS NULL
                        OR to_regclass('public.users') IS NULL THEN
                        RAISE EXCEPTION '%', RECOVERY;
                    END IF;
                    IF NOT EXISTS (
                        SELECT 1 FROM public.users u
                        WHERE u.tenant_id=tenant AND u.id=owner AND u.created_at=(o->>'created_at')::timestamptz
                    ) THEN
                        RAISE EXCEPTION '%', RECOVERY;
                    END IF;
                    -- The receipt names a person who can still sign in: the
                    -- account and its sign-in are restored to active, and the
                    -- credentials they use now are kept over the dump's.
                    UPDATE public.users SET status='active', deleted_at=NULL
                     WHERE tenant_id=tenant AND id=owner;
                    IF to_regclass('public.user_identities') IS NOT NULL THEN
                        UPDATE public.user_identities SET status='active', deleted_at=NULL
                         WHERE id=(SELECT identity_id FROM public.users WHERE tenant_id=tenant AND id=owner)
                           AND (status<>'active' OR deleted_at IS NOT NULL);
                        IF o->'identity' IS NOT NULL AND o->'identity' <> 'null'::jsonb THEN
                            UPDATE public.user_identities i
                               SET password_hash=(o->'identity'->>'password_hash'),
                                   password_version=(o->'identity'->>'password_version')::bigint,
                                   email_verified_at=(o->'identity'->>'email_verified_at')::timestamptz
                             WHERE i.id=(o->'identity'->>'id')::bigint
                               AND EXISTS (SELECT 1 FROM public.users u WHERE u.identity_id=i.id
                                            AND u.tenant_id=tenant AND u.id=owner);
                        END IF;
                    END IF;
                    IF to_regclass('public.user_two_factor') IS NOT NULL THEN
                        DELETE FROM public.user_two_factor WHERE tenant_id=tenant AND user_id=owner;
                        INSERT INTO public.user_two_factor
                            SELECT * FROM jsonb_populate_recordset(NULL::public.user_two_factor, o->'two_factor');
                    END IF;
                    DELETE FROM public.sessions WHERE tenant_id=tenant AND user_id=owner;
                    INSERT INTO public.sessions
                        SELECT * FROM jsonb_populate_recordset(NULL::public.sessions, o->'sessions');
                    -- A restored pending offer could be accepted on an old
                    -- deadline, so every one is cancelled: a handover is
                    -- simply offered again when it is still wanted.
                    IF to_regclass('public.user_ownership_transfers') IS NOT NULL THEN
                        UPDATE public.user_ownership_transfers
                           SET status='expired', resolved_at=clock_timestamp()
                         WHERE status='pending';
                    END IF;
                END IF;
                IF o->'receipt' IS NOT NULL AND o->'receipt' <> 'null'::jsonb
                    AND to_regclass('public.user_bootstrap') IS NOT NULL THEN
                    DELETE FROM public.user_bootstrap;
                    INSERT INTO public.user_bootstrap
                        SELECT (jsonb_populate_record(NULL::public.user_bootstrap, o->'receipt')).*;
                END IF;
            END $owner$;
            COMMIT;
        ",
        ),
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(status)) => Err(io::Error::other(format!("pg_restore exited with {status}"))),
    };
    drop(input);
    let output = destination
        .wait_with_output()
        .map_err(|error| format!("could not wait for psql: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "database restore could not preserve the current owner: {}",
            tail(&output.stderr)
        ));
    }
    applied.map_err(|error| format!("database restore could not finish: {error}"))
}

/// The restores a restart cut off, found by their saved history: the job,
/// the file and the dump being restored, newest job first. A file marked
/// restored is not among them.
pub fn unfinished_restores(backups: &Path) -> Vec<(i64, PathBuf, PathBuf)> {
    let mut found = Vec::new();
    for dir in fs::read_dir(backups).into_iter().flatten().flatten() {
        let Some(id) = dir
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix("job-")?.parse::<i64>().ok())
        else {
            continue;
        };
        for file in fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            let path = file.path();
            let is_saved = path
                .extension()
                .is_some_and(|ext| ext == "json" || ext == "held")
                && file
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("history-"));
            if let (true, Some((dump, _))) = (is_saved, read_saved(&path)) {
                found.push((id, path, dump));
            }
        }
    }
    found.sort_by_key(|(id, _, _)| std::cmp::Reverse(*id));
    found
}
/// A JSON value as one SQL literal (a quoted string: PostgreSQL reads it as
/// JSON wherever a json or jsonb column is expected). A JSON null becomes an
/// empty object.
fn literal(value: &Value) -> String {
    let json = match value {
        // `jsonb_to_record` takes a JSON object, never the scalar null.
        Value::Null => "{}".to_owned(),
        other => other.to_string(),
    };
    format!("'{}'", json.replace('\'', "''"))
}
/// Where a restore's failure is noted beside its saved history. The name
/// starts `history-`, so the job forgets it with the history once its outcome
/// is recorded.
fn failure_name(history_file: &Path) -> PathBuf {
    history_file.with_extension("failure")
}
fn note_failure(history_file: &Path, why: &str) -> Result<(), String> {
    let file = failure_name(history_file);
    write_durably(&file, why.as_bytes()).map_err(|error| {
        format!(
            "The failure could not be noted in {}: {error}",
            file.display()
        )
    })
}
/// The failure noted beside a saved history ([`give_up`]): the restore must
/// not run again, and the reason is recorded as the job's failure. A held
/// history, a note that cannot be read, or either that cannot be told absent,
/// still counts as a failure.
pub fn noted_failure(history_file: &Path) -> Option<String> {
    let held = held_name(history_file);
    match fs::symlink_metadata(&held) {
        Ok(_) => {
            return Some(format!(
                "The restore failed before the installer restarted, and its failure could not be noted, so the saved history was held as {} and nothing more was cleared",
                held.display()
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Some(format!(
                "The restore failed before the installer restarted, and its held history {} could not be checked: {error}",
                held.display()
            ))
        }
    }
    let file = failure_name(history_file);
    match fs::read(&file) {
        Ok(why) => Some(String::from_utf8_lossy(&why).into_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => Some(format!(
            "The restore failed before the installer restarted, and its note {} could not be read: {error}",
            file.display()
        )),
    }
}
/// Removes a saved history whose job outcome is already recorded, with its
/// failure note.
pub fn forget(history_file: &Path) {
    let _ = fs::remove_file(failure_name(history_file));
    let _ = fs::remove_file(history_file);
}
/// Where a saved history is held when its failure cannot be noted
/// ([`give_up`]). The name starts `history-`, like the note's.
fn held_name(history_file: &Path) -> PathBuf {
    history_file.with_extension("held")
}
fn is_held(history_file: &Path) -> bool {
    history_file.extension().is_some_and(|ext| ext == "held")
}
/// Renames a saved history to its held name, and makes the rename survive a
/// crash. It writes no bytes, so it can succeed when the note could not.
fn hold(history_file: &Path) -> io::Result<()> {
    fs::rename(history_file, held_name(history_file))?;
    fs::File::open(history_file.parent().unwrap_or(Path::new(".")))?.sync_all()
}
fn restored_name(history_file: &Path) -> PathBuf {
    history_file.with_extension("restored")
}
fn restored(history_file: &Path) -> bool {
    restored_name(history_file).is_file()
}
/// Sets a saved history aside after a restore that cannot be finished, so
/// the next start does not try it again.
pub fn set_aside(history_file: &Path) {
    let _ = fs::rename(history_file, history_file.with_extension("failed"));
}
fn read_saved(file: &Path) -> Option<(PathBuf, Value)> {
    let saved: Value = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    Some((
        PathBuf::from(saved["dump"].as_str()?),
        saved["history"].clone(),
    ))
}
fn write_saved(file: &Path, dump: &Path, history: &Value) -> Result<(), String> {
    write_durably(
        file,
        json!({"dump": dump.display().to_string(), "history": history})
            .to_string()
            .as_bytes(),
    )
    .map_err(|error| format!("could not save the history in {}: {error}", file.display()))
}
/// Writes `file` whole or not at all, and makes it survive a crash: the bytes
/// go to a partial file that is synced, renamed into place, and the rename
/// synced in its folder.
fn write_durably(file: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = file.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let mut partial = file.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    let mut out = fs::File::create(&partial)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    fs::rename(&partial, file)?;
    fs::File::open(dir)?.sync_all()
}

/// The last lines of a tool's output, for a plain error message.
pub fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(5);
    redact(&lines[start..].join(" | "))
}
/// A connection string with a password never reaches a step detail.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (before, after) = rest.split_at(at + 3);
        out.push_str(before);
        match after.find('@') {
            Some(end) if !after[..end].contains(char::is_whitespace) => {
                out.push_str("[redacted]");
                rest = &after[end..];
            }
            _ => rest = after,
        }
    }
    out.push_str(rest);
    out
}
