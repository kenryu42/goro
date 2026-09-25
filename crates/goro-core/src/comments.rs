//! Review comments on diff lines, exported as markdown an agent can act on.

use serde::{Deserialize, Serialize};

use crate::diff::{FileDiff, LineKind};

/// Which side of the diff a comment's line numbers refer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Old,
    New,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub path: String,
    pub side: Side,
    /// 1-based, inclusive.
    pub start: u32,
    pub end: u32,
    /// The commented diff lines, each prefixed with `+`, `-` or ` `.
    pub excerpt: Vec<String>,
    pub text: String,
}

impl Comment {
    /// A comment on `lines` (indices into `diff.lines`) of `path`. The range is on the new
    /// side unless every selected line is a removal.
    pub fn on_lines(
        id: u64,
        path: &str,
        diff: &FileDiff,
        lines: &[usize],
        text: String,
    ) -> Option<Self> {
        let selected: Vec<_> = lines.iter().filter_map(|&ix| diff.lines.get(ix)).collect();
        let side = if selected.iter().all(|l| l.kind == LineKind::Removed) {
            Side::Old
        } else {
            Side::New
        };
        let numbers: Vec<u32> = selected
            .iter()
            .filter_map(|l| match side {
                Side::Old => l.old_no,
                Side::New => l.new_no,
            })
            .collect();
        let (start, end) = (*numbers.iter().min()?, *numbers.iter().max()?);
        let excerpt = selected
            .iter()
            .map(|l| {
                let prefix = match l.kind {
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                    LineKind::Context => ' ',
                };
                format!("{prefix}{}", String::from_utf8_lossy(diff.line_bytes(l)))
            })
            .collect();
        Some(Self {
            id,
            path: path.to_string(),
            side,
            start,
            end,
            excerpt,
            text,
        })
    }

    pub fn location(&self) -> String {
        let side = match self.side {
            Side::Old => " (removed lines)",
            Side::New => "",
        };
        if self.start == self.end {
            format!("{}:{}{side}", self.path, self.start)
        } else {
            format!("{}:{}-{}{side}", self.path, self.start, self.end)
        }
    }
}

/// Markdown for an agent: each comment with its location and the diff it refers to.
pub fn to_markdown(comments: &[Comment]) -> String {
    if comments.is_empty() {
        return "No review comments.\n".to_string();
    }
    let mut sorted: Vec<&Comment> = comments.iter().collect();
    sorted.sort_by(|a, b| (&a.path, a.start).cmp(&(&b.path, b.start)));
    let mut out = format!(
        "# Review comments ({})\n\nAddress each comment below; line numbers refer to the current files.\n",
        comments.len()
    );
    for comment in sorted {
        out.push_str(&format!("\n## {}\n\n```diff\n", comment.location()));
        for line in &comment.excerpt {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("```\n\n");
        out.push_str(comment.text.trim());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff() -> FileDiff {
        FileDiff::compute(
            b"fn a() {\n    old();\n}\n".to_vec(),
            b"fn a() {\n    new();\n    more();\n}\n".to_vec(),
            3,
        )
    }

    #[test]
    fn comments_anchor_on_the_new_side_unless_only_removals() {
        let diff = diff();
        let kinds: Vec<LineKind> = diff.lines.iter().map(|l| l.kind).collect();
        let removed = kinds.iter().position(|k| *k == LineKind::Removed).unwrap();
        let added: Vec<usize> = (0..kinds.len())
            .filter(|&i| kinds[i] == LineKind::Added)
            .collect();

        let on_added =
            Comment::on_lines(1, "src/a.rs", &diff, &added, "why two calls?".into()).unwrap();
        assert_eq!(
            (on_added.side, on_added.start, on_added.end),
            (Side::New, 2, 3)
        );
        assert_eq!(on_added.excerpt, ["+    new();", "+    more();"]);

        let on_removed =
            Comment::on_lines(2, "src/a.rs", &diff, &[removed], "keep this".into()).unwrap();
        assert_eq!((on_removed.side, on_removed.start), (Side::Old, 2));
        assert_eq!(on_removed.location(), "src/a.rs:2 (removed lines)");
    }

    #[test]
    fn markdown_lists_comments_by_location() {
        let diff = diff();
        let added: Vec<usize> = (0..diff.lines.len())
            .filter(|&i| diff.lines[i].kind == LineKind::Added)
            .collect();
        let b =
            Comment::on_lines(1, "src/b.rs", &diff, &added[..1], "Rename this.".into()).unwrap();
        let a =
            Comment::on_lines(2, "src/a.rs", &diff, &added, "  Handle errors.  ".into()).unwrap();
        assert_eq!(
            to_markdown(&[b, a]),
            "# Review comments (2)\n\n\
             Address each comment below; line numbers refer to the current files.\n\
             \n## src/a.rs:2-3\n\n```diff\n+    new();\n+    more();\n```\n\nHandle errors.\n\
             \n## src/b.rs:2\n\n```diff\n+    new();\n```\n\nRename this.\n"
        );
        assert_eq!(to_markdown(&[]), "No review comments.\n");
    }
}
