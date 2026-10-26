use similar::TextDiff;

const MAX_DIFF_BYTES: usize = 16 * 1024;

/// Unified line diff, or `None` when the texts are identical.
pub(crate) fn unified_diff(before: &str, after: &str) -> Option<String> {
    if before == after {
        return None;
    }
    let mut diff = TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(2)
        .header("before", "after")
        .to_string();
    if diff.len() > MAX_DIFF_BYTES {
        let mut cut = MAX_DIFF_BYTES;
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        diff.truncate(cut);
        diff.push_str("\n… (diff truncated)\n");
    }
    Some(diff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_multibyte_diffs_on_a_char_boundary() {
        let after = "重试策略 🔁 ".repeat(2000);
        let diff = unified_diff("", &after).unwrap();
        assert!(diff.ends_with("… (diff truncated)\n"));
        assert!(diff.len() <= MAX_DIFF_BYTES + 32);
        assert!(diff.contains("重试策略 🔁"));
    }
}
