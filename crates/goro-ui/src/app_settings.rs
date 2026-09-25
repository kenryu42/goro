//! Applying user settings: theme, font, layout, keybindings and the global hotkey,
//! reloaded live when the settings file changes.

use std::path::PathBuf;
use std::str::FromStr;

use futures::StreamExt;
use futures::channel::mpsc::unbounded;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use goro_core::settings::{self, Settings};
use gpui_kit::{
    App, DummyKeyboardMapper, Global, KeyBinding, KeyBindingContextPredicate, NoAction,
};

use crate::{DIFF_CONTEXT, bind_keys};

pub struct AppSettings {
    pub settings: Settings,
    pub path: Option<PathBuf>,
    /// A problem with the settings file, shown in every window.
    pub error: Option<String>,
    /// Keeps the settings file watch alive.
    _watcher: Option<notify::RecommendedWatcher>,
    /// The global hotkey registration (main thread only, like the app itself).
    hotkey: Option<Hotkey>,
}

impl Global for AppSettings {}

pub fn settings(cx: &App) -> Settings {
    cx.try_global::<AppSettings>()
        .map(|s| s.settings.clone())
        .unwrap_or_default()
}

pub fn settings_error(cx: &App) -> Option<String> {
    cx.try_global::<AppSettings>().and_then(|s| s.error.clone())
}

/// Load settings, apply them, and reload whenever the file changes.
pub fn init(cx: &mut App) {
    let path = settings::settings_path();
    let (tx, mut rx) = unbounded::<()>();
    let watcher = path.as_ref().and_then(|path| {
        use notify::Watcher;
        let dir = path.parent()?.to_path_buf();
        std::fs::create_dir_all(&dir).ok()?;
        let file = path.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                // Only writes: on Linux, reading the file on reload is itself an event.
                if event.is_ok_and(|e| {
                    goro_core::watch::is_change(&e.kind)
                        && e.paths.iter().any(|p| p.file_name() == file.file_name())
                }) {
                    let _ = tx.unbounded_send(());
                }
            })
            .ok()?;
        watcher
            .watch(&dir, notify::RecursiveMode::NonRecursive)
            .ok()?;
        Some(watcher)
    });
    cx.set_global(AppSettings {
        settings: Settings::default(),
        path,
        error: None,
        _watcher: watcher,
        hotkey: None,
    });
    reload(cx);
    cx.spawn(async move |cx| {
        while rx.next().await.is_some() {
            while rx.try_recv().is_ok() {}
            cx.update(|cx| {
                reload(cx);
                cx.refresh_windows();
            });
        }
    })
    .detach();
}

fn reload(cx: &mut App) {
    let (settings, mut error) = match cx.global::<AppSettings>().path.clone() {
        Some(path) => settings::load(&path),
        None => (Settings::default(), None),
    };
    // Keybindings: the defaults, then the user's overrides.
    cx.clear_key_bindings();
    bind_keys(cx);
    let mut bindings = Vec::new();
    for (keys, action) in &settings.keybindings {
        let action = if action.is_empty() {
            Ok(Box::new(NoAction) as Box<dyn gpui_kit::Action>)
        } else {
            cx.build_action(action, None).map_err(|e| e.to_string())
        };
        let context = KeyBindingContextPredicate::parse(DIFF_CONTEXT)
            .ok()
            .map(Into::into);
        match action.and_then(|action| {
            KeyBinding::load(keys, action, context, false, None, &DummyKeyboardMapper)
                .map_err(|e| e.to_string())
        }) {
            Ok(binding) => bindings.push(binding),
            Err(err) => error = Some(format!("keybinding \"{keys}\": {err}")),
        }
    }
    cx.bind_keys(bindings);
    let app_settings = cx.global_mut::<AppSettings>();
    if let Err(err) = register_hotkey(&mut app_settings.hotkey, settings.global_hotkey.as_deref()) {
        error = Some(err);
    }
    app_settings.settings = settings;
    app_settings.error = error;
}

/// The hotkey manager (created on the main thread, kept for the life of the app) and the
/// key it holds.
struct Hotkey {
    manager: GlobalHotKeyManager,
    registered: Option<HotKey>,
}

fn register_hotkey(slot: &mut Option<Hotkey>, spec: Option<&str>) -> Result<(), String> {
    if slot.is_none() {
        if spec.is_none() {
            return Ok(());
        }
        let manager =
            GlobalHotKeyManager::new().map_err(|e| format!("global hotkey unavailable ({e})"))?;
        *slot = Some(Hotkey {
            manager,
            registered: None,
        });
    }
    let hotkey = slot.as_mut().expect("created above");
    let wanted = match spec {
        Some(spec) => {
            Some(HotKey::from_str(spec).map_err(|e| format!("global_hotkey \"{spec}\": {e}"))?)
        }
        None => None,
    };
    if hotkey.registered == wanted {
        return Ok(());
    }
    if let Some(old) = hotkey.registered.take() {
        let _ = hotkey.manager.unregister(old);
    }
    if let Some(new) = wanted {
        hotkey
            .manager
            .register(new)
            .map_err(|e| format!("global_hotkey \"{}\": {e}", spec.unwrap_or_default()))?;
        hotkey.registered = Some(new);
    }
    Ok(())
}

/// Call `on_press` (on the main thread) whenever the global hotkey is pressed.
pub fn on_hotkey(cx: &mut App, on_press: impl Fn(&mut App) + 'static) {
    let (tx, mut rx) = unbounded::<()>();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state == HotKeyState::Pressed {
            let _ = tx.unbounded_send(());
        }
    }));
    cx.spawn(async move |cx| {
        while rx.next().await.is_some() {
            cx.update(|cx| on_press(cx));
        }
    })
    .detach();
}

/// Open the settings file in the user's editor, creating it with the defaults first.
pub fn open_settings_file(cx: &App) -> Result<(), String> {
    let path = cx
        .try_global::<AppSettings>()
        .and_then(|s| s.path.clone())
        .ok_or("no config directory")?;
    settings::write_default_if_missing(&path).map_err(|e| e.to_string())?;
    let mut command = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg("-t");
        c
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    command
        .arg(&path)
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}
