//! Zap: keeps Windows awake while it runs in the system tray.
//!
//! Rust rewrite of Caffeine, https://github.com/kyleleong/caffeine (Object Pascal / Lazarus).

#![windows_subsystem = "windows"]

use std::ptr::null_mut;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, GetSystemMetrics, MessageBoxW, PostQuitMessage,
    TranslateMessage, IDYES, MB_DEFBUTTON2, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNO,
    MSG, SM_CXSMICON, SM_CYSMICON,
};

const APP_NAME: &str = "Zap";
/// Resource ID of assets/zap.ico, embedded by build.rs via zap.rc.
const ICON_RESOURCE_ID: u16 = 1;

fn main() {
    // Without this Windows reports 16px icon sizes on high-DPI screens and then
    // stretches the tray icon, which blurs it.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    // Equivalent of the original's UniqueInstance component.
    if !acquire_single_instance() {
        message_box(
            &format!("{APP_NAME} is already running.\n\nLook for its icon in the system tray."),
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    }

    let keep_display_on = CheckMenuItem::new("Keep display on", true, false, None);
    let about = MenuItem::new("About Zap", true, None);
    let exit = MenuItem::new("Exit", true, None);

    let menu = Menu::new();
    menu.append_items(&[
        &keep_display_on,
        &PredefinedMenuItem::separator(),
        &about,
        &exit,
    ])
    .expect("failed to build tray menu");

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(true)
        .with_tooltip(tooltip(false))
        .with_icon(load_icon())
        .build()
        .expect("failed to create tray icon");

    set_awake(false);

    // tray-icon needs a Win32 message loop on this thread. Menu events are queued
    // while a message is dispatched, so drain them after each one.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == *keep_display_on.id() {
                let display = keep_display_on.is_checked();
                set_awake(display);
                let _ = tray.set_tooltip(Some(tooltip(display)));
            } else if event.id == *about.id() {
                show_about();
            } else if event.id == *exit.id() && confirm_exit() {
                unsafe { PostQuitMessage(0) };
            }
        }
    }

    // Hand sleep control back to Windows. This also happens when the process exits.
    unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
}

/// Tells Windows the system (and optionally the display) is in use until further notice.
fn set_awake(keep_display_on: bool) {
    let mut flags = ES_CONTINUOUS | ES_SYSTEM_REQUIRED;
    if keep_display_on {
        flags |= ES_DISPLAY_REQUIRED;
    }
    unsafe { SetThreadExecutionState(flags) };
}

fn tooltip(keep_display_on: bool) -> String {
    if keep_display_on {
        format!("{APP_NAME}: keeping your PC and display awake")
    } else {
        format!("{APP_NAME}: keeping your PC awake")
    }
}

/// Loads the embedded icon at the tray's size so Windows picks the hand-rendered
/// image for that size rather than shrinking a large one.
fn load_icon() -> Icon {
    let size = unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
    Icon::from_resource(ICON_RESOURCE_ID, Some((size.0 as u32, size.1 as u32)))
        .expect("embedded icon is missing")
}

/// Returns false if another copy of Zap already holds the named mutex.
/// The handle is deliberately leaked so the mutex lives as long as the process.
fn acquire_single_instance() -> bool {
    let name = wide("Local\\Zap-SingleInstance");
    let handle = unsafe { CreateMutexW(null_mut(), 0, name.as_ptr()) };
    !handle.is_null() && unsafe { GetLastError() } != ERROR_ALREADY_EXISTS
}

fn confirm_exit() -> bool {
    message_box(
        "Are you sure you want to quit?\n\nThis will allow your computer to sleep.",
        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
    ) == IDYES
}

fn show_about() {
    message_box(
        &format!(
            "{APP_NAME} v{}\n\
             Based on Caffeine by Kyle Leong (\u{a9} 2020):\n\
             https://github.com/kyleleong/caffeine\n\n\
             Zap prevents your computer from going to sleep while it is running. \
             Use the tray icon to keep the display on too, or to quit.",
            env!("CARGO_PKG_VERSION")
        ),
        MB_OK | MB_ICONINFORMATION,
    );
}

fn message_box(text: &str, style: u32) -> i32 {
    let text = wide(text);
    let caption = wide(APP_NAME);
    unsafe { MessageBoxW(null_mut(), text.as_ptr(), caption.as_ptr(), style) }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
