//! Worker process for one JavaScript invocation. Its pipes carry JSON only.
//! The application owns the caller's scope and validates every returned request.
use rquickjs::{context::intrinsic, Context, Runtime};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const WORKER_ARG: &str = "--watcher-script-worker";
const FRAME_BYTES: usize = 128 * 1024;
const OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Serialize)]
struct Invocation<'a> {
    source: &'a str,
    facts: Value,
    milliseconds: u64,
    heap_mib: u64,
}

fn fd_rule(argument: u8, value: u64) -> Option<Vec<SeccompRule>> {
    let condition =
        SeccompCondition::new(argument, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, value).ok()?;
    Some(vec![SeccompRule::new(vec![condition]).ok()?])
}

fn isolate() -> Option<()> {
    use libc::*;
    ::rlimit::Resource::CORE.set(0, 0).ok()?;
    ::rlimit::Resource::AS.set(384 << 20, 384 << 20).ok()?;
    ::rlimit::Resource::CPU.set(2, 2).ok()?;
    let mut rules: BTreeMap<_, _> = [
        SYS_brk,
        SYS_munmap,
        SYS_mremap,
        SYS_madvise,
        SYS_futex,
        SYS_clock_gettime,
        // QuickJS initialization masks signals on the worker thread.
        SYS_rt_sigprocmask,
        // Runtime initialization requests entropy directly from the kernel.
        SYS_getrandom,
        SYS_rt_sigreturn,
        SYS_exit,
        SYS_exit_group,
    ]
    .into_iter()
    .map(|number| (number, vec![]))
    .collect();
    rules.insert(SYS_read, fd_rule(0, 0)?);
    rules.insert(SYS_write, fd_rule(0, 1)?);
    rules.insert(SYS_mmap, fd_rule(4, u32::MAX.into())?);
    let filter: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        std::env::consts::ARCH.try_into().ok()?,
    )
    .ok()?
    .try_into()
    .ok()?;
    seccompiler::apply_filter(&filter).ok()
}

/// Called before starting Tokio or opening credentials in a fresh executable.
pub fn enter_worker() {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new(WORKER_ARG)) {
        std::process::exit(if worker().is_some() { 0 } else { 1 });
    }
}

fn worker() -> Option<()> {
    isolate()?;
    let mut frame = Vec::new();
    std::io::stdin()
        .take(FRAME_BYTES as u64 + 1)
        .read_to_end(&mut frame)
        .ok()?;
    if frame.len() > FRAME_BYTES {
        return None;
    }
    // Decode owned strings so JSON escapes in source are accepted.
    #[derive(Deserialize)]
    struct Request {
        source: String,
        facts: Value,
        milliseconds: u64,
        heap_mib: u64,
    }
    let request: Request = serde_json::from_slice(&frame).ok()?;
    let deadline = Instant::now() + Duration::from_millis(request.milliseconds.clamp(1, 1000));
    let runtime = Runtime::new().ok()?;
    runtime.set_memory_limit((request.heap_mib.clamp(1, 32) as usize) << 20);
    runtime.set_max_stack_size(256 * 1024);
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
    let context = Context::custom::<(intrinsic::Eval, intrinsic::Json)>(&runtime).ok()?;
    let output = context.with(|ctx| {
        let facts = ctx
            .json_parse(serde_json::to_vec(&request.facts).ok()?)
            .ok()?;
        ctx.globals().set("facts", facts).ok()?;
        let source = format!(
            "(function(facts){{'use strict';\n{}\n}})(facts)",
            request.source
        );
        let result = ctx.eval::<rquickjs::Value, _>(source).ok()?;
        ctx.json_stringify(result).ok()??.to_string().ok()
    })?;
    if output.len() > OUTPUT_BYTES {
        return None;
    }
    std::io::stdout().write_all(output.as_bytes()).ok()
}

pub(crate) async fn evaluate(
    source: &str,
    facts: Value,
    milliseconds: u64,
    heap_mib: u64,
) -> Result<Value, &'static str> {
    let frame = serde_json::to_vec(&Invocation {
        source,
        facts,
        milliseconds,
        heap_mib,
    })
    .map_err(|_| "script_input")?;
    if frame.len() > FRAME_BYTES {
        return Err("script_input");
    }
    let mut child =
        tokio::process::Command::new(std::env::current_exe().map_err(|_| "script_unavailable")?)
            .arg(WORKER_ARG)
            .env_clear()
            .current_dir("/")
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "script_unavailable")?;
    let mut stdin = child.stdin.take().ok_or("script_unavailable")?;
    let stdout = child.stdout.take().ok_or("script_unavailable")?;
    let outcome =
        tokio::time::timeout(Duration::from_millis(milliseconds.min(1000) + 200), async {
            stdin
                .write_all(&frame)
                .await
                .map_err(|_| "script_refused")?;
            drop(stdin);
            let mut output = Vec::new();
            stdout
                .take(OUTPUT_BYTES as u64 + 1)
                .read_to_end(&mut output)
                .await
                .map_err(|_| "script_refused")?;
            if output.len() > OUTPUT_BYTES
                || !child.wait().await.map_err(|_| "script_refused")?.success()
            {
                return Err("script_refused");
            }
            serde_json::from_slice(&output).map_err(|_| "script_refused")
        })
        .await;
    outcome.unwrap_or(Err("script_timeout"))
}
