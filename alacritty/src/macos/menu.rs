//! Custom macOS menu entries.

use std::path::PathBuf;
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem, NSModalResponseOK, NSOpenPanel};
use objc2_foundation::{NSObjectProtocol, ns_string};
use winit::event_loop::EventLoopProxy;

use crate::cli::WindowOptions;
use crate::event::{Event, EventType};

static PROXY: OnceLock<EventLoopProxy<Event>> = OnceLock::new();
static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Forward an event to the main event loop.
fn send(event: EventType) {
    if let Some(proxy) = PROXY.get() {
        let _ = proxy.send_event(Event::new(event, None));
    }
}

/// Open the configuration file with `micro` in a new tab, falling back to `vi`.
///
/// Apps started from Finder don't inherit the shell environment, so `micro` is looked up through
/// a login shell. The file is created when missing.
fn open_config(tabbing_id: Option<String>) {
    const SCRIPT: &str = r#"if command -v micro >/dev/null 2>&1; then exec micro "$1"; else exec vi "$1"; fi"#;

    let Some(path) = CONFIG_PATH.get() else { return };

    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(err) = std::fs::write(path, "") {
            log::error!("Unable to create config file {path:?}: {err}");
            return;
        }
    }

    let shell = std::env::var("SHELL").unwrap_or_else(|_| String::from("/bin/zsh"));

    let mut options = WindowOptions::default();
    options.window_tabbing_id = tabbing_id;
    options.terminal_options.working_directory = path.parent().map(PathBuf::from);
    options.terminal_options.command = vec![
        shell,
        String::from("-lic"),
        String::from(SCRIPT),
        String::from("sh"),
        path.to_string_lossy().into_owned(),
    ];
    send(EventType::CreateWindow(options));
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AlacrittyMenuTarget"]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        #[unsafe(method(toggleExplorer:))]
        fn toggle_explorer_action(&self, _sender: Option<&AnyObject>) {
            send(EventType::ToggleExplorer(super::tabs::key_window_id()));
        }

        #[unsafe(method(openConfig:))]
        fn open_config_action(&self, _sender: Option<&AnyObject>) {
            // Open as a tab of the focused window.
            let tabbing_id = NSApplication::sharedApplication(self.mtm())
                .keyWindow()
                .map(|window| window.tabbingIdentifier().to_string());
            open_config(tabbing_id);
        }

        #[unsafe(method(openDirectory:))]
        fn open_directory(&self, _sender: Option<&AnyObject>) {
            let mtm = self.mtm();

            // Open as a tab of the focused window, must be read before the modal panel steals focus.
            let tabbing_id = NSApplication::sharedApplication(mtm)
                .keyWindow()
                .map(|window| window.tabbingIdentifier().to_string());

            let panel = NSOpenPanel::openPanel(mtm);
            panel.setCanChooseDirectories(true);
            panel.setCanChooseFiles(false);
            panel.setAllowsMultipleSelection(false);
            panel.setCanCreateDirectories(true);
            panel.setMessage(Some(ns_string!("Select a directory to open in a new terminal")));

            if panel.runModal() != NSModalResponseOK {
                return;
            }

            let Some(path) = panel.URL().and_then(|url| url.path()) else { return };

            let mut options = WindowOptions::default();
            options.window_tabbing_id = tabbing_id;
            options.terminal_options.working_directory = Some(path.to_string().into());
            send(EventType::CreateWindow(options));
        }
    }
);

/// Add `Open Directory…` and `Open Config…` to the `File` menu.
///
/// Must be called once the application finished launching, since winit builds its default menu
/// at that point.
pub fn install(proxy: EventLoopProxy<Event>, config_path: PathBuf) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    if PROXY.set(proxy).is_err() {
        return;
    }
    let _ = CONFIG_PATH.set(config_path);

    let app = NSApplication::sharedApplication(mtm);
    let Some(main_menu) = app.mainMenu() else { return };

    // Find the `File` submenu, or create it if winit's default menu doesn't have one.
    let file_menu = (0..main_menu.numberOfItems())
        .filter_map(|i| main_menu.itemAtIndex(i))
        .find(|item| item.title().to_string() == "File")
        .and_then(|item| item.submenu())
        .unwrap_or_else(|| {
            let menu = NSMenu::initWithTitle(mtm.alloc(), ns_string!("File"));
            let item = NSMenuItem::new(mtm);
            item.setSubmenu(Some(&menu));
            main_menu.insertItem_atIndex(&item, 1.min(main_menu.numberOfItems()));
            menu
        });

    let target: Retained<MenuTarget> = unsafe { msg_send![MenuTarget::alloc(mtm), init] };
    // No key equivalent for the explorer, Cmd+B is a regular key binding.
    let items = [
        (ns_string!("Open Directory…"), sel!(openDirectory:), ns_string!("O")),
        (ns_string!("Toggle File Explorer"), sel!(toggleExplorer:), ns_string!("")),
        (ns_string!("Open Config\u{2026}"), sel!(openConfig:), ns_string!(",")),
    ];
    for (title, action, key) in items {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), title, Some(action), key)
        };
        unsafe { item.setTarget(Some(&target)) };
        file_menu.addItem(&item);
    }

    // Menu items hold their target weakly, keep it alive for the process lifetime.
    std::mem::forget(target);
}
