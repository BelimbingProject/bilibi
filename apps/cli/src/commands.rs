//! The commands: each sends one request to the versioned API and prints the
//! answer, with the next commands worth running.
use crate::{flag, now, option, toon::Doc, wire, Cli, Done, Problem, Reply};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// The same value with every object's fields in name order, so two spellings
/// of one plan hash alike.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut fields: Vec<(&String, &Value)> = map.iter().collect();
            fields.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                fields
                    .into_iter()
                    .map(|(name, value)| (name.clone(), canonical(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// The ref `plan` prints and `commit` asks back: the whole SHA-256 of what
/// the person reviewed and where. It binds the address and the sign-in the
/// plan was made with, the ordered operations, and the server's review
/// itself (every preview, the company it resolved and the instance), so
/// `commit` goes ahead only when the same sign-in would still get the same
/// review for the same operations. The digest is not cut short: a ref that
/// two different plans share must not be findable. The server rechecks every
/// operation when it commits whatever the ref says.
fn plan_ref(address: &str, sign_in: &str, operations: &Value, review: &Value) -> String {
    let bound = json!({
        "version": 1,
        "address": address,
        "sign_in": sign_in,
        "operations": operations,
        "review": review,
    });
    Sha256::digest(canonical(&bound).to_string().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Cli {
    pub(crate) async fn home(&self) -> Done {
        let mut doc = Doc::default().put("bin", "bilibi-axi").put(
            "description",
            "Bilibi's operations from the command line: search, read, and reviewed writes, as the signed-in person.",
        );
        let mut raw = Map::new();
        let saved = match self.name() {
            Some(name) => self.store.load(&name)?,
            None => None,
        };
        let mut help = Vec::new();
        match &saved {
            None => {
                doc = doc.put("sign_in", "none");
                raw.insert("sign_in".into(), json!("none"));
                help.push("Run `bilibi-axi login <address>` to sign in, with the Bilibi's address such as https://bilibi.example.com".to_owned());
            }
            Some(saved) => {
                doc = doc
                    .put("server", saved.server_name.clone())
                    .put("address", saved.origin.clone());
                raw.insert("server".into(), json!(saved.server_name));
                raw.insert("address".into(), json!(saved.origin));
                let state = if saved.refresh_token.is_some() {
                    match self.api("/hello", None).await {
                        Ok(hello) => {
                            let notice = hello.pointer("/instance/notice").cloned();
                            doc = doc.put("sign_in", "signed in").put_some("instance", notice);
                            raw.insert("hello".into(), hello);
                            help.push("Run `bilibi-axi search <words>` to find operations, guides and pages".to_owned());
                            help.push("Run `bilibi-axi read <key> [field=value ...]` to call a read operation".to_owned());
                            help.push("Run `bilibi-axi plan <key> [field=value ...]` to check a write before the person approves it".to_owned());
                            "signed in"
                        }
                        Err(problem) => {
                            doc = doc
                                .put("sign_in", "saved, but the check failed")
                                .put("check", format!("{}: {}", problem.code, problem.message));
                            for (name, value) in problem.detail {
                                doc = doc.put(&name, value);
                            }
                            help.extend(problem.help);
                            "check failed"
                        }
                    }
                } else if let Some(pending) =
                    saved.pending.as_ref().filter(|p| p.expires_at > now())
                {
                    doc = doc
                        .put("sign_in", "waiting for approval")
                        .put("code", pending.user_code.clone())
                        .put("open", pending.link.clone());
                    help.push(
                        "Ask the person to open the link, check the code and approve".to_owned(),
                    );
                    help.push("Run `bilibi-axi login --wait` to finish signing in".to_owned());
                    "waiting for approval"
                } else {
                    doc = doc.put("sign_in", "none");
                    help.push(format!(
                        "Run `bilibi-axi login {}` to sign in",
                        saved.origin
                    ));
                    "none"
                };
                raw.insert("sign_in".into(), json!(state));
            }
        }
        help.push("Run `bilibi-axi --help` for every command".to_owned());
        Ok(Reply {
            doc: doc.help(&help),
            raw: Value::Object(raw),
        })
    }

    pub(crate) async fn hello(&self, args: &[String]) -> Done {
        let path = if args.is_empty() {
            "/hello".to_owned()
        } else {
            format!("/hello?message={}", wire::query(&args.join(" ")))
        };
        let hello = self.api(&path, None).await?;
        Ok(Reply {
            doc: Doc::default()
                .put_some("message", hello.get("message").cloned())
                .put_some("server", hello.get("server_name").cloned())
                .put_some("instance", hello.pointer("/instance/notice").cloned())
                .help(&[
                    "The connection and the sign-in work; a refusal from another command is about permission or input".to_owned(),
                ]),
            raw: hello,
        })
    }

    pub(crate) async fn search(&self, mut args: Vec<String>) -> Done {
        let full = flag(&mut args, "--full");
        let unavailable = flag(&mut args, "--unavailable");
        let kind = option(&mut args, "--kind")?;
        let limit = option(&mut args, "--limit")?;
        if args.is_empty() {
            return Err(Problem::usage("give the words to search for"));
        }
        let mut path = format!("/search?q={}", wire::query(&args.join(" ")));
        for (name, value) in [("kind", kind), ("limit", limit)] {
            if let Some(value) = value {
                path.push_str(&format!("&{name}={}", wire::query(&value)));
            }
        }
        if unavailable {
            path.push_str("&include_unavailable=true");
        }
        let found = self.api(&path, None).await?;
        if full {
            return Ok(Reply {
                doc: fields(Doc::default(), &found, &["result", "pages"]),
                raw: found,
            });
        }
        let rows = |name: &str| {
            found
                .get(name)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let cell = |row: &Value, name: &str| row.get(name).cloned().unwrap_or(Value::Null);
        let (mut operations, mut guides) = (Vec::new(), Vec::new());
        for row in rows("result") {
            if row.get("type").and_then(Value::as_str) == Some("guide") {
                guides.push(vec![cell(&row, "key"), cell(&row, "title")]);
            } else if unavailable {
                operations.push(vec![
                    cell(&row, "key"),
                    cell(&row, "kind"),
                    cell(&row, "title"),
                    cell(&row, "available"),
                    cell(&row, "reason"),
                ]);
            } else {
                operations.push(vec![
                    cell(&row, "key"),
                    cell(&row, "kind"),
                    cell(&row, "title"),
                ]);
            }
        }
        let pages: Vec<Vec<Value>> = rows("pages")
            .iter()
            .map(|row| vec![cell(row, "title"), cell(row, "url")])
            .collect();
        let mut help = Vec::new();
        if operations.is_empty() && guides.is_empty() && pages.is_empty() {
            help.push("Nothing matched; try other words, or `--unavailable` to include what this account cannot use".to_owned());
        }
        if !operations.is_empty() {
            help.push("Run `bilibi-axi describe <key>` for an operation's inputs".to_owned());
            help.push("Run `bilibi-axi read <key> [field=value ...]` for a read, or `bilibi-axi plan <key> [field=value ...]` for a write".to_owned());
        }
        if !guides.is_empty() {
            help.push("Run `bilibi-axi guide <key>` to read a guide".to_owned());
        }
        let columns: &[&'static str] = if unavailable {
            &["key", "kind", "title", "available", "reason"]
        } else {
            &["key", "kind", "title"]
        };
        Ok(Reply {
            doc: Doc::default()
                .table("operations", columns, operations)
                .table("guides", &["key", "title"], guides)
                .table("pages", &["title", "url"], pages)
                .help(&help),
            raw: found,
        })
    }

    pub(crate) async fn describe(&self, mut args: Vec<String>) -> Done {
        let full = flag(&mut args, "--full");
        let [key] = args.as_slice() else {
            return Err(Problem::usage("give one operation key"));
        };
        let described = self
            .api(&format!("/operations/{}", wire::query(key)), None)
            .await?;
        let kind = described
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let next = if kind == "write" {
            format!("Run `bilibi-axi plan {key} [field=value ...]` to check this write before the person approves it")
        } else {
            format!("Run `bilibi-axi read {key} [field=value ...]` to call it")
        };
        if full {
            return Ok(Reply {
                doc: fields(
                    Doc::default(),
                    &described,
                    &["key", "title", "kind", "summary"],
                )
                .help(&[next]),
                raw: described,
            });
        }
        let mut doc = Doc::default();
        for name in [
            "key",
            "title",
            "kind",
            "summary",
            "capability",
            "step_up",
            "creates",
        ] {
            if let Some(value) = described.get(name).filter(|value| !value.is_null()) {
                if value.as_array().is_some_and(Vec::is_empty) {
                    continue;
                }
                doc = doc.put(name, value.clone());
            }
        }
        let guides: Vec<Vec<Value>> = described
            .get("guides")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|guide| vec![guide["key"].clone(), guide["title"].clone()])
            .collect();
        let mut help = vec![next];
        if !guides.is_empty() {
            help.push("Run `bilibi-axi guide <key>` for what the fields mean".to_owned());
        }
        help.push(format!(
            "Run `bilibi-axi describe {key} --full` for the whole schema"
        ));
        Ok(Reply {
            doc: doc
                .table(
                    "input",
                    &["field", "type", "required", "about"],
                    inputs(&described),
                )
                .table("guides", &["key", "title"], guides)
                .help(&help),
            raw: described,
        })
    }

    pub(crate) async fn guide(&self, args: &[String]) -> Done {
        let [key] = args else {
            return Err(Problem::usage("give one guide key"));
        };
        let guide = self
            .api(&format!("/guides/{}", wire::query(key)), None)
            .await?;
        Ok(Reply {
            doc: fields(Doc::default(), &guide, &["key", "title", "summary"]),
            raw: guide,
        })
    }

    /// An `invalid_input` refusal with the operation's fields beside it, so
    /// the next call can be right without a separate `describe`.
    pub(crate) async fn with_inputs(&self, key: &str, mut problem: Problem) -> Problem {
        if problem.code == "INVALID_INPUT" {
            if let Ok(described) = self
                .api(&format!("/operations/{}", wire::query(key)), None)
                .await
            {
                let rows: Vec<Value> = inputs(&described)
                    .into_iter()
                    .map(|row| json!({"field": row[0], "type": row[1], "required": row[2]}))
                    .collect();
                problem = problem.with("input", Value::Array(rows));
            }
            problem = problem.help(format!(
                "Run `bilibi-axi describe {key}` for what each field means"
            ));
        }
        problem
    }

    pub(crate) async fn read(&mut self, mut args: Vec<String>) -> Done {
        if args.is_empty() {
            return Err(Problem::usage(
                "name the operation: read <key> [field=value ...]",
            ));
        }
        let key = args.remove(0);
        let input = self.input(&mut args)?;
        let path = format!("/operations/{}/call", wire::query(&key));
        match self.api(&path, Some(&json!({"input": input}))).await {
            Ok(answer) => Ok(Reply {
                doc: fields(Doc::default(), &answer, &["result"]),
                raw: answer,
            }),
            Err(problem) => Err(self.with_inputs(&key, problem).await),
        }
    }

    pub(crate) async fn plan(&mut self, args: Vec<String>) -> Done {
        let operations = self.operations(args)?;
        let first = operations
            .pointer("/0/key")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let single = operations.as_array().is_some_and(|list| list.len() == 1);
        let preview = match self
            .api("/plans", Some(&json!({"operations": operations})))
            .await
        {
            Ok(preview) => preview,
            Err(problem) => {
                return Err(match first {
                    Some(key) if single => self.with_inputs(&key, problem).await,
                    _ => problem,
                })
            }
        };
        let valid = preview
            .get("valid")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut raw = preview.clone();
        let mut doc = Doc::default().put("valid", valid);
        let help = if valid {
            let (address, sign_in) = self.bound_to()?;
            let reference = plan_ref(&address, &sign_in, &operations, &preview);
            raw["ref"] = json!(reference);
            doc = doc.put("ref", reference);
            vec![
                "Show the person this whole review, naming the instance, and wait for their explicit approval".to_owned(),
                "Then run `bilibi-axi commit <ref>` followed by the same operation arguments. It is refused as a stale ref if the operations, the sign-in or what Bilibi would write has changed since".to_owned(),
            ]
        } else {
            vec![
                "Nothing can be committed from this plan; correct the refused steps and plan again"
                    .to_owned(),
            ]
        };
        doc = doc.put_some("instance", preview.pointer("/instance/notice").cloned());
        Ok(Reply {
            doc: fields(doc, &preview, &["results"]).help(&help),
            raw,
        })
    }

    pub(crate) async fn commit(&mut self, mut args: Vec<String>) -> Done {
        if args.is_empty() {
            return Err(Problem::usage(
                "give the ref `plan` printed, then the same operations: commit <ref> <key> [field=value ...]",
            ));
        }
        let reference = args.remove(0);
        // The person's 2FA code comes on standard input: never an argument,
        // never a variable of the environment.
        let code = if flag(&mut args, "--code-stdin") {
            let text = self.stdin.take().unwrap_or_default();
            let code = text.lines().next().unwrap_or_default().trim().to_owned();
            if code.is_empty() {
                return Err(Problem::usage(
                    "--code-stdin reads the 2FA code from standard input, and nothing was given there",
                ));
            }
            Some(code)
        } else {
            None
        };
        let operations = self.operations(args)?;
        // Ask for the review again and commit only if it is the one the
        // person saw: same operations, same sign-in, same answer.
        let review = self
            .api("/plans", Some(&json!({"operations": operations})))
            .await?;
        if review.get("valid").and_then(Value::as_bool) != Some(true) {
            return Err(Problem::new(
                "PLAN_REJECTED",
                "Bilibi no longer accepts this plan; nothing was written",
            )
            .with(
                "results",
                review.get("results").cloned().unwrap_or_default(),
            )
            .help("Run `bilibi-axi plan` again and show the person the new review"));
        }
        let (address, sign_in) = self.bound_to()?;
        if plan_ref(&address, &sign_in, &operations, &review) != reference {
            return Err(Problem::new(
                "STALE_REF",
                "This is not what was reviewed under that ref: the operations, the sign-in, or what Bilibi would now write has changed. Nothing was written",
            )
            .help("Run `bilibi-axi plan` with these operations, show the person the new review, and commit with the ref it prints"));
        }
        let mut body = json!({"operations": operations});
        if let Some(code) = code {
            body["step_up_code"] = json!(code);
        }
        let committed = self.api("/plans/commit", Some(&body)).await?;
        let doc = Doc::default()
            .put("committed", true)
            .put_some("instance", committed.pointer("/instance/notice").cloned());
        Ok(Reply {
            doc: fields(doc, &committed, &["results"]).help(&[
                "Tell the person what was saved, naming the instance, with the links in the results".to_owned(),
            ]),
            raw: committed,
        })
    }
}

/// The fields of a server answer: the named ones first, then the rest that
/// the caller has not printed already. `instance` is left to the caller, who
/// prints its notice.
fn fields(mut doc: Doc, answer: &Value, first: &[&str]) -> Doc {
    let Some(map) = answer.as_object() else {
        return doc.put("result", answer.clone());
    };
    for name in first {
        if let Some(value) = map.get(*name) {
            doc = doc.put(name, value.clone());
        }
    }
    for (name, value) in map {
        if !first.contains(&name.as_str()) && name != "instance" && !doc.has(name) {
            doc = doc.put(name, value.clone());
        }
    }
    doc
}

/// One row per input field of a described operation: field, type, required, about.
fn inputs(described: &Value) -> Vec<Vec<Value>> {
    let required: Vec<&str> = described
        .pointer("/input/required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    described
        .pointer("/input/properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, schema)| {
            let mut kind = schema
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
                let choices: Vec<String> = choices
                    .iter()
                    .map(|choice| {
                        choice
                            .as_str()
                            .map_or_else(|| choice.to_string(), str::to_owned)
                    })
                    .collect();
                kind = format!("{kind}: {}", choices.join(" | "));
            }
            let about = schema
                .get("description")
                .or_else(|| schema.get("title"))
                .cloned()
                .unwrap_or(Value::Null);
            vec![
                json!(name),
                json!(kind),
                json!(required.contains(&name.as_str())),
                about,
            ]
        })
        .collect()
}
