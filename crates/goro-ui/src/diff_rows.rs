//! Rendering of the diff stream's rows: file headers, hunk headers, lines and notes.

use goro_core::diff::LineKind;
use goro_core::repo::Section;
use goro_core::review::{Note, Review, Row, display_line};
use goro_core::syntax::spans_in;
use gpui_kit::{
    AnyElement, ClickEvent, Context, Div, FontWeight, HighlightStyle, SharedString, Stateful,
    StyledText, div, prelude::*, px,
};

use crate::theme::Theme;
use crate::{GoroView, Op, ROW_HEIGHT, section_label};

const GUTTER_DIGITS: usize = 5;

pub(crate) fn gutter_width() -> gpui_kit::Pixels {
    // Two line-number columns plus the sign column, in monospace cells.
    px((GUTTER_DIGITS * 2 + 3) as f32 * 7.6)
}

pub(crate) fn render_row(
    review: &Review,
    ix: usize,
    is_cursor: bool,
    is_selected: bool,
    theme: &Theme,
    cx: &mut Context<GoroView>,
) -> AnyElement {
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
                .children(action_buttons(section, ix, "hunk", theme, cx))
        }
        Row::Line { file, line } => render_line(review, file, line, ix, theme),
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

fn render_line(
    review: &Review,
    file: usize,
    line: usize,
    ix: usize,
    theme: &Theme,
) -> Stateful<Div> {
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
    let number = |n: Option<u32>| {
        n.map(|n| format!("{n:>width$}", width = GUTTER_DIGITS))
            .unwrap_or_else(|| " ".repeat(GUTTER_DIGITS))
    };
    let gutter = format!("{} {} {sign} ", number(l.old_no), number(l.new_no));
    let mut content = StyledText::new(SharedString::from(text));
    if !highlights.is_empty() {
        content = content.with_highlights(highlights);
    }
    div()
        .id(("row", ix))
        .h(px(ROW_HEIGHT))
        .w_full()
        .flex()
        .flex_row()
        .whitespace_nowrap()
        .when_some(bg, |el, bg| el.bg(bg))
        .child(
            div()
                .flex_none()
                .w(gutter_width())
                .text_color(theme.muted)
                .when_some(gutter_bg, |el, bg| el.bg(bg))
                .child(gutter),
        )
        .child(div().pl_1().child(content))
        .when(l.no_newline, |el| {
            el.child(div().pl_2().text_color(theme.muted).child("⏎̸"))
        })
}
