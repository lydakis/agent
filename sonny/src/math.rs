//! LaTeX in a reply, as Unicode text (x², α, ∑, a/b), which stays
//! selectable and wraps with the line: inline `$…$`, and display math
//! (`$$…$$`, a ```math fence) a line at a time.

const SYMBOLS: &[(&str, &str)] = &[
    ("alpha", "α"),
    ("beta", "β"),
    ("gamma", "γ"),
    ("delta", "δ"),
    ("epsilon", "ε"),
    ("varepsilon", "ε"),
    ("zeta", "ζ"),
    ("eta", "η"),
    ("theta", "θ"),
    ("vartheta", "ϑ"),
    ("iota", "ι"),
    ("kappa", "κ"),
    ("lambda", "λ"),
    ("mu", "μ"),
    ("nu", "ν"),
    ("xi", "ξ"),
    ("pi", "π"),
    ("rho", "ρ"),
    ("sigma", "σ"),
    ("tau", "τ"),
    ("upsilon", "υ"),
    ("phi", "φ"),
    ("varphi", "φ"),
    ("chi", "χ"),
    ("psi", "ψ"),
    ("omega", "ω"),
    ("Gamma", "Γ"),
    ("Delta", "Δ"),
    ("Theta", "Θ"),
    ("Lambda", "Λ"),
    ("Xi", "Ξ"),
    ("Pi", "Π"),
    ("Sigma", "Σ"),
    ("Phi", "Φ"),
    ("Psi", "Ψ"),
    ("Omega", "Ω"),
    ("sum", "∑"),
    ("prod", "∏"),
    ("int", "∫"),
    ("oint", "∮"),
    ("partial", "∂"),
    ("nabla", "∇"),
    ("infty", "∞"),
    ("cdot", "·"),
    ("times", "×"),
    ("div", "÷"),
    ("pm", "±"),
    ("mp", "∓"),
    ("le", "≤"),
    ("leq", "≤"),
    ("ge", "≥"),
    ("geq", "≥"),
    ("ne", "≠"),
    ("neq", "≠"),
    ("approx", "≈"),
    ("equiv", "≡"),
    ("sim", "∼"),
    ("propto", "∝"),
    ("to", "→"),
    ("rightarrow", "→"),
    ("leftarrow", "←"),
    ("Rightarrow", "⇒"),
    ("Leftarrow", "⇐"),
    ("iff", "⇔"),
    ("implies", "⇒"),
    ("mapsto", "↦"),
    ("in", "∈"),
    ("notin", "∉"),
    ("subset", "⊂"),
    ("subseteq", "⊆"),
    ("supset", "⊃"),
    ("cup", "∪"),
    ("cap", "∩"),
    ("emptyset", "∅"),
    ("forall", "∀"),
    ("exists", "∃"),
    ("neg", "¬"),
    ("land", "∧"),
    ("lor", "∨"),
    ("wedge", "∧"),
    ("vee", "∨"),
    ("oplus", "⊕"),
    ("otimes", "⊗"),
    ("circ", "∘"),
    ("ldots", "…"),
    ("cdots", "⋯"),
    ("dots", "…"),
    ("degree", "°"),
    ("prime", "′"),
    ("langle", "⟨"),
    ("rangle", "⟩"),
    ("lfloor", "⌊"),
    ("rfloor", "⌋"),
    ("lceil", "⌈"),
    ("rceil", "⌉"),
    ("hbar", "ℏ"),
    ("ell", "ℓ"),
    ("quad", "  "),
    ("qquad", "    "),
    (",", " "),
    (";", " "),
    ("!", ""),
    (" ", " "),
    ("{", "{"),
    ("}", "}"),
    ("%", "%"),
    ("$", "$"),
    ("_", "_"),
    ("&", "&"),
    ("#", "#"),
    ("|", "‖"),
    ("mid", "|"),
];

const BLACKBOARD: &[(char, char)] = &[
    ('R', 'ℝ'),
    ('N', 'ℕ'),
    ('Z', 'ℤ'),
    ('Q', 'ℚ'),
    ('C', 'ℂ'),
    ('P', 'ℙ'),
    ('E', '𝔼'),
];

fn superscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        'a' => 'ᵃ',
        'b' => 'ᵇ',
        'c' => 'ᶜ',
        'd' => 'ᵈ',
        'e' => 'ᵉ',
        'k' => 'ᵏ',
        'm' => 'ᵐ',
        'o' => 'ᵒ',
        'p' => 'ᵖ',
        't' => 'ᵗ',
        'x' => 'ˣ',
        'T' => 'ᵀ',
        '*' => '*',
        '′' => '′',
        _ => return None,
    })
}

fn subscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'r' => 'ᵣ',
        'x' => 'ₓ',
        't' => 'ₜ',
        'm' => 'ₘ',
        'p' => 'ₚ',
        's' => 'ₛ',
        _ => return None,
    })
}

/// A `{group}` or a single character (or command) at the start of `text`,
/// and what follows it.
fn argument(text: &str) -> (String, &str) {
    let text = text.trim_start();
    if let Some(rest) = text.strip_prefix('{') {
        let mut depth = 1;
        for (i, c) in rest.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return (rest[..i].to_owned(), &rest[i + 1..]);
                    }
                }
                _ => {}
            }
        }
        return (rest.to_owned(), "");
    }
    if let Some(name) = text.strip_prefix('\\') {
        // A command's letters, else the one character after the backslash.
        let letters = name
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(name.len());
        let end = 1 + match letters {
            0 => name.chars().next().map_or(0, char::len_utf8),
            n => n,
        };
        return (text[..end].to_owned(), &text[end..]);
    }
    match text.chars().next() {
        Some(c) => (c.to_string(), &text[c.len_utf8()..]),
        None => (String::new(), ""),
    }
}

/// Raised or lowered when every character has a form for it; else `^(…)`.
fn shifted(text: &str, map: fn(char) -> Option<char>, mark: char) -> String {
    match text.chars().map(map).collect::<Option<String>>() {
        Some(shifted) => shifted,
        None if text.chars().count() == 1 => format!("{mark}{text}"),
        None => format!("{mark}({text})"),
    }
}

/// Inline LaTeX as Unicode text, as near as text gets.
pub fn unicode(tex: &str) -> String {
    let mut out = String::new();
    let mut rest = tex;
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        match c {
            '^' | '_' => {
                let (arg, after) = argument(rest);
                let arg = unicode(&arg);
                out.push_str(&if c == '^' {
                    shifted(&arg, superscript, '^')
                } else {
                    shifted(&arg, subscript, '_')
                });
                rest = after;
            }
            '{' | '}' => {}
            '\\' => {
                let end = rest
                    .find(|c: char| !c.is_ascii_alphabetic())
                    .unwrap_or(rest.len());
                let (name, after) = if end == 0 {
                    rest.char_indices()
                        .nth(1)
                        .map_or((rest, ""), |(i, _)| (&rest[..i], &rest[i..]))
                } else {
                    (&rest[..end], &rest[end..])
                };
                rest = after;
                match name {
                    "frac" | "dfrac" | "tfrac" => {
                        let (top, after) = argument(rest);
                        let (bottom, after) = argument(after);
                        let wrap = |s: String| {
                            if s.chars().count() > 1 && s.contains([' ', '+', '-']) {
                                format!("({s})")
                            } else {
                                s
                            }
                        };
                        out.push_str(&format!(
                            "{}/{}",
                            wrap(unicode(&top)),
                            wrap(unicode(&bottom))
                        ));
                        rest = after;
                    }
                    "sqrt" => {
                        let (arg, after) = argument(rest);
                        let arg = unicode(&arg);
                        if arg.chars().count() > 1 {
                            out.push_str(&format!("√({arg})"));
                        } else {
                            out.push_str(&format!("√{arg}"));
                        }
                        rest = after;
                    }
                    "mathbb" => {
                        let (arg, after) = argument(rest);
                        for c in arg.chars() {
                            let mapped = BLACKBOARD.iter().find(|(from, _)| *from == c);
                            out.push(mapped.map_or(c, |(_, to)| *to));
                        }
                        rest = after;
                    }
                    "text" | "mathrm" | "mathbf" | "mathit" | "operatorname" | "mathcal"
                    | "boldsymbol" | "textbf" | "textit" | "mbox" => {
                        let (arg, after) = argument(rest);
                        out.push_str(&unicode(&arg));
                        rest = after;
                    }
                    "left" | "right" | "big" | "Big" | "bigg" | "Bigg" | "displaystyle" => {}
                    "hat" | "bar" | "vec" | "tilde" | "dot" | "overline" => {
                        let (arg, after) = argument(rest);
                        let mark = match name {
                            "hat" => '\u{0302}',
                            "bar" | "overline" => '\u{0304}',
                            "vec" => '\u{20D7}',
                            "tilde" => '\u{0303}',
                            _ => '\u{0307}',
                        };
                        for c in unicode(&arg).chars() {
                            out.push(c);
                            out.push(mark);
                        }
                        rest = after;
                    }
                    name => match SYMBOLS.iter().find(|(n, _)| *n == name) {
                        Some((_, symbol)) => out.push_str(symbol),
                        // Functions such as \sin, \log and \max read as their names.
                        None => out.push_str(name),
                    },
                }
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::unicode;

    #[test]
    fn inline_latex_reads_as_text() {
        assert_eq!(unicode(r"x^2 + \alpha"), "x² + α");
        assert_eq!(unicode(r"\frac{a+b}{2}"), "(a+b)/2");
        assert_eq!(unicode(r"\sum_{i=1}^{n} i"), "∑ᵢ₌₁ⁿ i");
        assert_eq!(unicode(r"\sqrt{x} \le \infty"), "√x ≤ ∞");
        assert_eq!(unicode(r"f: \mathbb{R} \to \mathbb{R}"), "f: ℝ → ℝ");
        assert_eq!(
            unicode(r"\sin(x)^{y}"),
            "sin(x)ʸ".replace('ʸ', "^y").as_str()
        );
        assert_eq!(unicode(r"e^{i\pi}"), "e^(iπ)");
        // A backslash before a character no command starts with keeps it.
        assert_eq!(unicode("x^\\α"), "x^α");
    }
}
