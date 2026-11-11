//! A workflow's script: Claude Code's dynamic workflow language, name for
//! name, run in the QuickJS that `exec` runs scripts in. The script opens
//! with `export const meta = {...}`, a pure literal, and its body runs as
//! an async function's, with `agent`, `parallel`, `pipeline`, `phase`,
//! `log`, `args`, `budget` and `workflow` as globals. `Date.now()`,
//! `Math.random()` and `new Date()` with no argument throw, as there, so a
//! replay of a run takes the same branches.
use rquickjs::{AsyncContext, AsyncRuntime, Context, Function, Promise, Runtime, prelude::Async};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The most a plan's heap holds: a plan mostly waits on its agents, and
/// holds their results, not an MCP server's.
const MEMORY: usize = 64 << 20;

/// The most a script's file is, as read.
pub const SCRIPT_BYTES: u64 = 1 << 20;

/// How long reading a script's `meta` may take.
const META_TIME: Duration = Duration::from_secs(1);

/// A script read and checked: its `meta`, and its body as it runs, `export`
/// taken off so the body is a function's.
#[derive(Clone, Debug)]
pub struct Script {
    pub meta: Value,
    pub body: String,
}

impl Script {
    pub fn name(&self) -> &str {
        self.meta["name"].as_str().unwrap_or("workflow")
    }
}

/// `source` read as a workflow script: an `export const meta = {...}`
/// first, a pure literal with a `name` and a `description`, and a body
/// that compiles. What is wrong is said by name.
pub fn read(source: &str) -> Result<Script, String> {
    let (export, from, to) = meta_span(source)?;
    // Spaces where `export` was, so the body's lines and columns hold.
    let body = format!(
        "{}{}{}",
        &source[..export],
        " ".repeat("export".len()),
        &source[export + "export".len()..]
    );
    let literal = &source[from..to];
    let runtime = Runtime::new().map_err(|e| e.to_string())?;
    runtime.set_memory_limit(16 << 20);
    let deadline = Instant::now() + META_TIME;
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() > deadline)));
    let context = Context::full(&runtime).map_err(|e| e.to_string())?;
    let meta = context.with(|ctx| -> Result<Value, String> {
        let value: rquickjs::Value = ctx
            .eval(format!("({literal})"))
            .map_err(|e| format!("meta: {}", caught(&ctx, e)))?;
        let text = ctx
            .json_stringify(value)
            .map_err(|e| format!("meta: {}", caught(&ctx, e)))?
            .ok_or("meta must be an object literal")?
            .to_string()
            .map_err(|e| e.to_string())?;
        // Compiled, not run: a syntax error is the caller's to fix now.
        ctx.eval::<(), _>(format!("(async () => {{{body}\n}});"))
            .map_err(|e| caught(&ctx, e))?;
        serde_json::from_str(&text).map_err(|e| e.to_string())
    })?;
    checked(&meta)?;
    Ok(Script { meta, body })
}

/// Whether `meta` has what Claude Code's does: a `name` and a
/// `description`, strings, and optionally `whenToUse` and `phases`, each
/// phase with a `title`.
fn checked(meta: &Value) -> Result<(), String> {
    let text = |key: &str| meta[key].as_str().filter(|text| !text.trim().is_empty());
    if text("name").is_none() || text("description").is_none() {
        return Err("meta needs name and description, each a string".into());
    }
    if !(meta["whenToUse"].is_null() || meta["whenToUse"].is_string()) {
        return Err("meta.whenToUse is a string".into());
    }
    match &meta["phases"] {
        Value::Null => Ok(()),
        Value::Array(phases) if phases.iter().all(|p| p["title"].is_string()) => Ok(()),
        _ => Err("meta.phases is a list of {title, detail?, model?}".into()),
    }
}

/// Where `export const meta = {...}` is in `source`: where `export` starts,
/// and the object literal's first and past-last bytes. It comes first,
/// after comments; its literal has no `${...}` in it.
fn meta_span(source: &str) -> Result<(usize, usize, usize), String> {
    const NEEDS: &str =
        "a workflow script begins with export const meta = { name, description, ... }";
    let bytes = source.as_bytes();
    let mut at = skip_blank(bytes, 0);
    let export = at;
    for word in ["export", "const", "meta"] {
        if !source[at..].starts_with(word) {
            return Err(NEEDS.into());
        }
        at = skip_blank(bytes, at + word.len());
    }
    if bytes.get(at) != Some(&b'=') {
        return Err(NEEDS.into());
    }
    at = skip_blank(bytes, at + 1);
    if bytes.get(at) != Some(&b'{') {
        return Err("meta must be an object literal".into());
    }
    let from = at;
    let mut depth = 0usize;
    while at < bytes.len() {
        match bytes[at] {
            b'{' | b'[' | b'(' => depth += 1,
            b'}' | b']' | b')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((export, from, at + 1));
                }
            }
            quote @ (b'"' | b'\'' | b'`') => {
                at += 1;
                while at < bytes.len() && bytes[at] != quote {
                    if bytes[at] == b'\\' {
                        at += 1;
                    } else if quote == b'`' && bytes[at] == b'$' && bytes.get(at + 1) == Some(&b'{')
                    {
                        return Err("meta must be a pure literal: no ${...} in it".into());
                    }
                    at += 1;
                }
            }
            b'/' if matches!(bytes.get(at + 1), Some(b'/' | b'*')) => {
                at = skip_blank(bytes, at) - 1;
            }
            _ => {}
        }
        at += 1;
    }
    Err("meta's literal never closes".into())
}

/// The first byte at or past `at` that is neither space nor comment.
fn skip_blank(bytes: &[u8], mut at: usize) -> usize {
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        match (bytes.get(at), bytes.get(at + 1)) {
            (Some(b'/'), Some(b'/')) => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            (Some(b'/'), Some(b'*')) => {
                at += 2;
                while at + 1 < bytes.len() && !(bytes[at] == b'*' && bytes[at + 1] == b'/') {
                    at += 1;
                }
                at = (at + 2).min(bytes.len());
            }
            _ => return at,
        }
    }
}

/// What was thrown, as its message and stack.
fn caught(ctx: &rquickjs::Ctx<'_>, error: rquickjs::Error) -> String {
    let thrown = ctx.catch();
    match thrown.as_exception() {
        Some(exception) => {
            let message = exception.message().unwrap_or_default();
            let name: String = exception.get("name").unwrap_or_else(|_| "Error".into());
            match exception.stack() {
                Some(stack) if !stack.trim().is_empty() => {
                    format!("{name}: {message}\n{}", stack.trim_end())
                }
                _ => format!("{name}: {message}"),
            }
        }
        None => match error {
            rquickjs::Error::Exception => format!("{thrown:?}"),
            error => error.to_string(),
        },
    }
}

/// What a script asks of the run it is in.
pub trait Host: Send + Sync + 'static {
    /// `agent(prompt, opts)`: the worker's value, or why it throws.
    fn agent(
        self: Arc<Self>,
        prompt: String,
        opts: Value,
    ) -> impl std::future::Future<Output = Result<Value, String>> + Send;
    fn phase(&self, title: &str);
    fn log(&self, text: &str);
    /// Output tokens this run's agents spent.
    fn spent(&self) -> u64;
    /// The body of the workflow `reference` names, for `workflow()`.
    fn load(&self, reference: &Value) -> Result<Script, String>;
    /// Whether the run is to stop: the script is interrupted.
    fn halted(&self) -> bool;
    /// Resolves once the run is to stop.
    fn stopped(&self) -> impl std::future::Future<Output = ()> + Send;
}

/// The globals a script sees, made from the host's `__` functions: the
/// whole of Claude Code's workflow API, and nothing that would break a
/// replay.
const PRELUDE: &str = r#"(() => {
  const host = { agent: __agent, phase: __phase, log: __log, spent: __spent, load: __load };
  for (const name of ["__agent", "__phase", "__log", "__spent", "__load"]) delete globalThis[name];
  const TOTAL = __total;
  delete globalThis.__total;
  const ITEMS = 4096;
  const show = (value) => typeof value === "string" ? value : (JSON.stringify(value) ?? String(value));
  const said = (raw) => { const said = JSON.parse(raw); if ("error" in said) throw new Error(said.error); return said.ok; };
  let current = null;
  globalThis.phase = (title) => { current = String(title); host.phase(current); };
  globalThis.log = (message) => { host.log(show(message)); };
  globalThis.agent = async (prompt, opts) => {
    if (typeof prompt !== "string") throw new TypeError("agent() takes a prompt string");
    const asked = Object.assign({}, opts ?? {});
    if (asked.phase === undefined && current !== null) asked.phase = current;
    return said(await host.agent(prompt, JSON.stringify(asked)));
  };
  const items = (list, what) => {
    if (!Array.isArray(list)) throw new TypeError(`${what}() takes an array`);
    if (list.length > ITEMS) throw new RangeError(`${what}() accepts at most ${ITEMS} items; got ${list.length}`);
  };
  globalThis.parallel = async (thunks) => {
    items(thunks, "parallel");
    return Promise.all(thunks.map((thunk) => Promise.resolve().then(() => thunk()).catch(() => null)));
  };
  globalThis.pipeline = async (list, ...stages) => {
    items(list, "pipeline");
    return Promise.all(list.map(async (item, index) => {
      try {
        let prev = item;
        for (const stage of stages) prev = await stage(prev, item, index);
        return prev;
      } catch (e) {
        return null;
      }
    }));
  };
  globalThis.budget = Object.freeze({
    total: TOTAL,
    spent: () => host.spent(),
    remaining: () => TOTAL === null ? Infinity : Math.max(0, TOTAL - host.spent()),
  });
  const AsyncFunction = (async () => {}).constructor;
  const nested = async () => { throw new Error("workflow() nests one level only"); };
  globalThis.workflow = async (reference, args) => {
    const loaded = said(host.load(JSON.stringify(reference ?? null)));
    const body = new AsyncFunction("args", "workflow", loaded.body);
    return body(args, nested);
  };
  const deny = (what) => () => { throw new Error(`${what} is unavailable in workflows, which must replay the same way; pass timestamps in through args, and vary agents by index for randomness`); };
  const RealDate = Date;
  function WorkflowDate(...given) {
    if (!new.target) deny("Date()")();
    if (given.length === 0) deny("new Date()")();
    return new RealDate(...given);
  }
  WorkflowDate.prototype = RealDate.prototype;
  WorkflowDate.UTC = RealDate.UTC;
  WorkflowDate.parse = RealDate.parse;
  WorkflowDate.now = deny("Date.now()");
  globalThis.Date = WorkflowDate;
  Math.random = deny("Math.random()");
  globalThis.__settle = (running) => running.then(
    (value) => {
      try { return JSON.stringify({ ok: value === undefined ? null : value }) ?? '{"ok":null}'; }
      catch (e) { return JSON.stringify({ error: `the workflow's result is not JSON: ${e}` }); }
    },
    (e) => JSON.stringify({ error: e instanceof Error ? `${e}${e.stack ? "\n" + e.stack.trimEnd() : ""}` : show(e) }));
})();"#;

/// How a script's run ended.
#[derive(Debug, PartialEq)]
pub enum Ended {
    /// What it returned, as JSON.
    Returned(Value),
    /// What it threw.
    Threw(String),
    /// Stopped from outside: by `stop`, or for its run to be replayed.
    Halted,
}

/// Run `script` with `args` as its `args`, on a thread and runtime of its
/// own, its calls made on `host`'s.
pub async fn run<H: Host>(host: Arc<H>, script: &Script, args: Value, total: Option<u64>) -> Ended {
    let handle = tokio::runtime::Handle::current();
    let body = script.body.clone();
    let ran = tokio::task::spawn_blocking(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|e| e.to_string())
            .map(|runtime| runtime.block_on(evaluate(host, handle, body, args, total)))
    })
    .await;
    match ran {
        Ok(Ok(ended)) => ended,
        Ok(Err(error)) => Ended::Threw(error),
        Err(error) => Ended::Threw(error.to_string()),
    }
}

async fn evaluate<H: Host>(
    host: Arc<H>,
    handle: tokio::runtime::Handle,
    body: String,
    args: Value,
    total: Option<u64>,
) -> Ended {
    let runtime = match AsyncRuntime::new() {
        Ok(runtime) => runtime,
        Err(error) => return Ended::Threw(error.to_string()),
    };
    runtime.set_memory_limit(MEMORY).await;
    let halting = Arc::clone(&host);
    runtime
        .set_interrupt_handler(Some(Box::new(move || halting.halted())))
        .await;
    let context = match AsyncContext::full(&runtime).await {
        Ok(context) => context,
        Err(error) => return Ended::Threw(error.to_string()),
    };
    let stopping = Arc::clone(&host);
    let ran = context.async_with(async |ctx| {
        if let Err(error) = globals(&ctx, &host, &handle, &args, total) {
            return Ended::Threw(caught(&ctx, error));
        }
        if let Err(error) = ctx.eval::<(), _>(PRELUDE) {
            return Ended::Threw(caught(&ctx, error));
        }
        // The body on the wrapper's first line, so its line numbers hold.
        let wrapped = format!("__settle((async () => {{{body}\n}})())");
        let promise: Promise = match ctx.eval(wrapped) {
            Ok(promise) => promise,
            Err(error) => return Ended::Threw(caught(&ctx, error)),
        };
        match promise.into_future::<String>().await {
            Ok(settled) => match serde_json::from_str::<Value>(&settled) {
                Ok(mut said) if said.get("ok").is_some() => Ended::Returned(said["ok"].take()),
                Ok(said) => Ended::Threw(said["error"].as_str().unwrap_or("failed").to_owned()),
                Err(error) => Ended::Threw(error.to_string()),
            },
            Err(error) => Ended::Threw(caught(&ctx, error)),
        }
    });
    let ended = tokio::select! {
        ended = ran => ended,
        () = stopping.stopped() => Ended::Halted,
    };
    // An interrupt ends the evaluation as an uncatchable error.
    match ended {
        Ended::Threw(_) if host.halted() => Ended::Halted,
        ended => ended,
    }
}

fn globals<'js, H: Host>(
    ctx: &rquickjs::Ctx<'js>,
    host: &Arc<H>,
    handle: &tokio::runtime::Handle,
    args: &Value,
    total: Option<u64>,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    globals.set("args", ctx.json_parse(args.to_string())?)?;
    globals.set("__total", ctx.json_parse(json!(total).to_string())?)?;
    let (asking, handle) = (Arc::clone(host), handle.clone());
    globals.set(
        "__agent",
        Function::new(
            ctx.clone(),
            Async(move |prompt: String, opts: String| {
                let (host, handle) = (Arc::clone(&asking), handle.clone());
                async move {
                    let opts = serde_json::from_str(&opts).unwrap_or(Value::Null);
                    // On the run's runtime, where its connection lives;
                    // stopped with the script, as a call it no longer
                    // awaits must not run on.
                    let asked = Abort(handle.spawn(host.agent(prompt, opts))).await;
                    let said = match asked {
                        Ok(Ok(value)) => json!({"ok": value}),
                        Ok(Err(error)) => json!({"error": error}),
                        Err(error) => json!({"error": error.to_string()}),
                    };
                    rquickjs::Result::Ok(said.to_string())
                }
            }),
        )?,
    )?;
    let phasing = Arc::clone(host);
    globals.set(
        "__phase",
        Function::new(ctx.clone(), move |title: String| phasing.phase(&title))?,
    )?;
    let logging = Arc::clone(host);
    globals.set(
        "__log",
        Function::new(ctx.clone(), move |text: String| logging.log(&text))?,
    )?;
    let spending = Arc::clone(host);
    globals.set(
        "__spent",
        Function::new(ctx.clone(), move || spending.spent() as f64)?,
    )?;
    let loading = Arc::clone(host);
    globals.set(
        "__load",
        Function::new(ctx.clone(), move |reference: String| {
            let reference = serde_json::from_str(&reference).unwrap_or(Value::Null);
            match loading.load(&reference) {
                Ok(script) => json!({"ok": {"body": script.body, "name": script.name()}}),
                Err(error) => json!({"error": error}),
            }
            .to_string()
        })?,
    )?;
    Ok(())
}

/// A task stopped once what awaits it is dropped.
struct Abort<T>(tokio::task::JoinHandle<T>);

impl<T> std::future::Future for Abort<T> {
    type Output = Result<T, tokio::task::JoinError>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}

impl<T> Drop for Abort<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    #[test]
    fn a_script_opens_with_a_pure_literal_meta() {
        let script = read(
            "// a comment first\nexport const meta = {\n  name: 'audit', description: 'Find bugs { here }',\n  phases: [{ title: 'Find' }],\n}\nreturn meta.name\n",
        )
        .unwrap();
        assert_eq!(script.meta["name"], "audit");
        assert_eq!(script.meta["phases"][0]["title"], "Find");
        // `export` became spaces: the lines and columns are the source's.
        assert!(script.body.contains("       const meta = {"));
        assert_eq!(script.body.lines().count(), 6);
        let refused = |source: &str| read(source).unwrap_err();
        assert!(refused("const x = 1").contains("begins with export const meta"));
        assert!(refused("export const meta = {name: 'a'}").contains("description"));
        assert!(refused("export const meta = {name: `a${1}`, description: 'b'}").contains("pure"));
        assert!(refused("export const meta = {name: x, description: 'b'}").contains("meta"));
        assert!(
            refused("export const meta = {name: 'a', description: 'b'}\nawait agent(")
                .contains("SyntaxError")
        );
        assert!(
            refused("export const meta = {name: 'a', description: 'b', phases: 'x'}")
                .contains("phases")
        );
        // One that never ends is cut off, not waited on.
        assert!(
            refused("export const meta = {name: (() => { while (true) {} })(), description: 'b'}")
                .contains("meta")
        );
    }

    /// A run with no daemon: each agent answers its prompt upper-cased,
    /// or as the schema's object, after `delay`.
    #[derive(Default)]
    struct Fake {
        asked: Mutex<Vec<(String, Value)>>,
        phases: Mutex<Vec<String>>,
        logs: Mutex<Vec<String>>,
        spent: AtomicU64,
        halt: AtomicBool,
        stop: tokio::sync::Notify,
    }

    impl Host for Fake {
        async fn agent(self: Arc<Self>, prompt: String, opts: Value) -> Result<Value, String> {
            self.asked
                .lock()
                .unwrap()
                .push((prompt.clone(), opts.clone()));
            tokio::time::sleep(Duration::from_millis(5)).await;
            self.spent.fetch_add(10, Ordering::SeqCst);
            if prompt == "fail" {
                return Ok(Value::Null);
            }
            if prompt == "throw" {
                return Err("budget spent".into());
            }
            Ok(match opts["schema"].is_object() {
                true => json!({"answer": prompt}),
                false => json!(prompt.to_uppercase()),
            })
        }
        fn phase(&self, title: &str) {
            self.phases.lock().unwrap().push(title.to_owned());
        }
        fn log(&self, text: &str) {
            self.logs.lock().unwrap().push(text.to_owned());
        }
        fn spent(&self) -> u64 {
            self.spent.load(Ordering::SeqCst)
        }
        fn load(&self, reference: &Value) -> Result<Script, String> {
            match reference.as_str() {
                Some("child") => read(
                    "export const meta = {name: 'child', description: 'c'}\nreturn await agent(args.q)",
                ),
                Some("deeper") => read(
                    "export const meta = {name: 'deeper', description: 'd'}\nreturn await workflow('child', {q: 'x'})",
                ),
                _ => Err("no such workflow".into()),
            }
        }
        fn halted(&self) -> bool {
            self.halt.load(Ordering::SeqCst)
        }
        async fn stopped(&self) {
            loop {
                let notified = self.stop.notified();
                if self.halted() {
                    return;
                }
                notified.await;
            }
        }
    }

    fn ran(source: &str, args: Value, total: Option<u64>) -> (Ended, Arc<Fake>) {
        let fake = Arc::new(Fake::default());
        let script = read(source).unwrap();
        let ended = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(Arc::clone(&fake), &script, args, total));
        (ended, fake)
    }

    const META: &str =
        "export const meta = {name: 't', description: 'd', phases: [{title: 'Find'}]}\n";

    #[test]
    fn the_api_is_claude_code_s() {
        let (ended, fake) = ran(
            &format!(
                "{META}phase('Find')\nlog({{n: args.length}})\n\
                 const found = await pipeline(args, m => agent(`find ${{m}}`, {{label: m}}), (prev, item, i) => `${{prev}}:${{item}}:${{i}}`)\n\
                 const all = await parallel([() => agent('a'), () => agent('fail'), () => agent('throw'), () => {{ throw new Error('x') }}])\n\
                 const typed = await agent('q', {{schema: {{type: 'object', properties: {{answer: {{type: 'string'}}}}}}}})\n\
                 return {{found, all, typed, spent: budget.spent(), left: budget.remaining(), total: budget.total}}"
            ),
            json!(["x", "y"]),
            None,
        );
        let Ended::Returned(value) = ended else {
            panic!("{ended:?}")
        };
        assert_eq!(value["found"], json!(["FIND X:x:0", "FIND Y:y:1"]));
        // A thunk that throws, or whose agent throws, is null; a null
        // agent is null.
        assert_eq!(value["all"], json!(["A", null, null, null]));
        assert_eq!(value["typed"], json!({"answer": "q"}));
        // Six agents, ten tokens each.
        assert_eq!(value["spent"], 60);
        assert_eq!(value["total"], Value::Null);
        // Infinity is no JSON: null.
        assert_eq!(value["left"], Value::Null);
        assert_eq!(*fake.phases.lock().unwrap(), vec!["Find".to_owned()]);
        assert_eq!(*fake.logs.lock().unwrap(), vec![r#"{"n":2}"#.to_owned()]);
        // The phase set before goes with each call that names none.
        let asked = fake.asked.lock().unwrap();
        assert!(asked.iter().all(|(_, opts)| opts["phase"] == "Find"));
        // Items run at once, in no fixed order.
        let mut labels: Vec<&Value> = asked.iter().map(|(_, opts)| &opts["label"]).collect();
        labels.retain(|label| !label.is_null());
        labels.sort_by_key(|label| label.to_string());
        assert_eq!(labels, [&json!("x"), &json!("y")]);
    }

    #[test]
    fn what_would_break_a_replay_throws() {
        for call in ["Date.now()", "Math.random()", "new Date()", "Date()"] {
            let (ended, _) = ran(&format!("{META}return {call}"), Value::Null, None);
            assert!(
                matches!(&ended, Ended::Threw(e) if e.contains("unavailable in workflows")),
                "{call}: {ended:?}"
            );
        }
        let (ended, _) = ran(
            &format!("{META}return new Date(0).toISOString()"),
            Value::Null,
            None,
        );
        assert_eq!(ended, Ended::Returned(json!("1970-01-01T00:00:00.000Z")));
    }

    #[test]
    fn items_past_the_bound_are_an_error_not_a_cut() {
        let (ended, fake) = ran(
            &format!(
                "{META}return await parallel(Array.from({{length: 4097}}, () => () => agent('a')))"
            ),
            Value::Null,
            None,
        );
        assert!(
            matches!(&ended, Ended::Threw(e) if e.contains("at most 4096")),
            "{ended:?}"
        );
        assert!(fake.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn a_child_workflow_runs_inline_one_level_deep() {
        let (ended, _) = ran(
            &format!("{META}return await workflow('child', {{q: 'hi'}})"),
            Value::Null,
            None,
        );
        assert_eq!(ended, Ended::Returned(json!("HI")));
        let (ended, _) = ran(
            &format!("{META}return await workflow('deeper')"),
            Value::Null,
            None,
        );
        assert!(
            matches!(&ended, Ended::Threw(e) if e.contains("one level")),
            "{ended:?}"
        );
        let (ended, _) = ran(
            &format!("{META}return await workflow('none')"),
            Value::Null,
            None,
        );
        assert!(
            matches!(&ended, Ended::Threw(e) if e.contains("no such")),
            "{ended:?}"
        );
    }

    #[test]
    fn the_budget_is_the_run_s_and_a_halt_stops_the_script() {
        let (ended, _) = ran(
            &format!("{META}await agent('a'); return [budget.total, budget.remaining()]"),
            Value::Null,
            Some(25),
        );
        assert_eq!(ended, Ended::Returned(json!([25, 15])));
        // A loop that never awaits is interrupted.
        let fake = Arc::new(Fake::default());
        let script = read(&format!("{META}while (true) {{}}")).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let halting = Arc::clone(&fake);
        runtime.spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            halting.halt.store(true, Ordering::SeqCst);
            halting.stop.notify_waiters();
        });
        let ended = runtime.block_on(run(Arc::clone(&fake), &script, Value::Null, None));
        assert_eq!(ended, Ended::Halted);
        // As is one waiting on an agent.
        let fake = Arc::new(Fake::default());
        let script = read(&format!("{META}await new Promise(() => {{}})")).unwrap();
        let halting = Arc::clone(&fake);
        runtime.spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            halting.halt.store(true, Ordering::SeqCst);
            halting.stop.notify_waiters();
        });
        let ended = runtime.block_on(run(Arc::clone(&fake), &script, Value::Null, None));
        assert_eq!(ended, Ended::Halted);
    }
}
