//! Each skill Sonny ships says, in its description, what it is and when
//! to use it: an agent reads only that line before it opens the file.
//! When is a word such as "when", "use" or "before".
use std::path::Path;

const WHEN: [&str; 12] = [
    "when", "whenever", "use", "used", "using", "if", "before", "after", "while", "during",
    "trigger", "triggers",
];

fn says_when(description: &str) -> bool {
    description
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| WHEN.iter().any(|when| word.eq_ignore_ascii_case(when)))
}

#[test]
fn every_shipped_skill_says_when_to_use_it() {
    let skills = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
    let mut checked = 0;
    for entry in std::fs::read_dir(&skills).unwrap() {
        let file = entry.unwrap().path().join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let (front, _) = agent_client::policy::front_matter(&text);
        let description = front.description.unwrap_or_default();
        assert!(says_when(&description), "{}", file.display());
        checked += 1;
    }
    assert!(checked >= 10, "only {checked} skills found");
}

#[test]
fn the_check_reads_whole_words() {
    for (said, when) in [
        ("Whenever a PR opens", true),
        ("Reads PDFs; for forms, tables", false),
        ("Run before every deploy", true),
        ("Housekeeping", false),
        ("useful things", false),
    ] {
        assert_eq!(says_when(said), when, "{said}");
    }
}
