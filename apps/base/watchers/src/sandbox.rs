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
    lookups: Value,
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
        std::process::exit(worker().unwrap_or(1));
    }
}

fn worker() -> Option<i32> {
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
        lookups: Value,
        milliseconds: u64,
        heap_mib: u64,
    }
    let request: Request = serde_json::from_slice(&frame).ok()?;
    let deadline = Instant::now() + Duration::from_millis(request.milliseconds.clamp(1, 1000));
    let runtime = Runtime::new().ok()?;
    runtime.set_memory_limit((request.heap_mib.clamp(1, 32) as usize) << 20);
    runtime.set_max_stack_size(256 * 1024);
    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let timed_out = interrupted.clone();
    use std::sync::atomic::Ordering::Relaxed;
    runtime.set_interrupt_handler(Some(Box::new(move || {
        let expired = Instant::now() >= deadline;
        timed_out.store(expired, Relaxed);
        expired
    })));
    let context = Context::custom::<(intrinsic::Eval, intrinsic::Json)>(&runtime).ok()?;
    let facts = serde_json::to_vec(&request.facts).ok()?;
    let lookups = serde_json::to_vec(&request.lookups).ok()?;
    let output = context.with(|ctx| {
        let run = || -> rquickjs::Result<Option<String>> {
            let facts = ctx.json_parse(facts)?;
            ctx.globals().set("facts", facts)?;
            let lookups = ctx.json_parse(lookups)?;
            ctx.globals().set("__lookups", lookups)?;
            let source = format!(
                "(function(facts){{'use strict';\n\
                 const canonical=(value)=>value&&typeof value==='object'?(Array.isArray(value)?value.map(canonical):Object.fromEntries(Object.keys(value).sort().map(key=>[key,canonical(value[key])]))):value;\n\
                 const lookup=(operation,input={{}})=>{{const wanted=JSON.stringify(canonical({{operation,input}}));const found=__lookups.find(item=>JSON.stringify(canonical({{operation:item.operation,input:item.input}}))===wanted);if(!found)throw new Error('lookup_refused');return JSON.parse(JSON.stringify(found.reply));}};\n{}
                 }})(facts)",
                request.source
            );
            let result = ctx.eval::<rquickjs::Value, _>(source)?;
            ctx.json_stringify(result)?
                .map(|text| text.to_string())
                .transpose()
        };
        match run() {
            Ok(output) => Ok(output),
            Err(_) if interrupted.load(Relaxed) => Err(2),
            Err(rquickjs::Error::Allocation) => Err(3),
            Err(rquickjs::Error::Exception) => {
                let exception = ctx.catch();
                let message = exception
                    .as_exception()
                    .and_then(|exception| exception.message());
                Err(match message.as_deref() {
                    Some("out of memory") => 3,
                    Some("lookup_refused") => 4,
                    _ => 1,
                })
            }
            Err(_) => Err(1),
        }
    });
    let output = match output {
        Ok(Some(output)) => output,
        Ok(None) => return None,
        Err(code) => return Some(code),
    };
    if output.len() > OUTPUT_BYTES {
        return None;
    }
    std::io::stdout().write_all(output.as_bytes()).ok()?;
    Some(0)
}

pub(crate) async fn evaluate(
    source: &str,
    facts: Value,
    lookups: Value,
    milliseconds: u64,
    heap_mib: u64,
) -> Result<Value, &'static str> {
    let frame = serde_json::to_vec(&Invocation {
        source,
        facts,
        lookups,
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
            if output.len() > OUTPUT_BYTES {
                return Err("script_output");
            }
            match child.wait().await.map_err(|_| "script_refused")?.code() {
                Some(0) => serde_json::from_slice(&output).map_err(|_| "script_output"),
                Some(2) => Err("script_timeout"),
                Some(3) => Err("script_memory"),
                Some(4) => Err("script_lookup_refused"),
                _ => Err("script_refused"),
            }
        })
        .await;
    outcome.unwrap_or(Err("script_timeout"))
}
