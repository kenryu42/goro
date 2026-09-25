//! The quick switcher: recent repositories and the ones agents worked in lately,
//! filtered as you type.

use std::path::PathBuf;

use goro_core::detect::{AgentLogs, agent_activity};
use goro_core::repo::Repo;
use goro_core::store::RecentRepo;
use gpui_kit::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding,
    SharedString, Subscription, Window, WindowAppearance, actions, div, prelude::*, px,
};

use crate::text_editor::{self, EditorColors, TextEditor};
use crate::theme::Theme;

actions!(switcher, [Up, Down, Dismiss]);

pub const CONTEXT: &str = "Switcher";
const MAX_VISIBLE: usize = 12;

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

pub enum SwitcherEvent {
    Open(PathBuf),
    Dismiss,
}

#[derive(Clone)]
struct Item {
    root: PathBuf,
    name: String,
    /// From agent session logs rather than Goro's own history.
    from_agent: bool,
    /// Changed files, once counted.
    changes: Option<usize>,
}

pub struct Switcher {
    query: Entity<TextEditor>,
    items: Vec<Item>,
    current: Option<PathBuf>,
    selected: usize,
    /// Forced light/dark from the owning view; `None` follows the OS.
    appearance: Option<WindowAppearance>,
    _query_changed: Subscription,
}

impl EventEmitter<SwitcherEvent> for Switcher {}

impl Switcher {
    pub fn new(
        recent: Vec<RecentRepo>,
        current: Option<PathBuf>,
        appearance: Option<WindowAppearance>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| TextEditor::new("Open repository…", cx).single_line());
        let _query_changed = cx.observe(&query, |this, _, cx| {
            this.selected = 0;
            cx.notify();
        });
        let items: Vec<Item> = recent
            .into_iter()
            .filter(|r| r.root.is_dir())
            .map(|r| item(r.root, false))
            .collect();

        // Agent activity and change counts are gathered off the UI thread.
        let known: Vec<PathBuf> = items.iter().map(|i| i.root.clone()).collect();
        let task = cx.background_executor().spawn(async move {
            let mut found: Vec<PathBuf> = Vec::new();
            for activity in agent_activity(&AgentLogs::default_locations()) {
                let Some(dir) = activity.cwd.ancestors().find(|d| d.is_dir()) else {
                    continue;
                };
                if let Ok(repo) = Repo::discover(dir) {
                    let root = repo.root().to_path_buf();
                    if !found.contains(&root) {
                        found.push(root);
                    }
                }
            }
            let mut all = found.clone();
            all.extend(known.into_iter().filter(|k| !found.contains(k)));
            let counts: Vec<(PathBuf, Option<usize>)> = all
                .into_iter()
                .map(|root| {
                    let count = Repo::discover(&root)
                        .and_then(|r| r.status())
                        .ok()
                        .map(|c| c.len());
                    (root, count)
                })
                .collect();
            (found, counts)
        });
        cx.spawn(async move |this, cx| {
            let (agent_roots, counts) = task.await;
            let _ = this.update(cx, |this, cx| {
                // Agent-active repositories first, most recent activity first.
                for root in agent_roots.into_iter().rev() {
                    this.items.retain(|i| i.root != root);
                    this.items.insert(0, item(root, true));
                }
                for (root, count) in counts {
                    if let Some(i) = this.items.iter_mut().find(|i| i.root == root) {
                        i.changes = count;
                    }
                }
                cx.notify();
            });
        })
        .detach();

        Self {
            query,
            items,
            current,
            selected: 0,
            appearance,
            _query_changed,
        }
    }

    fn visible(&self, cx: &App) -> Vec<Item> {
        let query = self.query.read(cx).text().to_lowercase();
        self.items
            .iter()
            .filter(|i| matches_query(&query, &i.root.to_string_lossy().to_lowercase()))
            .take(MAX_VISIBLE)
            .cloned()
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
        let visible = self.visible(cx);
        if let Some(item) = visible.get(self.selected) {
            cx.emit(SwitcherEvent::Open(item.root.clone()));
        } else {
            // A typed path that isn't in the list.
            let typed = PathBuf::from(self.query.read(cx).text().trim());
            if typed.is_dir() {
                cx.emit(SwitcherEvent::Open(typed));
            }
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SwitcherEvent::Dismiss);
    }
}

fn item(root: PathBuf, from_agent: bool) -> Item {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    Item {
        root,
        name,
        from_agent,
        changes: None,
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

impl Focusable for Switcher {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for Switcher {
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
        let empty = visible.is_empty();
        let selected = self.selected.min(visible.len().saturating_sub(1));
        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .w(px(620.0))
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
            .when(empty, |el| {
                el.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_color(theme.muted)
                        .child("No matching repositories. Type a path and press enter."),
                )
            })
            .children(visible.into_iter().enumerate().map(|(ix, item)| {
                let is_current = self.current.as_ref() == Some(&item.root);
                let root = item.root.clone();
                div()
                    .id(("switcher-item", ix))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .when(ix == selected, |el| el.bg(theme.active_row_bg))
                    .hover(|s| s.bg(theme.active_row_bg))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(SwitcherEvent::Open(root.clone()));
                    }))
                    .child(div().font_weight(FontWeight::BOLD).child(item.name))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(theme.muted)
                            .child(SharedString::from(item.root.to_string_lossy().into_owned())),
                    )
                    .when(item.from_agent, |el| {
                        el.child(div().text_color(theme.new_marker).child("agent"))
                    })
                    .when(is_current, |el| {
                        el.child(div().text_color(theme.muted).child("open"))
                    })
                    .when_some(item.changes, |el, n| {
                        el.child(div().text_color(theme.muted).child(match n {
                            0 => "clean".to_string(),
                            1 => "1 change".to_string(),
                            n => format!("{n} changes"),
                        }))
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
