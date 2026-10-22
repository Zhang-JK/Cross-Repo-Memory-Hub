//! Lightweight keyword ranking. Every query term is matched as a
//! case-insensitive substring, which also works for CJK text that has no
//! whitespace word boundaries.

use crate::model::Memory;

const SNIPPET_CHARS: usize = 200;

pub(crate) fn terms(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for term in text.split_whitespace() {
        let term = term.to_lowercase();
        if !out.contains(&term) {
            out.push(term);
        }
    }
    out
}

/// Returns `None` when no term matches at all.
pub(crate) fn score(memory: &Memory, terms: &[String]) -> Option<f64> {
    let meta = &memory.meta;
    let title = meta.title.to_lowercase();
    let body = memory.body.to_lowercase();
    let kind = meta.kind.to_lowercase();
    let slug = meta.id.slug.to_lowercase();

    let mut total = 0.0;
    let mut matched = 0usize;
    for term in terms {
        let mut s = 0.0;
        s += title.matches(term.as_str()).count() as f64 * 5.0;
        if slug.contains(term.as_str()) {
            s += 2.0;
        }
        if meta.tags.iter().any(|t| t.to_lowercase().contains(term.as_str())) {
            s += 4.0;
        }
        if meta.groups.iter().any(|g| g.to_lowercase().contains(term.as_str())) {
            s += 2.0;
        }
        if kind.contains(term.as_str()) {
            s += 2.0;
        }
        s += body.matches(term.as_str()).count().min(20) as f64;
        if s > 0.0 {
            matched += 1;
            total += s;
        }
    }

    if matched == 0 {
        return None;
    }
    let coverage = matched as f64 / terms.len() as f64;
    Some(total * coverage * coverage + if matched == terms.len() { 10.0 } else { 0.0 })
}

/// First body line containing a query term, or the first prose line.
pub(crate) fn snippet(memory: &Memory, terms: &[String]) -> String {
    let lines = || memory.body.lines().map(str::trim).filter(|l| !l.is_empty());
    let line = lines()
        .find(|line| {
            let lower = line.to_lowercase();
            terms.iter().any(|t| lower.contains(t.as_str()))
        })
        .or_else(|| lines().find(|l| !l.starts_with('#')))
        .unwrap_or("");
    truncate(line, SNIPPET_CHARS)
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_string(),
    }
}
