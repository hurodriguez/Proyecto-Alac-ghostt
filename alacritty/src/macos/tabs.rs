//! Access to the native macOS tab group, used to render a custom tab bar.

use std::cell::Cell;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSEvent, NSMenu, NSMenuItem, NSTextField,
    NSView, NSWindow, NSWindowOrderingMode,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, ns_string};
use winit::raw_window_handle::RawWindowHandle;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabItem {
    pub title: String,
    pub active: bool,
}

fn ns_window(handle: &RawWindowHandle) -> Option<Retained<NSWindow>> {
    let RawWindowHandle::AppKit(handle) = handle else { return None };
    let view: Option<Retained<NSView>> =
        unsafe { Retained::retain(handle.ns_view.as_ptr().cast()) };
    view?.window()
}

/// Address of the native window, used as a stable identifier while it lives.
fn id_of(window: &NSWindow) -> usize {
    window as *const NSWindow as usize
}

/// Windows in the group of `window`, a window without siblings is its own single tab.
fn tabbed(window: &Retained<NSWindow>) -> Vec<Retained<NSWindow>> {
    match window.tabbedWindows() {
        Some(windows) => (0..windows.count()).map(|i| windows.objectAtIndex(i)).collect(),
        None => vec![window.clone()],
    }
}

/// Tabs sharing a group with the window, empty without tabbing.
///
/// Also hides the native tab bar, since the custom one replaces it.
pub fn snapshot(handle: &RawWindowHandle) -> Vec<TabItem> {
    let Some(window) = ns_window(handle) else { return Vec::new() };

    if window.tabGroup().is_some_and(|group| group.isTabBarVisible()) {
        window.toggleTabBar(None);
    }
    hide_native_tab_bar(&window);

    tabbed(&window)
        .into_iter()
        .map(|tab| TabItem { title: tab.title().to_string(), active: tab == window })
        .collect()
}

/// Hide the native tab bar, which the custom one replaces.
///
/// `toggleTabBar` is a no-op on some macOS versions, so the view is hidden directly.
fn hide_native_tab_bar(window: &NSWindow) {
    fn hide(view: &NSView) {
        if view.class().name().to_string_lossy() == "NSTabBar" && !view.isHidden() {
            view.setHidden(true);
        }

        let subviews = view.subviews();
        for i in 0..subviews.count() {
            hide(&subviews.objectAtIndex(i));
        }
    }

    if let Some(root) = unsafe { window.contentView().and_then(|view| view.superview()) } {
        hide(&root);
    }
}

/// Select the tab at `index` in the window's tab group.
pub fn select(handle: &RawWindowHandle, index: usize) {
    let Some(window) = ns_window(handle) else { return };
    let (Some(tab), Some(group)) = (tabbed(&window).get(index).cloned(), window.tabGroup()) else {
        return;
    };
    group.setSelectedWindow(Some(&tab));
}

/// Add the window to the tab group of the current key window.
///
/// Windows with a transparent title bar never get an automatic tab group, so AppKit will not tab
/// them on its own. The window must not be visible yet, otherwise it is the key window itself.
pub fn attach_to_key_window(handle: &RawWindowHandle) {
    let Some(window) = ns_window(handle) else { return };
    let Some(mtm) = MainThreadMarker::new() else { return };
    let Some(parent) = NSApplication::sharedApplication(mtm).keyWindow() else { return };

    if parent == window {
        return;
    }

    parent.addTabbedWindow_ordered(&window, NSWindowOrderingMode::Above)
}

/// Identifier of the native window behind `handle`, stable while the window lives.
pub fn window_id(handle: &RawWindowHandle) -> Option<usize> {
    ns_window(handle).map(|window| id_of(&window))
}

/// Identifier of the window that currently has the keyboard focus.
pub fn key_window_id() -> Option<usize> {
    let mtm = MainThreadMarker::new()?;
    NSApplication::sharedApplication(mtm).keyWindow().map(|window| id_of(&window))
}

/// Identifier of the native window of the tab at `index`.
pub fn tab_id(handle: &RawWindowHandle, index: usize) -> Option<usize> {
    tabbed(&ns_window(handle)?).get(index).map(|tab| id_of(tab))
}

/// Time within which two clicks on the same tab count as a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

thread_local! {
    /// Last click on a tab, shared by all windows since the second click lands on another one.
    static LAST_CLICK: Cell<Option<(Instant, usize)>> = const { Cell::new(None) };
}

/// Register a click on the tab at `index`, returns `true` when it completes a double click.
pub fn is_double_click(index: usize) -> bool {
    let now = Instant::now();
    LAST_CLICK.with(|last| {
        let double = matches!(last.get(), Some((at, i)) if i == index && now - at < DOUBLE_CLICK);
        last.set((!double).then_some((now, index)));
        double
    })
}

/// Ask for a tab title, `None` when cancelled. Blocks until the dialog is closed.
pub fn prompt_title(current: &str) -> Option<String> {
    let mtm = MainThreadMarker::new()?;

    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Rename tab"));
    alert.setInformativeText(&NSString::from_str("Leave empty to use the terminal's title."));
    alert.addButtonWithTitle(&NSString::from_str("Rename"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));

    let field = NSTextField::textFieldWithString(&NSString::from_str(current), mtm);
    field.setFrame(NSRect::new(NSPoint::new(0., 0.), NSSize::new(260., 24.)));
    alert.setAccessoryView(Some(&field));
    alert.window().setInitialFirstResponder(Some(&field));

    (alert.runModal() == NSAlertFirstButtonReturn)
        .then(|| field.stringValue().to_string().trim().to_owned())
}

/// Entry picked in the tab context menu.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TabMenuChoice {
    Rename,
    Close,
}

thread_local! {
    static MENU_CHOICE: Cell<Option<TabMenuChoice>> = const { Cell::new(None) };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AlacrittyTabMenuTarget"]
    struct TabMenuTarget;

    unsafe impl NSObjectProtocol for TabMenuTarget {}

    impl TabMenuTarget {
        #[unsafe(method(renameTab:))]
        fn rename_tab(&self, _sender: Option<&AnyObject>) {
            MENU_CHOICE.with(|choice| choice.set(Some(TabMenuChoice::Rename)));
        }

        #[unsafe(method(closeTab:))]
        fn close_tab(&self, _sender: Option<&AnyObject>) {
            MENU_CHOICE.with(|choice| choice.set(Some(TabMenuChoice::Close)));
        }
    }
);

/// Show the tab context menu at the mouse position, blocks until it is dismissed.
pub fn context_menu() -> Option<TabMenuChoice> {
    let mtm = MainThreadMarker::new()?;
    MENU_CHOICE.with(|choice| choice.set(None));

    let target: Retained<TabMenuTarget> = unsafe { msg_send![TabMenuTarget::alloc(mtm), init] };
    let menu = NSMenu::new(mtm);
    let items = [
        (ns_string!("Rename Tab\u{2026}"), sel!(renameTab:)),
        (ns_string!("Close Tab"), sel!(closeTab:)),
    ];
    for (title, action) in items {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), title, Some(action), ns_string!(""))
        };
        unsafe { item.setTarget(Some(&target)) };
        menu.addItem(&item);
    }

    // The menu only holds its targets weakly, `target` lives until the menu is dismissed.
    menu.popUpMenuPositioningItem_atLocation_inView(None, NSEvent::mouseLocation(), None);
    MENU_CHOICE.with(Cell::take)
}
