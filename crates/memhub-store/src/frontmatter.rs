//! Encoding of a memory as Markdown with a YAML front-matter header:
//!
//! ```text
//! ---
//! id: repo/memhub/build-system
//! title: Build system
//! kind: architecture
//! tags: [cargo, ci]
//! ...
//! ---
//!
//! Markdown body
//! ```

use std::path::Path;

use crate::{
    error::{Result, StoreError},
    model::{Memory, MemoryId, MemoryMeta},
};

const FENCE: &str = "---";

/// Parses a memory file. Files without front matter are accepted and get
/// default properties, so humans can drop plain Markdown notes into the store.
pub fn parse(text: &str, id: MemoryId, path: &Path) -> Result<Memory> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (yaml, body) = split(text);

    let mut meta: MemoryMeta = match yaml {
        Some(yaml) if !yaml.trim().is_empty() => {
            serde_yaml::from_str(yaml).map_err(|e| StoreError::Malformed {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?
        }
        _ => serde_yaml::from_str("{}").expect("empty front matter deserializes"),
    };

    meta.id = id;
    if meta.title.trim().is_empty() {
        meta.title = body
            .lines()
            .find_map(|l| l.strip_prefix("# "))
            .map(|t| t.trim().to_string())
            .unwrap_or_else(|| meta.id.slug.clone());
    }
    Ok(Memory {
        meta,
        body: normalize_body(body),
    })
}

/// Canonical body form as written to disk: no leading blank lines, exactly
/// one trailing newline (or empty).
pub(crate) fn normalize_body(body: &str) -> String {
    let body = body.trim_start_matches(['\r', '\n']).trim_end();
    if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
    }
}

pub fn render(memory: &Memory) -> Result<String> {
    render_parts(&memory.meta, &memory.body)
}

pub(crate) fn render_parts(meta: &impl serde::Serialize, body: &str) -> Result<String> {
    let yaml = serde_yaml::to_string(meta)
        .map_err(|e| StoreError::Invalid(format!("cannot serialize front matter: {e}")))?;
    let mut out = String::with_capacity(yaml.len() + body.len() + 16);
    out.push_str(FENCE);
    out.push('\n');
    out.push_str(&yaml);
    if !yaml.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(FENCE);
    out.push_str("\n\n");
    out.push_str(&normalize_body(body));
    Ok(out)
}

pub(crate) fn split(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text
        .strip_prefix("---\r\n")
        .or_else(|| text.strip_prefix("---\n"))
    else {
        return (None, text);
    };

    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == FENCE || trimmed == "..." {
            let yaml = &rest[..offset];
            let body = &rest[offset + line.len()..];
            let body = body
                .strip_prefix("\r\n")
                .or_else(|| body.strip_prefix('\n'))
                .unwrap_or(body);
            return (Some(yaml), body);
        }
        offset += line.len();
    }
    // Unterminated front matter: treat the whole file as body.
    (None, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Scope;

    fn id() -> MemoryId {
        MemoryId::new(Scope::Repo, "demo", "note").unwrap()
    }

    #[test]
    fn roundtrip_preserves_props_and_extra_keys() {
        let text = "---\ntitle: Hello\nkind: pitfall\ntags: [a, b]\ncustom: 42\n---\n\nBody line\n";
        let mem = parse(text, id(), Path::new("x.md")).unwrap();
        assert_eq!(mem.meta.title, "Hello");
        assert_eq!(mem.meta.kind, "pitfall");
        assert_eq!(mem.meta.tags, vec!["a", "b"]);
        assert_eq!(mem.body, "Body line\n");
        assert!(mem.meta.extra.contains_key("custom"));

        let rendered = render(&mem).unwrap();
        let again = parse(&rendered, id(), Path::new("x.md")).unwrap();
        assert_eq!(again, mem);
    }

    #[test]
    fn plain_markdown_gets_defaults() {
        let mem = parse("# Title From Heading\n\ntext", id(), Path::new("x.md")).unwrap();
        assert_eq!(mem.meta.title, "Title From Heading");
        assert_eq!(mem.meta.kind, "note");
        assert_eq!(mem.meta.version, 1);
    }

    #[test]
    fn malformed_yaml_is_reported() {
        let err = parse("---\ntags: [unclosed\n---\n", id(), Path::new("x.md")).unwrap_err();
        assert!(matches!(err, StoreError::Malformed { .. }));
    }
}

#[cfg(test)]
mod experience_tests {
    use super::*;
    use crate::{experience::Status, model::Scope};

    #[test]
    fn experience_fields_round_trip_next_to_unknown_keys() {
        let id = MemoryId::new(Scope::Repo, "demo", "note").unwrap();
        let text = "---\ntitle: T\nstatus: established\ncites:\n  evidence: [evidence/2026-11-03/bot/x]\n  code: ['src/a.rs:3@abc']\nsupport: 2\ncustom: keep\n---\n\nBody\n";
        let mem = parse(text, id.clone(), Path::new("x.md")).unwrap();
        assert_eq!(mem.meta.experience.status, Some(Status::Established));
        assert_eq!(mem.meta.experience.support, 2);
        assert_eq!(mem.meta.experience.cites.code, vec!["src/a.rs:3@abc"]);
        assert_eq!(mem.meta.extra.len(), 1, "{:?}", mem.meta.extra);
        let again = parse(&render(&mem).unwrap(), id, Path::new("x.md")).unwrap();
        assert_eq!(again, mem);
        let plain = render(&parse("---\ntitle: P\n---\nx", MemoryId::new(Scope::Repo, "d", "p").unwrap(), Path::new("p.md")).unwrap()).unwrap();
        assert!(!plain.contains("status") && !plain.contains("cites"), "{plain}");
    }
}
