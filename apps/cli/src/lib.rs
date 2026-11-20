//! `bilibi-axi`: Bilibi's operations from a command line, for agents.
//!
//! It is one more door to the operation contracts that `/api/v1`, `/mcp` and
//! the pages already call, and it is only a door: it signs in with the OAuth
//! device grant, sends each command to the versioned API and prints the
//! answer. It holds no business rule. Whether an input is valid, whether the
//! account may do it, what a write would change and whether it still may are
//! all the server's answers, printed as they came. `scripts/cli-door.py` and
//! the two-doors walkthrough (`examples/`) keep it that way.
//!
//! Output is TOON with next-step hints (`toon.rs`); `--json` prints the
//! server's JSON. The saved sign-in is a private file (`store.rs`), and the
//! client talks to one address (`wire.rs`).
mod commands;
mod refusal;
mod sign_in;
mod store;
mod toon;
mod wire;

use serde_json::{json, Map, Value};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use store::Store;
use toon::Doc;
use wire::Wire;

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// How long `login --wait` waits when no number follows it.
const DEFAULT_WAIT: u64 = 120;
/// A token this close to its end is refreshed before use.
const REFRESH_MARGIN: u64 = 60;

const USAGE: &str = "usage: bilibi-axi [command] [args] [flags]
commands[11]:
  (none)                          where this machine is signed in, and what to run next
  login <address> [--name <app>]  start sign-in: prints a code and a link for the person
  login --wait [seconds]          finish sign-in once the person approved (default 120)
  logout                          forget the saved sign-in on this machine
  hello [message]                 check the connection and the sign-in
  search <words> [--kind read|write] [--limit N] [--unavailable] [--full]
  describe <key> [--full]         an operation's inputs and permissions
  guide <key>                     a guide's text
  read <key> [field=value ...] [--input <json|@file|->]
  plan <key> [field=value ...] | plan --file <path|->     check writes; prints a ref
  commit <ref> <key> [field=value ...] | commit <ref> --file <path|->
flags[3]:
  --json           print the server's JSON instead of TOON
  --server <name>  use this saved sign-in instead of the last one
  --help
environment[3]:
  BILIBI_AXI_HOME            folder of saved sign-ins (default ~/.config/bilibi-axi)
  BILIBI_AGENT_SELF_REPORT   harness/model-version-effort, sent with every request
  BILIBI_STEP_UP_CODE        the person's current 2FA code, for a commit that asked for one
values:
  field=value reads the value as JSON when it parses (3, true, [1,2]) and as text otherwise;
  write field='\"3\"' for the text 3. A --file is a JSON list of {\"key\":..,\"input\":{..}} steps.
";

/// One run of the command, with everything it may read from its surroundings.
pub struct Invocation {
    /// The arguments after the program name.
    pub args: Vec<String>,
    /// The folder of saved sign-ins.
    pub home: PathBuf,
    /// `BILIBI_AGENT_SELF_REPORT`.
    pub self_report: Option<String>,
    /// `BILIBI_STEP_UP_CODE`.
    pub step_up_code: Option<String>,
    /// Standard input, read only when an argument is `-`.
    pub stdin: Option<String>,
}

/// What the command printed and its exit code: 0 done, 1 refused or failed,
/// 2 wrong usage.
pub struct Outcome {
    pub stdout: String,
    pub code: u8,
}

/// A refusal or failure, said in a way an agent can act on.
struct Problem {
    code: String,
    message: String,
    /// The server's own fields about it (field errors, a settings address).
    detail: Vec<(String, Value)>,
    help: Vec<String>,
    usage: bool,
}

impl Problem {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            detail: Vec::new(),
            help: Vec::new(),
            usage: false,
        }
    }
    fn usage(message: impl Into<String>) -> Self {
        Self {
            usage: true,
            help: vec!["Run `bilibi-axi --help` for every command".to_owned()],
            ..Self::new("USAGE", message)
        }
    }
    fn help(mut self, line: impl Into<String>) -> Self {
        self.help.push(line.into());
        self
    }
    fn with(mut self, name: &str, value: Value) -> Self {
        if !value.is_null() {
            self.detail.push((name.to_owned(), value));
        }
        self
    }
}

impl From<String> for Problem {
    fn from(message: String) -> Self {
        Self::new("LOCAL", message)
    }
}

struct Reply {
    doc: Doc,
    /// What `--json` prints.
    raw: Value,
}

type Done = Result<Reply, Problem>;

/// Runs one command.
pub async fn run(invocation: Invocation) -> Outcome {
    let mut args = invocation.args;
    let json = flag(&mut args, "--json");
    if flag(&mut args, "--help") || args.first().is_some_and(|first| first == "help") {
        return Outcome {
            stdout: USAGE.to_owned(),
            code: 0,
        };
    }
    let done = match Cli::new(&mut args, invocation.home, invocation.self_report) {
        Ok(mut cli) => {
            cli.step_up_code = invocation.step_up_code;
            cli.stdin = invocation.stdin;
            cli.dispatch(args).await
        }
        Err(problem) => Err(problem),
    };
    match done {
        Ok(reply) => Outcome {
            stdout: if json {
                format!("{:#}\n", reply.raw)
            } else {
                reply.doc.render()
            },
            code: 0,
        },
        Err(problem) => {
            let code = if problem.usage { 2 } else { 1 };
            let stdout = if json {
                let mut body = Map::new();
                body.insert("error".into(), json!(problem.message));
                body.insert("code".into(), json!(problem.code));
                body.extend(problem.detail);
                body.insert("help".into(), json!(problem.help));
                format!("{:#}\n", Value::Object(body))
            } else {
                let mut doc = Doc::default()
                    .put("error", problem.message)
                    .put("code", problem.code);
                for (name, value) in problem.detail {
                    doc = doc.put(&name, value);
                }
                doc.help(&problem.help).render()
            };
            Outcome { stdout, code }
        }
    }
}

/// Removes `name` from the arguments and says whether it was there.
fn flag(args: &mut Vec<String>, name: &str) -> bool {
    let before = args.len();
    args.retain(|arg| arg != name);
    args.len() != before
}

/// Removes `name <value>` from the arguments.
fn option(args: &mut Vec<String>, name: &str) -> Result<Option<String>, Problem> {
    let Some(at) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    if at + 1 >= args.len() {
        return Err(Problem::usage(format!("{name} needs a value")));
    }
    args.remove(at);
    Ok(Some(args.remove(at)))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// `field=value` pairs as one input object.
fn pairs(args: &[String]) -> Result<Map<String, Value>, Problem> {
    let mut input = Map::new();
    for arg in args {
        let Some((name, text)) = arg.split_once('=') else {
            return Err(Problem::usage(format!("`{arg}` is not field=value")));
        };
        if name.is_empty() || input.contains_key(name) {
            return Err(Problem::usage(format!(
                "`{name}` is given twice or is empty"
            )));
        }
        let value = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()));
        input.insert(name.to_owned(), value);
    }
    Ok(input)
}

struct Cli {
    wire: Wire,
    store: Store,
    /// `--server`, else the last sign-in.
    server: Option<String>,
    step_up_code: Option<String>,
    stdin: Option<String>,
}

impl Cli {
    fn new(
        args: &mut Vec<String>,
        home: PathBuf,
        self_report: Option<String>,
    ) -> Result<Self, Problem> {
        Ok(Self {
            server: option(args, "--server")?,
            wire: Wire::new(self_report)?,
            store: Store::new(home),
            step_up_code: None,
            stdin: None,
        })
    }

    async fn dispatch(&mut self, mut args: Vec<String>) -> Done {
        if args.is_empty() {
            return self.home().await;
        }
        let command = args.remove(0);
        match command.as_str() {
            "login" => self.login(args).await,
            "logout" => self.logout(&args),
            "hello" => self.hello(&args).await,
            "search" => self.search(args).await,
            "describe" => self.describe(args).await,
            "guide" => self.guide(&args).await,
            "read" => self.read(args).await,
            "plan" => self.plan(args).await,
            "commit" => self.commit(args).await,
            other => Err(Problem::usage(format!("`{other}` is not a command"))),
        }
    }

    // ---- reading a JSON argument ----------------------------------------------

    /// `<json>`, `@file` or `-` (standard input).
    fn text(&mut self, source: &str) -> Result<String, Problem> {
        if source == "-" {
            return self.stdin.take().ok_or_else(|| {
                Problem::usage("`-` reads standard input once, and nothing was piped in")
            });
        }
        match source.strip_prefix('@') {
            Some(path) => std::fs::read_to_string(path)
                .map_err(|_| Problem::usage(format!("could not read {path}"))),
            None => Ok(source.to_owned()),
        }
    }

    /// The input object of one operation: `--input`, else the pairs.
    fn input(&mut self, args: &mut Vec<String>) -> Result<Value, Problem> {
        match option(args, "--input")? {
            Some(source) => {
                if !args.is_empty() {
                    return Err(Problem::usage(
                        "give --input or field=value pairs, not both",
                    ));
                }
                match serde_json::from_str(&self.text(&source)?) {
                    Ok(input @ Value::Object(_)) => Ok(input),
                    _ => Err(Problem::usage("--input must be one JSON object")),
                }
            }
            None => Ok(Value::Object(pairs(args)?)),
        }
    }

    /// The ordered operations of `plan` and `commit`: `--file`, else one
    /// operation from a key and its input.
    fn operations(&mut self, mut args: Vec<String>) -> Result<Value, Problem> {
        if let Some(file) = option(&mut args, "--file")? {
            if !args.is_empty() {
                return Err(Problem::usage("give --file or one operation, not both"));
            }
            let source = if file == "-" {
                file
            } else {
                format!("@{file}")
            };
            return match serde_json::from_str(&self.text(&source)?) {
                Ok(list @ Value::Array(_)) => Ok(list),
                _ => Err(Problem::usage(
                    "the file must be a JSON list of {\"key\":..,\"input\":{..}} steps",
                )),
            };
        }
        if args.is_empty() {
            return Err(Problem::usage(
                "name the operation: <key> [field=value ...]",
            ));
        }
        let key = args.remove(0);
        let input = self.input(&mut args)?;
        Ok(json!([{"key": key, "input": input}]))
    }
}
