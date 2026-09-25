//! Light and dark palettes, chosen from the OS appearance.

use goro_core::repo::ChangeStatus;
use goro_core::syntax::Style;
use gpui_kit::{Hsla, WindowAppearance, rgb};

pub struct Theme {
    pub bg: Hsla,
    pub fg: Hsla,
    pub muted: Hsla,
    pub border: Hsla,
    pub sidebar_bg: Hsla,
    pub file_header_bg: Hsla,
    pub hunk_bg: Hsla,
    pub hunk_fg: Hsla,
    pub added_bg: Hsla,
    pub added_gutter_bg: Hsla,
    pub removed_bg: Hsla,
    pub removed_gutter_bg: Hsla,
    pub accent: Hsla,
    pub active_row_bg: Hsla,
    pub selection_bg: Hsla,
    pub error: Hsla,
    added: Hsla,
    modified: Hsla,
    deleted: Hsla,
    renamed: Hsla,
    syntax: [Hsla; 17],
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

impl Theme {
    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
        }
    }

    fn dark() -> Self {
        Self {
            bg: c(0x0d1117),
            fg: c(0xe6edf3),
            muted: c(0x7d8590),
            border: c(0x30363d),
            sidebar_bg: c(0x010409),
            file_header_bg: c(0x161b22),
            hunk_bg: c(0x121d2f),
            hunk_fg: c(0x9198a1),
            added_bg: c(0x12261e),
            added_gutter_bg: c(0x1b4721),
            removed_bg: c(0x25171c),
            removed_gutter_bg: c(0x542426),
            accent: c(0x2f81f7),
            active_row_bg: c(0x1f2937),
            selection_bg: c(0x388bfd).opacity(0.22),
            error: c(0xf85149),
            added: c(0x3fb950),
            modified: c(0xd29922),
            deleted: c(0xf85149),
            renamed: c(0x58a6ff),
            syntax: syntax_palette([
                0x79c0ff, // attribute
                0x8b949e, // comment
                0x79c0ff, // constant
                0xd2a8ff, // function
                0xff7b72, // keyword
                0x79c0ff, // label
                0x79c0ff, // number
                0xe6edf3, // operator
                0x79c0ff, // property
                0xc9d1d9, // punctuation
                0xa5d6ff, // string
                0x79c0ff, // escape
                0x7ee787, // tag
                0xffa657, // type
                0xffa657, // variable
                0x79c0ff, // heading
                0xa5d6ff, // link
            ]),
        }
    }

    fn light() -> Self {
        Self {
            bg: c(0xffffff),
            fg: c(0x1f2328),
            muted: c(0x656d76),
            border: c(0xd0d7de),
            sidebar_bg: c(0xf6f8fa),
            file_header_bg: c(0xf6f8fa),
            hunk_bg: c(0xddf4ff),
            hunk_fg: c(0x59636e),
            added_bg: c(0xe6ffec),
            added_gutter_bg: c(0xccffd8),
            removed_bg: c(0xffebe9),
            removed_gutter_bg: c(0xffd7d5),
            accent: c(0x0969da),
            active_row_bg: c(0xeaeef2),
            selection_bg: c(0x0969da).opacity(0.14),
            error: c(0xd1242f),
            added: c(0x1a7f37),
            modified: c(0x9a6700),
            deleted: c(0xd1242f),
            renamed: c(0x0969da),
            syntax: syntax_palette([
                0x0550ae, // attribute
                0x6e7781, // comment
                0x0550ae, // constant
                0x8250df, // function
                0xcf222e, // keyword
                0x0550ae, // label
                0x0550ae, // number
                0x1f2328, // operator
                0x0550ae, // property
                0x24292f, // punctuation
                0x0a3069, // string
                0x0550ae, // escape
                0x116329, // tag
                0x953800, // type
                0x953800, // variable
                0x0550ae, // heading
                0x0a3069, // link
            ]),
        }
    }

    pub fn syntax(&self, style: Style) -> Hsla {
        self.syntax[match style {
            Style::Attribute => 0,
            Style::Comment => 1,
            Style::Constant => 2,
            Style::Function => 3,
            Style::Keyword => 4,
            Style::Label => 5,
            Style::Number => 6,
            Style::Operator => 7,
            Style::Property => 8,
            Style::Punctuation => 9,
            Style::String => 10,
            Style::Escape => 11,
            Style::Tag => 12,
            Style::Type => 13,
            Style::Variable => 14,
            Style::Heading => 15,
            Style::Link => 16,
        }]
    }

    pub fn status(&self, status: ChangeStatus) -> (&'static str, Hsla) {
        match status {
            ChangeStatus::Added => ("A", self.added),
            ChangeStatus::Modified => ("M", self.modified),
            ChangeStatus::Deleted => ("D", self.deleted),
            ChangeStatus::Renamed => ("R", self.renamed),
            ChangeStatus::Copied => ("C", self.renamed),
            ChangeStatus::TypeChanged => ("T", self.modified),
            ChangeStatus::Conflicted => ("U", self.deleted),
        }
    }
}

fn syntax_palette(hex: [u32; 17]) -> [Hsla; 17] {
    hex.map(c)
}
