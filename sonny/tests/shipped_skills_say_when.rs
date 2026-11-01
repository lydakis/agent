//! Each skill Sonny ships says, in its description, what it is and when
//! to use it, as the skill index asks of the person's own: an agent reads
//! only that line before it opens the file, and `/status` would name one
//! that does not.
use std::path::Path;

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
        let warning = agent_client::policy::skill_warning(front.description.as_deref());
        assert_eq!(warning, None, "{}", file.display());
        checked += 1;
    }
    assert!(checked >= 10, "only {checked} skills found");
}
