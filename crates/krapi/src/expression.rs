//! THE `${...}` EXPRESSION SCAN — the grammar that separates kro-rs
//! templates from plain YAML values. A string leaf is scanned for
//! `${...}` groups (brace-nested, so `${merge(a, {"x": 1})}` scans
//! whole); a string that is *exactly one* expression takes the CEL
//! value as its rendered type; any other string interpolates each
//! expression's stringified value between its literal parts.
//!
//! A literal `$${...}` escapes the interpolation (the `$` survives).

/// One scanned group: the CEL source between `${` and its matching `}`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the whole `${...}` group.
    pub start: usize,
    /// Byte offset just past the whole group.
    pub end: usize,
    /// The CEL source (no braces).
    pub expr: String,
}

/// Scan every `${...}` group in one string. Unbalanced groups are an
/// error — a template author who opens `${` and never closes it gets a
/// refusal at compile time, never a silent literal.
///
/// # Errors
/// An unclosed `${` group.
#[allow(clippy::many_single_char_names)] // the scanner's i/j/depth are the idiom
pub fn scan(s: &str) -> Result<Vec<Span>, String> {
    let b = s.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i + 1 < b.len() {
        // the escape: `$${` starts a literal dollar + brace
        if b[i] == b'$' && b[i + 1] == b'$' && i + 2 < b.len() && b[i + 2] == b'{' {
            i += 3;
            continue;
        }
        if b[i] == b'$' && b[i + 1] == b'{' {
            let start = i;
            let mut depth = 1usize;
            let mut j = i + 2;
            let mut in_string = false;
            let mut escaped = false;
            while j < b.len() {
                let c = b[j];
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if c == b'\\' {
                        escaped = true;
                    } else if c == b'"' {
                        in_string = false;
                    }
                } else {
                    match c {
                        b'"' => in_string = true,
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                j += 1;
            }
            if j >= b.len() {
                return Err(format!(
                    "unclosed ${{...}} group starting at byte {start}: {s:?}"
                ));
            }
            spans.push(Span {
                start,
                end: j + 1,
                expr: s[start + 2..j].to_owned(),
            });
            i = j + 1;
        } else {
            i += 1;
        }
    }
    Ok(spans)
}

/// The rendered shape of one string leaf: either the whole leaf is one
/// expression (the CEL value stands for the field) or the leaf is
/// interpolation (each expression stringifies between literals).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Render {
    /// The whole string is `${...}` — the CEL value replaces the field.
    Whole(String),
    /// Literal parts and expressions interleave; the render is a string.
    /// Literal parts are carried with `$${` unescaped to `${`.
    Parts(Vec<Piece>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Literal(String),
    Expr(String),
}

/// Classify one string leaf.
///
/// # Errors
/// An unclosed `${` group.
pub fn classify(s: &str) -> Result<Render, String> {
    let spans = scan(s)?;
    if spans.is_empty() {
        return Ok(Render::Parts(vec![Piece::Literal(unescape(s))]));
    }
    if spans.len() == 1 && spans[0].start == 0 && spans[0].end == s.len() {
        return Ok(Render::Whole(spans[0].expr.clone()));
    }
    let mut pieces = Vec::new();
    let mut cursor = 0usize;
    for sp in &spans {
        if sp.start > cursor {
            pieces.push(Piece::Literal(unescape(&s[cursor..sp.start])));
        }
        pieces.push(Piece::Expr(sp.expr.clone()));
        cursor = sp.end;
    }
    if cursor < s.len() {
        pieces.push(Piece::Literal(unescape(&s[cursor..])));
    }
    Ok(Render::Parts(pieces))
}

/// Unwrap a policy expression (`readyWhen`, `includeWhen`, `forEach`,
/// `conditions`): the string must be exactly one `${...}` group.
///
/// # Errors
/// Anything that is not exactly one group.
pub fn unwrap(s: &str) -> Result<String, String> {
    match classify(s)? {
        Render::Whole(e) => Ok(e),
        Render::Parts(_) => Err(format!(
            "policy expressions must be a single ${{...}} group, got: {s:?}"
        )),
    }
}

/// Restore the escaped `${` (the scan swallowed the extra `$`).
fn unescape(s: &str) -> String {
    if s.contains("$${") {
        s.replace("$${", "${")
    } else {
        s.to_owned()
    }
}

/// A template path — where inside the object one expression sits. Segments
/// are object keys or array indexes; the renderer descends and rewrites
/// only the leaf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

impl std::fmt::Display for Seg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Seg::Key(k) => write!(f, "{k}"),
            Seg::Index(i) => write!(f, "[{i}]"),
        }
    }
}

/// A template's embedded expressions: each carries its path and its CEL
/// source (the whole/interpolated distinction rides with it).
#[derive(Debug, Clone)]
pub struct FieldExpr {
    pub path: Vec<Seg>,
    pub render: Render,
}

/// Walk a parsed template object and collect every string leaf's
/// expressions, keyed by path. Maps recurse by key, arrays by index;
/// scalar leaves carry their `Render`.
///
/// # Errors
/// An unclosed `${` group anywhere in the tree.
pub fn collect(value: &serde_json::Value) -> Result<Vec<FieldExpr>, String> {
    let mut out = Vec::new();
    walk(value, &mut Vec::new(), &mut out)?;
    Ok(out)
}

fn walk(
    v: &serde_json::Value,
    path: &mut Vec<Seg>,
    out: &mut Vec<FieldExpr>,
) -> Result<(), String> {
    match v {
        serde_json::Value::Object(map) => {
            for (k, child) in map {
                path.push(Seg::Key(k.clone()));
                walk(child, path, out)?;
                path.pop();
            }
            Ok(())
        }
        serde_json::Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                path.push(Seg::Index(i));
                walk(child, path, out)?;
                path.pop();
            }
            Ok(())
        }
        serde_json::Value::String(s) => {
            let render = classify(s)?;
            if matches!(render, Render::Parts(ref ps) if ps.iter().all(|p| matches!(p, Piece::Literal(_))))
            {
                return Ok(());
            }
            out.push(FieldExpr {
                path: path.clone(),
                render,
            });
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_string_has_no_groups() {
        assert_eq!(
            classify("hello").unwrap(),
            Render::Parts(vec![Piece::Literal("hello".to_owned())])
        );
    }

    #[test]
    fn a_whole_expression_takes_the_value() {
        assert_eq!(
            classify("${schema.spec.name}").unwrap(),
            Render::Whole("schema.spec.name".to_owned())
        );
    }

    #[test]
    fn nested_braces_scan_whole() {
        let r = classify(r#"${merge(schema.spec.labels, {"a": "b"})}"#).unwrap();
        assert_eq!(
            r,
            Render::Whole(r#"merge(schema.spec.labels, {"a": "b"})"#.to_owned())
        );
    }

    #[test]
    fn interpolation_concatenates() {
        let r = classify("web-${schema.spec.name}-v${schema.spec.n}").unwrap();
        assert_eq!(
            r,
            Render::Parts(vec![
                Piece::Literal("web-".to_owned()),
                Piece::Expr("schema.spec.name".to_owned()),
                Piece::Literal("-v".to_owned()),
                Piece::Expr("schema.spec.n".to_owned()),
            ])
        );
    }

    #[test]
    fn the_escape_survives() {
        assert_eq!(
            classify("$${literal}").unwrap(),
            Render::Parts(vec![Piece::Literal("${literal}".to_owned())])
        );
    }

    #[test]
    fn an_unclosed_group_is_refused() {
        assert!(classify("${schema.spec.name").is_err());
    }

    #[test]
    fn collect_finds_paths_through_maps_arrays_and_skips_literals() {
        let t = serde_json::json!({
            "metadata": {"name": "${schema.spec.name}"},
            "spec": {"containers": [{"image": "${schema.spec.image}"}, {"image": "nginx"}]},
            "annotations": {"note": "plain"}
        });
        let found = collect(&t).unwrap();
        let paths: Vec<String> = found
            .iter()
            .map(|f| {
                let mut out = String::new();
                for (i, seg) in f.path.iter().enumerate() {
                    match seg {
                        Seg::Key(k) => {
                            if i > 0 {
                                out.push('.');
                            }
                            out.push_str(k);
                        }
                        Seg::Index(idx) => {
                            use std::fmt::Write as _;
                            let _ = write!(out, "[{idx}]");
                        }
                    }
                }
                out
            })
            .collect();
        assert_eq!(paths, vec!["metadata.name", "spec.containers[0].image"]);
    }
}
