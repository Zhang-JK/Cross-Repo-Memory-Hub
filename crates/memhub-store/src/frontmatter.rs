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
    let yaml = serde_yaml::to_string(&memory.meta)
        .map_err(|e| StoreError::Invalid(format!("cannot serialize front matter: {e}")))?;
    let mut out = String::with_capacity(yaml.len() + memory.body.len() + 16);
    out.push_str(FENCE);
    out.push('\n');
    out.push_str(&yaml);
    if !yaml.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(FENCE);
    out.push_str("\n\n");
    out.push_str(&normalize_body(&memory.body));
    Ok(out)
}

fn split(text: &str) -> (Option<&str>, &str) {
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
