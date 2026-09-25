//! The commit message editor: multi-line plain text with selection, clipboard, and IME.
//!
//! Adapted from GPUI's `examples/input.rs` (Apache-2.0), extended to multiple lines. The
//! editing model lives in [`TextBuffer`]; this module only lays out, paints, and routes
//! input.

use std::ops::Range;

use gpui_kit::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, Hsla, KeyBinding,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ScrollHandle, ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window,
    actions, div, fill, point, prelude::*, px, relative, size,
};

use crate::text_buffer::TextBuffer;

actions!(
    commit_editor,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        LineStart,
        LineEnd,
        SelectLineStart,
        SelectLineEnd,
        Newline,
        Paste,
        Cut,
        Copy
    ]
);

pub const CONTEXT: &str = "CommitEditor";
const MIN_LINES: usize = 3;

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("shift-up", SelectUp, c),
        KeyBinding::new("shift-down", SelectDown, c),
        KeyBinding::new("home", LineStart, c),
        KeyBinding::new("end", LineEnd, c),
        KeyBinding::new("shift-home", SelectLineStart, c),
        KeyBinding::new("shift-end", SelectLineEnd, c),
        KeyBinding::new("enter", Newline, c),
        KeyBinding::new("cmd-left", LineStart, c),
        KeyBinding::new("cmd-right", LineEnd, c),
        KeyBinding::new("cmd-shift-left", SelectLineStart, c),
        KeyBinding::new("cmd-shift-right", SelectLineEnd, c),
        KeyBinding::new("cmd-a", SelectAll, c),
        KeyBinding::new("cmd-v", Paste, c),
        KeyBinding::new("cmd-x", Cut, c),
        KeyBinding::new("cmd-c", Copy, c),
        KeyBinding::new("ctrl-a", SelectAll, c),
        KeyBinding::new("ctrl-v", Paste, c),
        KeyBinding::new("ctrl-x", Cut, c),
        KeyBinding::new("ctrl-c", Copy, c),
    ]);
}

#[derive(Clone, Copy)]
pub struct EditorColors {
    pub text: Hsla,
    pub placeholder: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
}

pub struct CommitEditor {
    focus_handle: FocusHandle,
    buffer: TextBuffer,
    placeholder: SharedString,
    colors: Option<EditorColors>,
    scroll: ScrollHandle,
    /// Per line: its byte offset in the text, and its shaped layout from the last paint.
    layouts: Vec<(usize, ShapedLine)>,
    bounds: Option<Bounds<Pixels>>,
    line_height: Pixels,
    is_selecting: bool,
}

impl CommitEditor {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            buffer: TextBuffer::default(),
            placeholder: placeholder.into(),
            colors: None,
            scroll: ScrollHandle::new(),
            layouts: Vec::new(),
            bounds: None,
            line_height: px(18.0),
            is_selecting: false,
        }
    }

    pub fn text(&self) -> &str {
        self.buffer.text()
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.buffer.set_text(text);
        cx.notify();
    }

    pub fn set_colors(&mut self, colors: EditorColors) {
        self.colors = Some(colors);
    }

    fn edit(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut TextBuffer)) {
        f(&mut self.buffer);
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, TextBuffer::backspace);
    }
    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, TextBuffer::delete);
    }
    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |b| b.left(false));
    }
    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |b| b.right(false));
    }
    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |b| b.up(false));
    }
    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |b| b.down(false));
    }
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |b| b.left(true));
    }
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |b| b.right(true));
    }
    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |b| b.up(true));
    }
    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |b| b.down(true));
    }
    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, TextBuffer::select_all);
    }
    fn line_start(&mut self, _: &LineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |b| b.line_start(false));
    }
    fn line_end(&mut self, _: &LineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |b| b.line_end(false));
    }
    fn select_line_start(&mut self, _: &SelectLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |b| b.line_start(true));
    }
    fn select_line_end(&mut self, _: &SelectLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |b| b.line_end(true));
    }
    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |b| b.insert("\n"));
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.edit(cx, |b| b.insert(&text.replace("\r\n", "\n")));
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let selection = self.buffer.selection();
        if !selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.buffer.text()[selection].to_string(),
            ));
        }
    }

    fn cut(&mut self, action: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        self.copy(&Copy, window, cx);
        let _ = action;
        if !self.buffer.selection().is_empty() {
            self.edit(cx, |b| b.insert(""));
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.is_selecting = true;
        let offset = self.index_for_position(event.position);
        if event.modifiers.shift {
            self.edit(cx, |b| b.select_to(offset));
        } else {
            self.edit(cx, |b| b.move_to(offset));
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let offset = self.index_for_position(event.position);
            self.edit(cx, |b| b.select_to(offset));
        }
    }

    fn line_at_y(&self, y: Pixels) -> Option<usize> {
        let bounds = self.bounds?;
        if self.layouts.is_empty() {
            return None;
        }
        let ix = ((y - bounds.top()) / self.line_height).floor().max(0.0) as usize;
        Some(ix.min(self.layouts.len() - 1))
    }

    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(line)) = (self.bounds, self.line_at_y(position.y)) else {
            return self.buffer.text().len();
        };
        let (start, layout) = &self.layouts[line];
        start + layout.closest_index_for_x(position.x - bounds.left())
    }

    /// Position of `offset`'s line and x within the last layout.
    fn position_of(&self, offset: usize) -> Option<(usize, Pixels)> {
        let line = self.buffer.line_of(offset);
        let (start, layout) = self.layouts.get(line)?;
        Some((line, layout.x_for_index(offset - start)))
    }
}

impl EntityInputHandler for CommitEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.buffer.range_from_utf16(&range_utf16);
        actual_range.replace(self.buffer.range_to_utf16(&range));
        Some(self.buffer.text()[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.buffer.range_to_utf16(&self.buffer.selection()),
            reversed: self.buffer.is_reversed(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.buffer
            .marked
            .as_ref()
            .map(|range| self.buffer.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.buffer.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.buffer.range_from_utf16(&r))
            .or(self.buffer.marked.clone())
            .unwrap_or(self.buffer.selection());
        self.edit(cx, |b| b.replace(range, new_text));
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.buffer.range_from_utf16(&r))
            .or(self.buffer.marked.clone())
            .unwrap_or(self.buffer.selection());
        self.buffer.replace(range.clone(), new_text);
        self.buffer.marked =
            (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        if let Some(selected) = new_selected_range_utf16 {
            // The selection is relative to the inserted text.
            let inserted = new_text;
            let to_byte = |utf16: usize| {
                let mut count = 0;
                for (ix, ch) in inserted.char_indices() {
                    if count >= utf16 {
                        return ix;
                    }
                    count += ch.len_utf16();
                }
                inserted.len()
            };
            let start = range.start + to_byte(selected.start);
            let end = range.start + to_byte(selected.end);
            self.buffer.move_to(start);
            self.buffer.select_to(end);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.buffer.range_from_utf16(&range_utf16);
        let (line, start_x) = self.position_of(range.start)?;
        let end_x = self
            .position_of(range.end)
            .filter(|(end_line, _)| *end_line == line)
            .map_or(start_x, |(_, x)| x);
        let top = bounds.top() + self.line_height * line as f32;
        Some(Bounds::from_corners(
            point(bounds.left() + start_x, top),
            point(bounds.left() + end_x, top + self.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let line = self.line_at_y(point.y)?;
        let (start, layout) = &self.layouts[line];
        let x = point.x - self.bounds?.left();
        let offset = start + layout.index_for_x(x)?;
        Some(self.buffer.offset_to_utf16(offset))
    }
}

impl Focusable for CommitEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CommitEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("commit-editor")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::line_start))
            .on_action(cx.listener(Self::line_end))
            .on_action(cx.listener(Self::select_line_start))
            .on_action(cx.listener(Self::select_line_end))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .size_full()
            .child(EditorElement {
                editor: cx.entity(),
            })
    }
}

struct EditorElement {
    editor: Entity<CommitEditor>,
}

struct Prepaint {
    lines: Vec<(usize, ShapedLine)>,
    selections: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    placeholder: Option<ShapedLine>,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui_kit::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let lines = self.editor.read(cx).buffer.lines().len().max(MIN_LINES);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * lines as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui_kit::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let editor = self.editor.read(cx);
        let style = window.text_style();
        let colors = editor.colors.unwrap_or(EditorColors {
            text: style.color,
            placeholder: style.color.opacity(0.4),
            cursor: style.color,
            selection: style.color.opacity(0.25),
        });
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let buffer = &editor.buffer;
        let text = buffer.text();
        let selection = buffer.selection();
        let marked = buffer.marked.clone();

        let mut lines = Vec::new();
        let mut selections = Vec::new();
        for (ix, range) in buffer.lines().into_iter().enumerate() {
            let content = &text[range.clone()];
            let base = TextRun {
                len: content.len(),
                font: style.font(),
                color: colors.text,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let runs = match &marked {
                Some(m) if m.start >= range.start && m.end <= range.end && !m.is_empty() => {
                    let (a, b) = (m.start - range.start, m.end - range.start);
                    [
                        TextRun {
                            len: a,
                            ..base.clone()
                        },
                        TextRun {
                            len: b - a,
                            underline: Some(UnderlineStyle {
                                color: Some(colors.text),
                                thickness: px(1.0),
                                wavy: false,
                            }),
                            ..base.clone()
                        },
                        TextRun {
                            len: content.len() - b,
                            ..base
                        },
                    ]
                    .into_iter()
                    .filter(|r| r.len > 0)
                    .collect()
                }
                _ => vec![base],
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(content.to_string()),
                font_size,
                &runs,
                None,
            );
            let top = bounds.top() + line_height * ix as f32;
            // Selection on this line, extending past the end when it covers the newline.
            let sel_start = selection.start.max(range.start);
            let sel_end = selection.end.min(range.end);
            if !selection.is_empty()
                && sel_start <= sel_end
                && selection.start <= range.end
                && selection.end >= range.start
            {
                let x0 = shaped.x_for_index(sel_start - range.start);
                let mut x1 = shaped.x_for_index(sel_end - range.start);
                if selection.end > range.end {
                    x1 += px(6.0);
                }
                if x1 > x0 {
                    selections.push(fill(
                        Bounds::from_corners(
                            point(bounds.left() + x0, top),
                            point(bounds.left() + x1, top + line_height),
                        ),
                        colors.selection,
                    ));
                }
            }
            lines.push((range.start, shaped));
        }

        let cursor_offset = buffer.cursor();
        let cursor_line = buffer.line_of(cursor_offset);
        let cursor = selection.is_empty().then(|| {
            let (start, layout) = &lines[cursor_line];
            let x = layout.x_for_index(cursor_offset - start);
            fill(
                Bounds::new(
                    point(
                        bounds.left() + x,
                        bounds.top() + line_height * cursor_line as f32,
                    ),
                    size(px(1.5), line_height),
                ),
                colors.cursor,
            )
        });

        // Keep the cursor's line visible in the scroll container.
        let viewport = editor.scroll.bounds();
        let offset = editor.scroll.offset();
        let cursor_top = line_height * cursor_line as f32;
        let visible_top = -offset.y;
        if viewport.size.height > px(0.0) {
            if cursor_top < visible_top {
                editor.scroll.set_offset(point(offset.x, -cursor_top));
            } else if cursor_top + line_height > visible_top + viewport.size.height {
                editor.scroll.set_offset(point(
                    offset.x,
                    -(cursor_top + line_height - viewport.size.height),
                ));
            }
        }

        let placeholder = text.is_empty().then(|| {
            window.text_system().shape_line(
                editor.placeholder.clone(),
                font_size,
                &[TextRun {
                    len: editor.placeholder.len(),
                    font: style.font(),
                    color: colors.placeholder,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            )
        });
        Prepaint {
            lines,
            selections,
            cursor,
            placeholder,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui_kit::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
        let line_height = window.line_height();
        for quad in prepaint.selections.drain(..) {
            window.paint_quad(quad);
        }
        if let Some(placeholder) = &prepaint.placeholder {
            let _ = placeholder.paint(
                bounds.origin,
                line_height,
                gpui_kit::TextAlign::Left,
                None,
                window,
                cx,
            );
        }
        for (ix, (_, line)) in prepaint.lines.iter().enumerate() {
            let origin = point(bounds.left(), bounds.top() + line_height * ix as f32);
            let _ = line.paint(
                origin,
                line_height,
                gpui_kit::TextAlign::Left,
                None,
                window,
                cx,
            );
        }
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        let lines = std::mem::take(&mut prepaint.lines);
        self.editor.update(cx, |editor, _| {
            editor.layouts = lines;
            editor.bounds = Some(bounds);
            editor.line_height = line_height;
        });
    }
}
