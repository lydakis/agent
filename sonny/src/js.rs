//! The vendored renderers (Pele for Mermaid, MathJax for LaTeX) are
//! JavaScript, run by the QuickJS Sonny already carries for exec, so a
//! picture needs nothing on the machine. Each bundle is read once, on the
//! first picture that needs it, into an engine on its own thread; every
//! later picture only renders, which takes milliseconds.
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Mutex, OnceLock,
        mpsc::{self, Sender},
    },
    time::{Duration, Instant},
};

use rquickjs::{Context, Ctx, Runtime};

/// How long a picture may take: five seconds, so a picture that will not
/// come never holds a reply up. Tests share busy runners, and check what
/// is drawn, not how fast.
const LIMIT: Duration = Duration::from_secs(if cfg!(test) { 60 } else { 5 });

/// What one engine may hold: MathJax with a large formula stays well
/// under a tenth of this.
const MEMORY: usize = 256 << 20;
const STACK: usize = 8 << 20;

type Job = (String, Sender<Result<String, String>>);

/// A bundle and the engine that runs it, started on first use.
pub struct Engine {
    what: &'static str,
    bundle: &'static str,
    limit: Duration,
    jobs: OnceLock<Mutex<Sender<Job>>>,
}

impl Engine {
    pub const fn new(what: &'static str, bundle: &'static str) -> Self {
        Self {
            what,
            bundle,
            limit: LIMIT,
            jobs: OnceLock::new(),
        }
    }

    /// The string `expression` evaluates to, with the bundle loaded, or
    /// what it threw. At most [`LIMIT`].
    pub fn eval(&self, expression: String) -> Result<String, String> {
        let jobs = self.jobs.get_or_init(|| {
            let (send, jobs) = mpsc::channel::<Job>();
            let (what, bundle, limit) = (self.what, self.bundle, self.limit);
            std::thread::Builder::new()
                .name(format!("sonny-js-{}", what.to_lowercase()))
                .spawn(move || serve(what, bundle, limit, jobs))
                .expect("a thread for drawing");
            Mutex::new(send)
        });
        let (reply, answer) = mpsc::channel();
        jobs.lock()
            .map_err(|_| format!("drawing {} stopped", self.what))?
            .send((expression, reply))
            .map_err(|_| format!("drawing {} stopped", self.what))?;
        answer
            .recv()
            .map_err(|_| format!("drawing {} stopped", self.what))?
    }
}

/// One engine's life: load on the first job, then answer each in turn.
/// An engine whose job failed may be left half changed (stopped
/// mid-render for time or memory), so the job after loads it again.
fn serve(what: &str, bundle: &str, limit: Duration, jobs: mpsc::Receiver<Job>) {
    let deadline = Rc::new(Cell::new(Instant::now()));
    let mut loaded: Option<(Runtime, Context)> = None;
    for (expression, reply) in jobs {
        deadline.set(Instant::now() + limit);
        let out = match loaded.take().map_or_else(|| load(bundle, &deadline), Ok) {
            Ok((runtime, context)) => {
                let out = context.with(|ctx| {
                    ctx.eval::<String, _>(expression)
                        .map_err(|error| caught(&ctx, error))
                });
                if Instant::now() > deadline.get() {
                    Err(format!("{what} took longer than {}s", limit.as_secs_f32()))
                } else {
                    if out.is_ok() {
                        loaded = Some((runtime, context));
                    }
                    out
                }
            }
            Err(error) => Err(format!("{what}: {error}")),
        };
        let _ = reply.send(out);
    }
}

fn load(bundle: &str, deadline: &Rc<Cell<Instant>>) -> Result<(Runtime, Context), String> {
    let runtime = Runtime::new().map_err(|e| e.to_string())?;
    runtime.set_memory_limit(MEMORY);
    runtime.set_max_stack_size(STACK);
    let deadline = Rc::clone(deadline);
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() > deadline.get())));
    let context = Context::full(&runtime).map_err(|e| e.to_string())?;
    context.with(|ctx| {
        ctx.eval::<(), _>(bundle)
            .map_err(|error| caught(&ctx, error))
    })?;
    Ok((runtime, context))
}

/// What was thrown, as its message.
fn caught(ctx: &Ctx<'_>, error: rquickjs::Error) -> String {
    let thrown = ctx.catch();
    match thrown.as_exception() {
        Some(exception) => exception.message().unwrap_or_else(|| "failed".into()),
        None => match error {
            rquickjs::Error::Exception => thrown
                .as_string()
                .and_then(|s| s.to_string().ok())
                .unwrap_or_else(|| format!("{thrown:?}")),
            error => error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUNDLE: &str = "var drawn = 0; \
        function draw(x) { drawn += 1; if (!x) throw new Error('nothing to draw'); return x + drawn; }";

    #[test]
    fn an_engine_keeps_its_bundle_until_a_job_fails() {
        static ENGINE: Engine = Engine::new("test", BUNDLE);
        assert_eq!(ENGINE.eval("draw('a')".into()), Ok("a1".into()));
        assert_eq!(ENGINE.eval("draw('b')".into()), Ok("b2".into()));
        assert_eq!(
            ENGINE.eval("draw('')".into()),
            Err("nothing to draw".into())
        );
        assert_eq!(ENGINE.eval("draw('c')".into()), Ok("c1".into()));
        assert!(ENGINE.eval("draw(".into()).is_err());
    }

    #[test]
    fn a_job_that_will_not_end_is_stopped() {
        static ENGINE: Engine = Engine {
            what: "test",
            bundle: BUNDLE,
            limit: Duration::from_millis(200),
            jobs: OnceLock::new(),
        };
        let started = Instant::now();
        let out = ENGINE.eval("for (;;) {}".into());
        assert_eq!(out, Err("test took longer than 0.2s".into()));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(ENGINE.eval("draw('d')".into()), Ok("d1".into()));
    }
}
