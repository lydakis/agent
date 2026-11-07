//! Providers bound again when the config file they came from changes, as
//! nginx reloads: the new set is built and checked beside the old, then
//! swapped in whole. Turns already running keep the providers they started
//! with (each holds its own copy of the set) and finish on them; turns that
//! start after the swap use the new ones. A binding whose spec is unchanged
//! is kept as it is, with its pacing, sockets, keys and listings. An edit
//! that does not parse or bind, or that removes a provider agents still
//! name or binds it to another API family, is refused whole and the old
//! set stays. Nothing runs between
//! changes: the file is watched by the kernel (`file_watch`).
use super::{Binder, ProviderSpec};
use agent_runtime::{Error, Result, provider::Provider, store::Store};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::mpsc;

/// The most agents a refused removal names; it counts them all.
const NAMED: usize = 20;

pub struct Reload {
    pub config: PathBuf,
    pub binder: Binder,
    /// The specs bound now, in order, as given.
    pub specs: Vec<String>,
    /// `ready.providers` as it stands.
    pub bindings: serde_json::Map<String, Value>,
    /// A build running beside the run loop, and whether the file changed
    /// again while it ran.
    pub building: bool,
    pub again: bool,
    /// The model list refresh the last set started, cancelled when a newer
    /// set starts its own, so a slow listing never lands over a newer one.
    pub refreshing: Option<tokio::task::AbortHandle>,
    /// The last reload's outcome, for `stats`.
    pub last: Value,
}

/// A new set of providers, built and checked, waiting to be swapped in.
pub struct Built {
    pub specs: Vec<String>,
    pub providers: HashMap<String, Provider>,
    pub bindings: serde_json::Map<String, Value>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    /// What the in-use check covered, asked again just before the swap.
    pub unusable: Vec<String>,
}

/// The specs the config file names now, as a client starting the daemon
/// would read them: its `provider`, else the providers whose well-known
/// key variable the daemon was started with.
fn specs(config: &std::path::Path) -> Result<Vec<String>> {
    let read =
        agent_client::config::read(config).map_err(|error| match error.split_once(": ") {
            Some((code, detail)) => Error::with(code, detail),
            None => Error::with("config_invalid", error),
        })?;
    crate::client::environment_providers(&|name| std::env::var(name).ok(), &|| {
        Ok(read.provider.clone())
    })
}

impl Reload {
    /// Builds the set the file names now beside the run loop and sends it,
    /// or why it was refused, to `done`; nothing when it names the set
    /// bound now.
    pub fn start(
        &mut self,
        current: Arc<HashMap<String, Provider>>,
        store: Store,
        done: mpsc::UnboundedSender<Result<Option<Built>>>,
    ) {
        if self.building {
            self.again = true;
            return;
        }
        self.building = true;
        let (config, binder) = (self.config.clone(), self.binder.clone());
        let (bound, bindings) = (self.specs.clone(), self.bindings.clone());
        tokio::spawn(async move {
            let built = build(config, binder, bound, bindings, current, store).await;
            let _ = done.send(built);
        });
    }

    /// Records what a finished build came to; the caller swaps an applied
    /// set in. Whether another build should start, for a change made
    /// while this one ran.
    pub fn finished(&mut self, outcome: &Result<Option<Built>>) -> bool {
        self.building = false;
        let at = super::now_ms();
        match outcome {
            Ok(None) => {}
            Ok(Some(built)) => {
                self.specs = built.specs.clone();
                self.bindings = built.bindings.clone();
                self.last = json!({"at_ms": at, "status": "applied",
                    "added": built.added, "removed": built.removed, "changed": built.changed});
            }
            Err(error) => {
                self.last = json!({"at_ms": at, "status": "refused",
                    "error": error.code, "detail": error.detail, "facts": error.facts});
            }
        }
        std::mem::take(&mut self.again)
    }
}

async fn build(
    config: PathBuf,
    binder: Binder,
    bound: Vec<String>,
    bindings: serde_json::Map<String, Value>,
    current: Arc<HashMap<String, Provider>>,
    store: Store,
) -> Result<Option<Built>> {
    let config_for_read = config.clone();
    let specs = tokio::task::spawn_blocking(move || specs(&config_for_read))
        .await
        .map_err(|_| Error::new("provider_reload_failed"))??;
    if specs == bound {
        return Ok(None);
    }
    if specs.is_empty() {
        return Err(Error::with(
            "no_providers",
            format!("{} names no provider", config.display()),
        ));
    }
    let parsed = {
        let specs = specs.clone();
        tokio::task::spawn_blocking(move || {
            specs
                .iter()
                .map(|spec| ProviderSpec::parse(spec))
                .collect::<Result<Vec<_>>>()
        })
        .await
        .map_err(|_| Error::new("provider_reload_failed"))??
    };
    // A spec's name is what comes before its `=`, as `ProviderSpec` reads it.
    let was: HashMap<String, &String> = bound
        .iter()
        .map(|spec| {
            (
                spec.split_once('=')
                    .map_or(spec.as_str(), |(name, _)| name)
                    .to_owned(),
                spec,
            )
        })
        .collect();
    let mut providers = HashMap::new();
    let mut new_bindings = serde_json::Map::new();
    let (mut added, mut changed) = (Vec::new(), Vec::new());
    // Names kept but bound to another API family: what was stored for
    // their agents is no longer theirs to send, as if they were removed.
    let mut retyped = Vec::new();
    let aws_start = agent_runtime::provider::aws::Start::new();
    for spec in &parsed {
        let kept = (was.get(&spec.name).is_some_and(|text| **text == spec.spec))
            .then(|| current.get(&spec.name).zip(bindings.get(&spec.name)))
            .flatten();
        let (provider, binding) = match kept {
            Some((provider, binding)) => (provider.clone(), binding.clone()),
            None => {
                match was.contains_key(&spec.name) {
                    true => changed.push(spec.name.clone()),
                    false => added.push(spec.name.clone()),
                }
                binder.bind(spec, &aws_start).await.map_err(|error| {
                    Error::with(
                        &error.code,
                        match &error.detail {
                            Some(detail) => format!("{}: {detail}", spec.name),
                            None => spec.name.clone(),
                        },
                    )
                })?
            }
        };
        if current
            .get(&spec.name)
            .is_some_and(|was| was.family() != provider.family())
        {
            retyped.push(spec.name.clone());
        }
        if providers.insert(spec.name.clone(), provider).is_some() {
            return Err(Error::with("duplicate_provider", spec.name.as_str()));
        }
        new_bindings.insert(spec.name.clone(), binding);
    }
    let mut removed: Vec<String> = was
        .keys()
        .filter(|name| !providers.contains_key(*name))
        .cloned()
        .collect();
    removed.sort();
    let mut unusable = removed.clone();
    unusable.extend(retyped);
    unusable.sort();
    unused(&store, &unusable).await?;
    Ok(Some(Built {
        specs,
        providers,
        bindings: new_bindings,
        added,
        removed,
        changed,
        unusable,
    }))
}

/// Refuses with `provider_in_use`, naming them, while agents or unfinished
/// turns still use any of `unusable`: providers a new set removes or binds
/// to another API family.
pub async fn unused(store: &Store, unusable: &[String]) -> Result<()> {
    if unusable.is_empty() {
        return Ok(());
    }
    let names = unusable.to_vec();
    let agents = store
        .op("bots_on_providers", move |db| db.bots_on_providers(&names))
        .await?;
    if agents.is_empty() {
        return Ok(());
    }
    let shown: Vec<&str> = agents.iter().take(NAMED).map(String::as_str).collect();
    let more = match agents.len() - shown.len() {
        0 => String::new(),
        more => format!(" and {more} more"),
    };
    Err(Error {
        code: "provider_in_use".into(),
        detail: Some(format!(
            "{} still run on {}: {}{more}; move or remove them first",
            if agents.len() == 1 {
                "1 agent"
            } else {
                "agents"
            },
            unusable.join(", "),
            shown.join(", "),
        )),
        facts: match json!({"providers": unusable, "agents": shown, "count": agents.len()}) {
            Value::Object(facts) => Some(Box::new(facts)),
            _ => None,
        },
    })
}
