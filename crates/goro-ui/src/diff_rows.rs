//! Rendering of the diff stream's rows: file headers, hunk headers, lines and notes.

use goro_core::diff::LineKind;
use goro_core::repo::Loaded;
use goro_core::repo::Section;
use goro_core::review::{Content, IMAGE_ROWS, Note, Review, Row, display_line};
use goro_core::syntax::spans_in;
use gpui_kit::{
    AnyElement, ClickEvent, Context, Div, FontWeight, HighlightStyle, SharedString, Stateful,
    StyledText, div, prelude::*, px,
};

use crate::theme::Theme;
use crate::view::{GoroView, Op, ROW_HEIGHT, section_label};

const GUTTER_DIGITS: usize = 5;

pub(crate) fn gutter_width() -> gpui_kit::Pixels {
    // Two line-number columns plus the sign column, in monospace cells.
    px((GUTTER_DIGITS * 2 + 3) as f32 * 7.6)
}

#[derive(Clone, Copy)]
pub(crate) struct RowFlags {
    pub is_cursor: bool,
    pub is_selected: bool,
    /// A changed line added since the last look.
    pub is_new: bool,
}

pub(crate) fn render_row(
    view: &GoroView,
    review: &Review,
    ix: usize,
    flags: RowFlags,
    theme: &Theme,
    cx: &mut Context<GoroView>,
) -> AnyElement {
    let RowFlags {
        is_cursor,
        is_selected,
        is_new,
    } = flags;
    let row = review.rows()[ix];
    let el = match row {
        Row::File { file } => file_header(review, file, Some(ix), theme, cx).h(px(ROW_HEIGHT)),
        Row::Hunk { file, hunk } => {
            let diff = review.files[file].diff().expect("hunk rows have diffs");
            let h = &diff.hunks[hunk];
            let mut header = format!(
                "@@ -{},{} +{},{} @@",
                h.old_start, h.old_len, h.new_start, h.new_len
            );
            if let Some(func) = diff.func_context_bytes(h) {
                header.push(' ');
                header.push_str(&String::from_utf8_lossy(func));
            }
            let section = review.files[file].change.section;
            let reviewed = review.hunk_is_reviewed(file, hunk);
            div()
                .id(("row", ix))
                .h(px(ROW_HEIGHT))
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .pl(gutter_width())
                .bg(theme.hunk_bg)
                .text_color(theme.hunk_fg)
                .whitespace_nowrap()
                .child(header)
                .when(reviewed, |el| {
                    el.child(div().text_color(theme.muted).child("✓ reviewed"))
                })
                .children(action_buttons(section, ix, "hunk", theme, cx))
        }
        Row::Line { file, line } => render_line(review, file, line, ix, theme),
        Row::Pair { file, old, new } => div()
            .id(("row", ix))
            .h(px(ROW_HEIGHT))
            .w_full()
            .flex()
            .flex_row()
            .child(half(review, file, old, Half::Old, theme))
            .child(div().w(px(1.0)).h_full().bg(theme.border))
            .child(half(review, file, new, Half::New, theme)),
        Row::Image { file, part } => {
            let row = div().id(("row", ix)).h(px(ROW_HEIGHT)).w_full();
            match (&review.files[file].content, part) {
                (Content::Loaded(Loaded::Image { kind, old, new }), 0) => row.relative().child(
                    div()
                        .absolute()
                        .top_0()
                        .left(gutter_width())
                        .h(px(ROW_HEIGHT * IMAGE_ROWS as f32))
                        .flex()
                        .gap_4()
                        .py_2()
                        .child(image_panel(
                            "Before",
                            old.as_ref().map(|b| view.image_for(b, *kind)),
                            theme,
                        ))
                        .child(image_panel(
                            "After",
                            new.as_ref().map(|b| view.image_for(b, *kind)),
                            theme,
                        )),
                ),
                _ => row,
            }
        }
        Row::Comment { comment, line, .. } => {
            let c = &review.comments()[comment];
            let text = c.text.lines().nth(line).unwrap_or_default().to_string();
            div()
                .id(("row", ix))
                .h(px(ROW_HEIGHT))
                .w_full()
                .flex()
                .items_center()
                .pl(gutter_width())
                .bg(theme.comment_bg)
                .whitespace_nowrap()
                .child(
                    div()
                        .w(px(24.0))
                        .flex_none()
                        .text_color(theme.accent)
                        .child(if line == 0 { "💬" } else { "" }),
                )
                .child(text)
        }
        Row::Note { note, .. } => div()
            .id(("row", ix))
            .h(px(ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .pl(gutter_width())
            .text_color(theme.muted)
            .whitespace_nowrap()
            .child(note_text(note)),
    };
    // Overlays, so the cursor and selection neither shift content nor recolor borders.
    el.relative()
        .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
            this.click_row(ix, event.modifiers().shift, cx);
        }))
        .when(is_selected, |el| {
            el.child(div().absolute().inset_0().bg(theme.selection_bg))
        })
        .when(is_new, |el| {
            el.child(
                div()
                    .absolute()
                    .left(px(4.0))
                    .top(px(6.0))
                    .size(px(7.0))
                    .rounded_full()
                    .bg(theme.new_marker),
            )
        })
        .when(is_cursor, |el| {
            el.child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(3.0))
                    .bg(theme.accent),
            )
        })
        .into_any_element()
}

/// A file's header. `row` is its row in the stream (for its action buttons); the sticky
/// header passes the same row.
pub(crate) fn file_header(
    review: &Review,
    file: usize,
    row: Option<usize>,
    theme: &Theme,
    cx: &mut Context<GoroView>,
) -> Stateful<Div> {
    let change = &review.files[file].change;
    let (badge, color) = theme.status(change.status);
    let path = match &change.old_path {
        Some(old) => format!("{old} → {}", change.path),
        None => change.path_lossy().into_owned(),
    };
    let row_ix = row.unwrap_or(review.file_row(file));
    div()
        .id(("row", row_ix))
        .h(px(ROW_HEIGHT * 1.5))
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .bg(theme.file_header_bg)
        .border_b_1()
        .border_t_1()
        .border_color(theme.border)
        .whitespace_nowrap()
        .child(
            div()
                .text_color(color)
                .font_weight(FontWeight::BOLD)
                .child(badge),
        )
        .child(div().font_weight(FontWeight::BOLD).child(path))
        .when(review.file_is_reviewed(file), |el| {
            el.child(div().text_color(theme.muted).child("✓ reviewed"))
        })
        .child(
            div()
                .text_color(theme.muted)
                .child(section_label(change.section).to_lowercase()),
        )
        .children(action_buttons(change.section, row_ix, "file", theme, cx))
}

fn action_buttons(
    section: Section,
    row: usize,
    scope: &'static str,
    theme: &Theme,
    cx: &mut Context<GoroView>,
) -> Vec<AnyElement> {
    // A turn's snapshots are read-only.
    if section == Section::Snapshot {
        return Vec::new();
    }
    let stage_label = if section == Section::Staged {
        "Unstage"
    } else {
        "Stage"
    };
    let mut buttons = vec![button(
        (scope, row * 2),
        format!("{stage_label} {scope}"),
        theme,
        cx.listener(move |this, _: &ClickEvent, _, cx| {
            cx.stop_propagation();
            this.act_on_row(row, Op::Stage, cx);
        }),
    )];
    if section != Section::Staged {
        buttons.push(button(
            (scope, row * 2 + 1),
            format!("Discard {scope}"),
            theme,
            cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.act_on_row(row, Op::Discard, cx);
            }),
        ));
    }
    buttons
}

pub(crate) fn button(
    id: impl Into<gpui_kit::ElementId>,
    label: impl Into<SharedString>,
    theme: &Theme,
    on_click: impl Fn(&ClickEvent, &mut gpui_kit::Window, &mut gpui_kit::App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .px_1p5()
        .rounded_sm()
        .text_color(theme.muted)
        .border_1()
        .border_color(theme.border)
        .cursor_pointer()
        .hover(|s| s.text_color(theme.fg).bg(theme.active_row_bg))
        .on_click(on_click)
        .child(label.into())
        .into_any_element()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Half {
    Old,
    New,
}

/// One side of a split row: its line number and content (empty if the side has no line).
fn half(review: &Review, file: usize, line: Option<usize>, side: Half, theme: &Theme) -> Div {
    let el = div()
        .h_full()
        .w_1_2()
        .flex()
        .flex_row()
        .overflow_hidden()
        .whitespace_nowrap();
    let Some(line) = line else {
        return el.bg(theme.file_header_bg);
    };
    let cell = line_cell(review, file, line, theme);
    let number = match side {
        Half::Old => cell.old_no,
        Half::New => cell.new_no,
    };
    el.when_some(cell.bg, |el, bg| el.bg(bg))
        .child(
            div()
                .flex_none()
                .w(px((GUTTER_DIGITS + 3) as f32 * 7.6))
                .text_color(theme.muted)
                .when_some(cell.gutter_bg, |el, bg| el.bg(bg))
                .child(format!(
                    "{} {} ",
                    number
                        .map(|n| format!("{n:>width$}", width = GUTTER_DIGITS))
                        .unwrap_or_else(|| " ".repeat(GUTTER_DIGITS)),
                    cell.sign
                )),
        )
        .child(div().pl_1().child(cell.content))
}

/// One side of an image preview, sized to fit inside the rows reserved for it.
fn image_panel(label: &str, image: Option<std::sync::Arc<gpui_kit::Image>>, theme: &Theme) -> Div {
    const WIDTH: f32 = 320.0;
    // The reserved rows, minus the label and padding.
    let height = ROW_HEIGHT * IMAGE_ROWS as f32 - ROW_HEIGHT - 24.0;
    let frame = div()
        .w(px(WIDTH))
        .h(px(height))
        .overflow_hidden()
        .border_1()
        .border_color(theme.border);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_color(theme.muted).child(label.to_string()))
        .child(match image {
            Some(image) => frame.child(
                gpui_kit::img(image)
                    .w(px(WIDTH - 2.0))
                    .h(px(height - 2.0))
                    .object_fit(gpui_kit::ObjectFit::Contain),
            ),
            None => frame
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted)
                .child("(none)"),
        })
}

fn note_text(note: Note) -> String {
    match note {
        Note::Loading => "Loading…".into(),
        Note::Binary { old_len, new_len } => {
            format!("Binary file changed ({old_len} → {new_len} bytes)")
        }
        Note::TooLarge { len } => format!("File too large to diff ({len} bytes)"),
        Note::Submodule => "Submodule changed".into(),
        Note::Conflict => "Unresolved merge conflict".into(),
        Note::NotAFile => "Not a regular file".into(),
        Note::NoContentChange => "No content changes (mode or rename only)".into(),
        Note::Failed => "Failed to load this file".into(),
    }
}

/// The pieces of a rendered diff line.
struct LineCell {
    old_no: Option<u32>,
    new_no: Option<u32>,
    sign: &'static str,
    bg: Option<gpui_kit::Hsla>,
    gutter_bg: Option<gpui_kit::Hsla>,
    content: StyledText,
    no_newline: bool,
}

fn line_cell(review: &Review, file: usize, line: usize, theme: &Theme) -> LineCell {
    let entry = &review.files[file];
    let diff = entry.diff().expect("line rows have diffs");
    let l = &diff.lines[line];
    let spans = entry.highlights.as_ref().map(|h| match l.kind {
        LineKind::Removed => &h.old,
        _ => &h.new,
    });
    let (text, spans) = display_line(
        diff.line_bytes(l),
        spans
            .into_iter()
            .flat_map(|spans| spans_in(spans, l.content.clone())),
    );
    let highlights: Vec<_> = spans
        .into_iter()
        .map(|(range, style)| {
            (
                range,
                HighlightStyle {
                    color: Some(theme.syntax(style)),
                    ..Default::default()
                },
            )
        })
        .collect();
    let (bg, gutter_bg, sign) = match l.kind {
        LineKind::Context => (None, None, " "),
        LineKind::Added => (Some(theme.added_bg), Some(theme.added_gutter_bg), "+"),
        LineKind::Removed => (Some(theme.removed_bg), Some(theme.removed_gutter_bg), "-"),
    };
    let mut content = StyledText::new(SharedString::from(text));
    if !highlights.is_empty() {
        content = content.with_highlights(highlights);
    }
    LineCell {
        old_no: l.old_no,
        new_no: l.new_no,
        sign,
        bg,
        gutter_bg,
        content,
        no_newline: l.no_newline,
    }
}

fn render_line(
    review: &Review,
    file: usize,
    line: usize,
    ix: usize,
    theme: &Theme,
) -> Stateful<Div> {
    let cell = line_cell(review, file, line, theme);
    let number = |n: Option<u32>| {
        n.map(|n| format!("{n:>width$}", width = GUTTER_DIGITS))
            .unwrap_or_else(|| " ".repeat(GUTTER_DIGITS))
    };
    let gutter = format!(
        "{} {} {} ",
        number(cell.old_no),
        number(cell.new_no),
        cell.sign
    );
    div()
        .id(("row", ix))
        .h(px(ROW_HEIGHT))
        .w_full()
        .flex()
        .flex_row()
        .whitespace_nowrap()
        .when_some(cell.bg, |el, bg| el.bg(bg))
        .child(
            div()
                .flex_none()
                .w(gutter_width())
                .text_color(theme.muted)
                .when_some(cell.gutter_bg, |el, bg| el.bg(bg))
                .child(gutter),
        )
        .child(div().pl_1().child(cell.content))
        .when(cell.no_newline, |el| {
            el.child(div().pl_2().text_color(theme.muted).child("⏎̸"))
        })
}
