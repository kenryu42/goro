//! A filterable list overlay: the repository switcher (`cmd-p`) and the turn timeline (`t`).

use gpui_kit::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding,
    SharedString, Subscription, Window, WindowAppearance, actions, div, prelude::*, px,
};

use crate::text_editor::{self, EditorColors, TextEditor};
use crate::theme::Theme;

actions!(picker, [Up, Down, Dismiss]);

pub const CONTEXT: &str = "Picker";
const MAX_VISIBLE: usize = 14;

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("ctrl-p", Up, c),
        KeyBinding::new("ctrl-n", Down, c),
        KeyBinding::new("escape", Dismiss, c),
    ]);
}

#[derive(Debug, Clone, Default)]
pub struct PickerItem {
    pub title: String,
    pub detail: String,
    /// A short highlighted tag (e.g. "agent", "running").
    pub tag: Option<String>,
    /// Muted text at the end (e.g. "3 changes").
    pub note: Option<String>,
}

pub enum PickerEvent {
    /// Index into the items.
    Pick(usize),
    /// Enter with no matching item; the typed text.
    Typed(String),
    Dismiss,
}

pub struct Picker {
    query: Entity<TextEditor>,
    items: Vec<PickerItem>,
    selected: usize,
    /// Forced light/dark from the owning view; `None` follows the OS.
    appearance: Option<WindowAppearance>,
    _query_changed: Subscription,
}

impl EventEmitter<PickerEvent> for Picker {}

impl Picker {
    pub fn new(
        placeholder: &str,
        items: Vec<PickerItem>,
        appearance: Option<WindowAppearance>,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = placeholder.to_string();
        let query = cx.new(|cx| TextEditor::new(placeholder, cx).single_line());
        let _query_changed = cx.observe(&query, |this, _, cx| {
            this.selected = 0;
            cx.notify();
        });
        Self {
            query,
            items,
            selected: 0,
            appearance,
            _query_changed,
        }
    }

    pub fn set_items(&mut self, items: Vec<PickerItem>, cx: &mut Context<Self>) {
        self.items = items;
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        cx.notify();
    }

    /// Indices of the items matching the query, in order.
    fn visible(&self, cx: &App) -> Vec<usize> {
        let query = self.query.read(cx).text().to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, i)| {
                matches_query(&query, &format!("{} {}", i.title, i.detail).to_lowercase())
            })
            .map(|(ix, _)| ix)
            .take(MAX_VISIBLE)
            .collect()
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.selected = self.selected.saturating_sub(1);
        cx.notify();
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        let count = self.visible(cx).len();
        self.selected = (self.selected + 1).min(count.saturating_sub(1));
        cx.notify();
    }

    fn confirm(&mut self, _: &text_editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        match self.visible(cx).get(self.selected) {
            Some(&ix) => cx.emit(PickerEvent::Pick(ix)),
            None => {
                let typed = self.query.read(cx).text().trim().to_string();
                cx.emit(PickerEvent::Typed(typed));
            }
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PickerEvent::Dismiss);
    }
}

/// Case-insensitive subsequence match (`gor` matches `…/goro`).
fn matches_query(query: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    query
        .chars()
        .filter(|c| !c.is_whitespace())
        .all(|q| chars.any(|h| h == q))
}

impl Focusable for Picker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for Picker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(self.appearance.unwrap_or_else(|| window.appearance()));
        let colors = EditorColors {
            text: theme.fg,
            placeholder: theme.muted,
            cursor: theme.accent,
            selection: theme.selection_bg,
        };
        self.query.update(cx, |editor, _| editor.set_colors(colors));
        let visible = self.visible(cx);
        let selected = self.selected.min(visible.len().saturating_sub(1));
        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .w(px(700.0))
            .flex()
            .flex_col()
            .bg(theme.sidebar_bg)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_lg()
            .overflow_hidden()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(self.query.clone()),
            )
            .when(visible.is_empty(), |el| {
                el.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_color(theme.muted)
                        .child("Nothing matches."),
                )
            })
            .children(visible.into_iter().enumerate().map(|(row, ix)| {
                let item = self.items[ix].clone();
                div()
                    .id(("picker-item", ix))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .when(row == selected, |el| el.bg(theme.active_row_bg))
                    .hover(|s| s.bg(theme.active_row_bg))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(PickerEvent::Pick(ix))))
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::BOLD)
                            .whitespace_nowrap()
                            .child(item.title),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(theme.muted)
                            .child(SharedString::from(item.detail)),
                    )
                    .when_some(item.tag, |el, tag| {
                        el.child(div().flex_none().text_color(theme.new_marker).child(tag))
                    })
                    .when_some(item.note, |el, note| {
                        el.child(div().flex_none().text_color(theme.muted).child(note))
                    })
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::matches_query;

    #[test]
    fn subsequence_matching() {
        assert!(matches_query("gor", "/users/me/goro"));
        assert!(matches_query("me goro", "/users/me/goro"));
        assert!(matches_query("", "/anything"));
        assert!(!matches_query("zz", "/users/me/goro"));
    }
}
