//! Sonny's own `memory` source: each fact saved to or removed from the
//! person's memory or a project's is one event, the fact's name its topic.
//! No agent follows it unless it asks to, with `sources: ["memory"]`.
use super::sources::News;
use serde_json::{Map, Value, json};
use std::path::Path;

/// What changed in memory since the last look, as lines a source prints:
/// a fact saved (new, or written again) by the agent that saved it, and a
/// fact removed. `cursor` is the source's own folder, where each fact's
/// file is kept with when it was last written, its project and its type, so
/// a removal carries the fields a filter may name; the first look tells
/// none, so a new store is not told memory's past.
pub fn news(home: &Path, cursor: &Path) -> News {
    news_in(&home.join(".agents/memory"), cursor)
}

fn news_in(root: &Path, cursor: &Path) -> News {
    let path = cursor.join("read.json");
    let first = !path.exists();
    let had: Map<String, Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    // Every project, not the capped view /background shows.
    let view = agent_client::memory::view_all(root);
    let scopes =
        std::iter::once(&view["user"]).chain(view["projects"].as_array().into_iter().flatten());
    let mut now = Map::new();
    let mut read = Vec::new();
    let mut lines = Vec::new();
    for scope in scopes {
        let Some(dir) = scope["dir"].as_str() else {
            continue;
        };
        let project = &scope["name"];
        let Some(facts) = scope["facts"].as_array() else {
            // A folder that could not be read keeps what was known of it,
            // so none of its facts reads as removed.
            for (file, at) in had.iter().filter(|(file, _)| in_dir(file, dir)) {
                now.insert(file.clone(), at.clone());
            }
            continue;
        };
        read.push(dir.to_owned());
        for fact in facts {
            let (Some(file), Some(at)) = (fact["path"].as_str(), fact["modified"].as_u64()) else {
                continue;
            };
            now.insert(
                file.to_owned(),
                json!({"at": at, "project": project, "type": fact["type"]}),
            );
            if first || had.get(file).map(|was| &was["at"]) == Some(&json!(at)) {
                continue;
            }
            let by = fact["source"]
                .as_str()
                .filter(|by| !by.is_empty())
                .unwrap_or("someone");
            lines.push(json!({
                "id": format!("{file}/{at}"),
                "topic": fact["name"],
                "from": format!("{by} saved"),
                "text": fact["description"],
                "project": project,
                "type": fact["type"],
                "file": file,
            }));
        }
    }
    for (file, was) in &had {
        if now.contains_key(file) || !read.iter().any(|dir| in_dir(file, dir)) {
            continue;
        }
        let at = &was["at"];
        let name = Path::new(file)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned());
        lines.push(json!({
            "id": format!("{file}/removed/{at}"),
            "topic": name,
            "from": "removed",
            "text": format!("{file} is gone"),
            "project": was["project"],
            "type": was["type"],
            "file": file,
        }));
    }
    let changed = first || now != had;
    News {
        lines: lines.iter().map(Value::to_string).collect(),
        read: changed.then_some((path, now)),
    }
}

/// Whether `file` is a fact of the folder `dir` itself, not of one in it.
fn in_dir(file: &str, dir: &str) -> bool {
    Path::new(file).parent() == Some(Path::new(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sonny-memory-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn save(dir: &Path, name: &str, by: &str, description: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{name}.md")),
            format!(
                "---\nname: {name}\ndescription: {description}\ntype: project\nsource: {by}\n\
                 verified: 2026-11-14\n---\n\nBody.\n"
            ),
        )
        .unwrap();
    }

    fn said(news: &News) -> Vec<Value> {
        news.lines
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn a_fact_saved_or_removed_is_one_event_named_by_the_fact_and_the_past_is_not_told() {
        let temp = scratch("told");
        let (root, cursor) = (temp.join("memory"), temp.join("cursor"));
        let shop = root.join("projects/shop");
        save(&root, "old-ci-notes", "calc.lead", "CI runs on push.");
        // The first look tells nothing of what was already there.
        let first = news_in(&root, &cursor);
        assert!(first.lines.is_empty());
        assert!(first.mark_read());
        save(
            &shop,
            "release-steps",
            "shop.fix",
            "Tags are pushed from the Mac.",
        );
        std::fs::remove_file(root.join("old-ci-notes.md")).unwrap();
        let news = news_in(&root, &cursor);
        let lines = said(&news);
        let file = shop.join("release-steps.md").to_string_lossy().into_owned();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            (
                &lines[0]["topic"],
                &lines[0]["from"],
                &lines[0]["text"],
                &lines[0]["project"],
                &lines[0]["file"]
            ),
            (
                &json!("release-steps"),
                &json!("shop.fix saved"),
                &json!("Tags are pushed from the Mac."),
                &json!("shop"),
                &json!(file)
            )
        );
        // A removal carries the project and type a filter may name.
        assert_eq!(
            (
                &lines[1]["topic"],
                &lines[1]["from"],
                &lines[1]["project"],
                &lines[1]["type"]
            ),
            (
                &json!("old-ci-notes"),
                &json!("removed"),
                &Value::Null,
                &json!("project")
            )
        );
        // Until the lookout keeps them, the same news is read again, with
        // the same IDs, so none is told twice.
        let again = said(&news_in(&root, &cursor));
        assert_eq!(
            again.iter().map(|l| &l["id"]).collect::<Vec<_>>(),
            lines.iter().map(|l| &l["id"]).collect::<Vec<_>>()
        );
        assert!(news.mark_read());
        assert!(news_in(&root, &cursor).lines.is_empty());
    }

    #[test]
    fn every_project_is_watched_past_the_hundred_background_shows() {
        let temp = scratch("many");
        let (root, cursor) = (temp.join("memory"), temp.join("cursor"));
        for n in 0..=100 {
            std::fs::create_dir_all(root.join(format!("projects/p{n:03}"))).unwrap();
        }
        assert!(news_in(&root, &cursor).mark_read());
        save(
            &root.join("projects/p100"),
            "late",
            "p100.lead",
            "The last project.",
        );
        let lines = said(&news_in(&root, &cursor));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["project"], "p100");
    }

    #[test]
    fn a_fact_written_again_is_saved_again() {
        let temp = scratch("again");
        let (root, cursor) = (temp.join("memory"), temp.join("cursor"));
        save(&root, "style", "calc.lead", "Short replies.");
        assert!(news_in(&root, &cursor).mark_read());
        // Written again a moment later, by another agent.
        std::thread::sleep(std::time::Duration::from_millis(20));
        save(&root, "style", "calc.docs", "Short replies, no headings.");
        let lines = said(&news_in(&root, &cursor));
        assert_eq!(lines.len(), 1);
        assert_eq!(
            (&lines[0]["from"], &lines[0]["project"]),
            (&json!("calc.docs saved"), &Value::Null)
        );
    }
}
