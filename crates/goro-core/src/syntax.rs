//! Tree-sitter syntax highlighting, independent of any UI.
//!
//! Highlighting produces byte-range spans tagged with a small, theme-agnostic set of
//! [`Style`]s; the UI maps those to colors.

use std::cell::RefCell;
use std::ops::Range;
use std::sync::OnceLock;

use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
    Json,
    Markdown,
}

const LANGUAGE_COUNT: usize = 8;

impl Language {
    pub fn for_path(path: &str) -> Option<Self> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let ext = name.rsplit_once('.').map(|(_, ext)| ext)?;
        Some(match ext.to_ascii_lowercase().as_str() {
            "rs" => Self::Rust,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Self::JavaScript,
            "py" | "pyi" => Self::Python,
            "go" => Self::Go,
            "json" | "jsonc" => Self::Json,
            "md" | "markdown" => Self::Markdown,
            _ => return None,
        })
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Theme-agnostic highlight classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Style {
    Attribute,
    Comment,
    Constant,
    Function,
    Keyword,
    Label,
    Number,
    Operator,
    Property,
    Punctuation,
    String,
    Escape,
    Tag,
    Type,
    Variable,
    Heading,
    Link,
}

/// Capture names we recognize, most specific first where it matters. tree-sitter matches
/// a capture like `function.method.call` against the longest recognized prefix.
const CAPTURES: &[(&str, Style)] = &[
    ("attribute", Style::Attribute),
    ("comment", Style::Comment),
    ("constant", Style::Constant),
    ("constant.builtin", Style::Constant),
    ("constructor", Style::Type),
    ("escape", Style::Escape),
    ("function", Style::Function),
    ("function.builtin", Style::Function),
    ("function.macro", Style::Function),
    ("function.method", Style::Function),
    ("keyword", Style::Keyword),
    ("label", Style::Label),
    ("module", Style::Type),
    ("number", Style::Number),
    ("operator", Style::Operator),
    ("property", Style::Property),
    ("punctuation", Style::Punctuation),
    ("string", Style::String),
    ("string.escape", Style::Escape),
    ("string.special", Style::String),
    ("tag", Style::Tag),
    ("type", Style::Type),
    ("type.builtin", Style::Type),
    ("variable.builtin", Style::Keyword),
    ("variable.parameter", Style::Variable),
    ("text.title", Style::Heading),
    ("text.literal", Style::String),
    ("text.uri", Style::Link),
    ("text.reference", Style::Link),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub style: Style,
}

static CONFIGS: [OnceLock<Option<HighlightConfiguration>>; LANGUAGE_COUNT] =
    [const { OnceLock::new() }; LANGUAGE_COUNT];

thread_local! {
    static HIGHLIGHTER: RefCell<Highlighter> = RefCell::new(Highlighter::new());
}

fn config(language: Language) -> Option<&'static HighlightConfiguration> {
    CONFIGS[language.index()]
        .get_or_init(|| {
            let (lang, name, highlights, injections, locals): (
                tree_sitter::Language,
                &str,
                String,
                &str,
                &str,
            ) = match language {
                Language::Rust => (
                    tree_sitter_rust::LANGUAGE.into(),
                    "rust",
                    tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
                    "",
                    "",
                ),
                // The TypeScript queries extend the JavaScript ones.
                Language::TypeScript => (
                    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                    "typescript",
                    [
                        tree_sitter_typescript::HIGHLIGHTS_QUERY,
                        tree_sitter_javascript::HIGHLIGHT_QUERY,
                    ]
                    .concat(),
                    "",
                    tree_sitter_typescript::LOCALS_QUERY,
                ),
                Language::Tsx => (
                    tree_sitter_typescript::LANGUAGE_TSX.into(),
                    "tsx",
                    [
                        tree_sitter_typescript::HIGHLIGHTS_QUERY,
                        tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                        tree_sitter_javascript::HIGHLIGHT_QUERY,
                    ]
                    .concat(),
                    "",
                    tree_sitter_typescript::LOCALS_QUERY,
                ),
                Language::JavaScript => (
                    tree_sitter_javascript::LANGUAGE.into(),
                    "javascript",
                    [
                        tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                        tree_sitter_javascript::HIGHLIGHT_QUERY,
                    ]
                    .concat(),
                    "",
                    tree_sitter_javascript::LOCALS_QUERY,
                ),
                Language::Python => (
                    tree_sitter_python::LANGUAGE.into(),
                    "python",
                    tree_sitter_python::HIGHLIGHTS_QUERY.into(),
                    "",
                    "",
                ),
                Language::Go => (
                    tree_sitter_go::LANGUAGE.into(),
                    "go",
                    tree_sitter_go::HIGHLIGHTS_QUERY.into(),
                    "",
                    "",
                ),
                Language::Json => (
                    tree_sitter_json::LANGUAGE.into(),
                    "json",
                    tree_sitter_json::HIGHLIGHTS_QUERY.into(),
                    "",
                    "",
                ),
                Language::Markdown => (
                    tree_sitter_md::LANGUAGE.into(),
                    "markdown",
                    tree_sitter_md::HIGHLIGHT_QUERY_BLOCK.into(),
                    "",
                    "",
                ),
            };
            let mut config =
                HighlightConfiguration::new(lang, name, &highlights, injections, locals).ok()?;
            let names: Vec<&str> = CAPTURES.iter().map(|(name, _)| *name).collect();
            config.configure(&names);
            Some(config)
        })
        .as_ref()
}

/// Highlight a whole file. Spans are sorted, non-overlapping, and use the innermost
/// capture. Returns an empty list if the language fails to load or parse.
pub fn highlight(language: Language, source: &[u8]) -> Vec<Span> {
    let Some(config) = config(language) else {
        return Vec::new();
    };
    HIGHLIGHTER.with_borrow_mut(|highlighter| {
        let Ok(events) = highlighter.highlight(config, source, None, None, |_| None) else {
            return Vec::new();
        };
        let mut spans: Vec<Span> = Vec::new();
        let mut stack: Vec<Style> = Vec::new();
        for event in events {
            match event {
                Ok(HighlightEvent::HighlightStart(h)) => stack.push(CAPTURES[h.0].1),
                Ok(HighlightEvent::HighlightEnd) => {
                    stack.pop();
                }
                Ok(HighlightEvent::Source { start, end }) => {
                    if let Some(&style) = stack.last() {
                        match spans.last_mut() {
                            Some(last) if last.range.end == start && last.style == style => {
                                last.range.end = end;
                            }
                            _ => spans.push(Span {
                                range: start..end,
                                style,
                            }),
                        }
                    }
                }
                Err(_) => return Vec::new(),
            }
        }
        spans
    })
}

/// The spans overlapping `range`, clipped to it and made relative to `range.start`.
pub fn spans_in(spans: &[Span], range: Range<usize>) -> impl Iterator<Item = Span> + '_ {
    let first = spans.partition_point(|s| s.range.end <= range.start);
    spans[first..]
        .iter()
        .take_while(move |s| s.range.start < range.end)
        .map(move |s| Span {
            range: s.range.start.max(range.start) - range.start
                ..s.range.end.min(range.end) - range.start,
            style: s.style,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled<'a>(source: &'a str, spans: &[Span]) -> Vec<(&'a str, Style)> {
        spans
            .iter()
            .map(|s| (&source[s.range.clone()], s.style))
            .collect()
    }

    #[test]
    fn detects_languages_by_extension() {
        assert_eq!(Language::for_path("src/main.rs"), Some(Language::Rust));
        assert_eq!(Language::for_path("a/b.TSX"), Some(Language::Tsx));
        assert_eq!(Language::for_path("README.md"), Some(Language::Markdown));
        assert_eq!(Language::for_path("Makefile"), None);
        assert_eq!(Language::for_path("dir.rs/file"), None);
    }

    #[test]
    fn highlights_rust() {
        let source = "fn main() {\n    let x = \"hi\"; // note\n}\n";
        let spans = highlight(Language::Rust, source.as_bytes());
        let styled = styled(source, &spans);
        assert!(styled.contains(&("fn", Style::Keyword)), "{styled:?}");
        assert!(styled.contains(&("main", Style::Function)), "{styled:?}");
        assert!(styled.contains(&("\"hi\"", Style::String)), "{styled:?}");
        assert!(styled.contains(&("// note", Style::Comment)), "{styled:?}");
    }

    #[test]
    fn every_language_loads_and_highlights() {
        let samples = [
            (Language::Rust, "fn a() {}"),
            (Language::TypeScript, "const a: number = 1;"),
            (Language::Tsx, "const a = <div className=\"x\" />;"),
            (Language::JavaScript, "function a() { return 1; }"),
            (Language::Python, "def a():\n    return 1\n"),
            (Language::Go, "package main\nfunc a() {}\n"),
            (Language::Json, "{\"a\": 1}"),
            (Language::Markdown, "# Title\n\ntext\n"),
        ];
        for (language, source) in samples {
            assert!(
                !highlight(language, source.as_bytes()).is_empty(),
                "{language:?}"
            );
        }
    }

    #[test]
    fn spans_in_clips_to_line() {
        let spans = vec![
            Span {
                range: 0..4,
                style: Style::Keyword,
            },
            Span {
                range: 6..12,
                style: Style::String,
            },
        ];
        let clipped: Vec<_> = spans_in(&spans, 2..8).collect();
        assert_eq!(
            clipped,
            vec![
                Span {
                    range: 0..2,
                    style: Style::Keyword
                },
                Span {
                    range: 4..6,
                    style: Style::String
                },
            ]
        );
    }
}
