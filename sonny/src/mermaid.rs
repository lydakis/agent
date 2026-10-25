//! Mermaid diagrams as SVG, through Pele (vendor/pele.global.js, MIT),
//! run by the machine's `node` or `bun`. Pele themes its SVG with CSS
//! variables, which resvg does not read, so the colours are written in
//! here, for the terminal's light or dark background.
use std::collections::HashMap;

const PELE: &str = include_str!("../vendor/pele.global.js");

/// Pele's own variables, for each background.
const LIGHT: [(&str, &str); 8] = [
    ("--pele-bg", "#ffffff"),
    ("--pele-fg", "#1f1f1f"),
    ("--pele-muted", "#6e6e6e"),
    ("--pele-line", "#1f1f1f"),
    ("--pele-surface", "#f3f3f3"),
    ("--pele-surface-alt", "#e6e6e6"),
    ("--pele-border", "#8a8a8a"),
    ("--pele-accent", "#5b7bd5"),
];
const DARK: [(&str, &str); 8] = [
    ("--pele-bg", "#1e1f22"),
    ("--pele-fg", "#e6e6e6"),
    ("--pele-muted", "#9a9a9a"),
    ("--pele-line", "#c8c8c8"),
    ("--pele-surface", "#2b2d31"),
    ("--pele-surface-alt", "#383a40"),
    ("--pele-border", "#6b6f76"),
    ("--pele-accent", "#7aa2f7"),
];

/// The diagram as an SVG resvg can draw, or why not.
pub fn svg(source: &str) -> Result<String, String> {
    let source = serde_json::to_string(source).map_err(|e| e.to_string())?;
    let script = format!(
        "{PELE}\ntry {{ process.stdout.write(pele.render({source}).svg) }} \
         catch (e) {{ process.stderr.write(String(e && e.message || e)); process.exit(1) }}\n"
    );
    let svg = crate::js::run(script, "Mermaid")?;
    let theme = if crate::term::light() { LIGHT } else { DARK };
    Ok(resolve(&svg, &theme))
}

/// Every `var(--name, fallback)` written out: the root's own variables
/// (`--_fg: var(--pele-fg, #1f1f1f)`) through the theme, then its
/// fallback. A corner radius set in a style becomes an `rx` attribute.
fn resolve(svg: &str, theme: &[(&str, &str)]) -> String {
    let mut known: HashMap<String, String> = theme
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    // The root's declarations, in order, each resolved with those before.
    if let Some(style) = svg
        .split_once("<svg")
        .and_then(|(_, root)| root.split_once('>'))
        .and_then(|(root, _)| root.split_once("style=\""))
        .and_then(|(_, style)| style.split_once('"'))
        .map(|(style, _)| style.to_owned())
    {
        for declaration in style.split(';') {
            if let Some((name, value)) = declaration.split_once(':')
                && name.trim().starts_with("--")
            {
                let value = substitute(value.trim(), &known);
                known.entry(name.trim().to_owned()).or_insert(value);
            }
        }
    }
    let svg = substitute(svg, &known);
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg.as_str();
    while let Some(at) = rest.find("style=\"rx:") {
        out.push_str(&rest[..at]);
        let after = &rest[at + "style=\"rx:".len()..];
        let Some(end) = after.find('"') else {
            rest = &rest[at..];
            break;
        };
        let radius = after[..end].trim().trim_end_matches("px");
        out.push_str(&format!("rx=\"{radius}\""));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// `var(…)` replaced, innermost first, by a known value or its fallback.
fn substitute(text: &str, known: &HashMap<String, String>) -> String {
    let mut text = text.to_owned();
    while let Some(start) = text.find("var(") {
        // The matching parenthesis, past any nested ones.
        let mut depth = 0;
        let mut end = None;
        for (i, c) in text[start + 3..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(start + 3 + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        let inner = &text[start + 4..end];
        let (name, fallback) = match inner.split_once(',') {
            Some((name, fallback)) => (name.trim(), Some(fallback.trim())),
            None => (inner.trim(), None),
        };
        let value = known
            .get(name)
            .cloned()
            .or_else(|| fallback.map(|f| substitute(f, known)))
            .unwrap_or_default();
        text.replace_range(start..=end, &value);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pele_variables_become_colours() {
        let svg = r#"<svg style="--_fg:var(--pele-fg,#1f1f1f);--_r:var(--pele-radius,4px);color:var(--_fg)" fill="var(--_fg)"><rect fill="var(--_s, var(--pele-surface,#f3f3f3))" style="rx:var(--_r)"/></svg>"#;
        let out = resolve(svg, &DARK);
        assert!(!out.contains("var("), "{out}");
        assert!(out.contains(r##"fill="#e6e6e6""##), "{out}");
        assert!(out.contains(r##"<rect fill="#2b2d31" rx="4"/>"##), "{out}");
    }

    #[test]
    fn a_flowchart_draws_when_node_is_here() {
        if !crate::js::available() {
            return;
        }
        let out = svg("flowchart LR\n  A[Sonny] --> B[daemon]").unwrap();
        assert!(out.starts_with("<svg") && !out.contains("var("), "{out}");
        crate::image::render(&out, 80).unwrap();
        assert!(svg("flowchart LR\n  A[").is_err());
    }
}
