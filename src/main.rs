//! Zap: keeps Windows awake while it runs in the system tray.
//!
//! Rust rewrite of Caffeine, https://github.com/kyleleong/caffeine (Object Pascal / Lazarus).

#![windows_subsystem = "windows"]

use std::ptr::null_mut;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS};
use windows_sys::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, REG_SZ,
    RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, FindWindowW, GetMessageW, GetSystemMetrics, MessageBoxW, PostQuitMessage,
    TranslateMessage, IDYES, MB_DEFBUTTON2, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK,
    MB_SETFOREGROUND, MB_YESNO, MSG, SM_CXSMICON,
};

const APP_NAME: &str = "Zap";
/// Icon resource IDs, embedded by build.rs.
const ICON_ACTIVE: u16 = 1;
const ICON_PAUSED: u16 = 2;
/// Settings live under HKEY_CURRENT_USER\Software\Zap.
const SETTINGS_KEY: &str = "Software\\Zap";
const KEEP_DISPLAY_ON_VALUE: &str = "KeepDisplayOn";
/// Per-user autostart entry, the same one Task Manager's Startup apps lists.
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
/// Where Task Manager records an autostart entry the user has disabled.
const STARTUP_APPROVED_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";
const RUN_VALUE: &str = "Zap";

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

    // On by default: on Modern Standby laptops a dark screen looks like sleep.
    let display = load_keep_display_on().unwrap_or(true);
    let pause = MenuItem::new("Pause", true, None);
    let keep_display_on = CheckMenuItem::new("Keep display on", true, display, None);
    let start_with_windows =
        CheckMenuItem::new("Start with Windows", true, starts_with_windows(), None);
    let about = MenuItem::new("About Zap", true, None);
    let exit = MenuItem::new("Exit", true, None);

    let menu = Menu::new();
    menu.append_items(&[
        &pause,
        &keep_display_on,
        &start_with_windows,
        &PredefinedMenuItem::separator(),
        &about,
        &exit,
    ])
    .expect("failed to build tray menu");

    // Left click toggles pause; right click opens the menu.
    let icon_size = tray_icon_size();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_icon(load_icon(ICON_ACTIVE, icon_size))
        .build()
        .expect("failed to create tray icon");

    let mut zap = Zap {
        tray,
        pause,
        keep_display_on,
        paused: false,
        icon_size,
    };
    zap.apply();

    // tray-icon needs a Win32 message loop on this thread. Tray and menu events
    // are queued while a message is dispatched, so drain them after each one.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } => zap.toggle_pause(),
                // Display scaling may have changed since the icon was loaded.
                TrayIconEvent::Enter { .. } => zap.refresh_icon_size(),
                _ => {}
            }
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == *zap.pause.id() {
                zap.toggle_pause();
            } else if event.id == *zap.keep_display_on.id() {
                save_keep_display_on(zap.keep_display_on.is_checked());
                zap.apply();
            } else if event.id == *start_with_windows.id() {
                set_start_with_windows(start_with_windows.is_checked());
                // Show what actually took effect, in case the registry write failed.
                start_with_windows.set_checked(starts_with_windows());
            } else if event.id == *about.id() {
                show_about();
            } else if event.id == *exit.id() && (zap.paused || confirm_exit()) {
                unsafe { PostQuitMessage(0) };
            }
        }
    }

    // Hand sleep control back to Windows. This also happens when the process exits.
    unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
}

struct Zap {
    tray: TrayIcon,
    pause: MenuItem,
    keep_display_on: CheckMenuItem,
    paused: bool,
    icon_size: u32,
}

impl Zap {
    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        self.apply();
    }

    /// Brings the power request, icon, tooltip and menu in line with the current state.
    fn apply(&self) {
        if self.paused {
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        } else {
            set_awake(self.keep_display_on.is_checked());
        }

        let _ = self.tray.set_icon(Some(load_icon(self.icon_id(), self.icon_size)));
        let state = if self.paused { "inactive" } else { "active" };
        let _ = self.tray.set_tooltip(Some(format!("{APP_NAME}: {state}")));
        self.pause.set_text(if self.paused { "Resume" } else { "Pause" });
    }

    fn refresh_icon_size(&mut self) {
        let size = tray_icon_size();
        if size != self.icon_size {
            self.icon_size = size;
            let _ = self.tray.set_icon(Some(load_icon(self.icon_id(), size)));
        }
    }

    fn icon_id(&self) -> u16 {
        if self.paused {
            ICON_PAUSED
        } else {
            ICON_ACTIVE
        }
    }
}

/// Tells Windows the system (and optionally the display) is in use until further notice.
fn set_awake(keep_display_on: bool) {
    let mut flags = ES_CONTINUOUS | ES_SYSTEM_REQUIRED;
    if keep_display_on {
        flags |= ES_DISPLAY_REQUIRED;
    }
    unsafe { SetThreadExecutionState(flags) };
}

/// Returns the saved "Keep display on" choice, or None if it has never been set.
fn load_keep_display_on() -> Option<bool> {
    let key = wide(SETTINGS_KEY);
    let value = wide(KEEP_DISPLAY_ON_VALUE);
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (status == ERROR_SUCCESS).then_some(data != 0)
}

/// Saves the "Keep display on" choice, creating the key if needed. Failure is
/// ignored: the setting just won't be remembered next time.
fn save_keep_display_on(on: bool) {
    let key = wide(SETTINGS_KEY);
    let value = wide(KEEP_DISPLAY_ON_VALUE);
    let data = u32::from(on);
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            REG_DWORD,
            (&data as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
}

/// The command Windows runs at sign-in: this exe's full path, quoted.
fn run_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\"", exe.display()))
}

/// True if Zap is set to start at sign-in, from this exe, and the entry hasn't
/// been disabled in Task Manager. An entry pointing at another copy of Zap
/// (e.g. after moving the exe) counts as off, so ticking the menu item repoints it.
fn starts_with_windows() -> bool {
    let (Some(command), Some(registered)) = (run_command(), read_reg_string(RUN_KEY, RUN_VALUE))
    else {
        return false;
    };
    if !registered.eq_ignore_ascii_case(&command) {
        return false;
    }

    // Task Manager stores a 12-byte value whose first byte is odd when disabled.
    let key = wide(STARTUP_APPROVED_KEY);
    let value = wide(RUN_VALUE);
    let mut data = [0u8; 12];
    let mut size = data.len() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_BINARY,
            null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    !(status == ERROR_SUCCESS && data[0] & 1 == 1)
}

/// Adds or removes the sign-in entry. Turning it on also clears any "disabled"
/// mark Task Manager left, so the menu and Task Manager agree.
fn set_start_with_windows(on: bool) {
    let run_key = wide(RUN_KEY);
    let approved_key = wide(STARTUP_APPROVED_KEY);
    let value = wide(RUN_VALUE);
    unsafe {
        RegDeleteKeyValueW(HKEY_CURRENT_USER, approved_key.as_ptr(), value.as_ptr());
        if !on {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, run_key.as_ptr(), value.as_ptr());
            return;
        }
    }
    let Some(command) = run_command() else { return };
    let data = wide(&command);
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            run_key.as_ptr(),
            value.as_ptr(),
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
}

/// Reads a REG_SZ value from HKEY_CURRENT_USER.
fn read_reg_string(key: &str, value: &str) -> Option<String> {
    let key = wide(key);
    let value = wide(value);
    let mut size = 0u32;
    let read = |buffer: *mut u16, size: &mut u32| unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buffer.cast(),
            size,
        )
    };
    if read(null_mut(), &mut size) != ERROR_SUCCESS {
        return None;
    }
    let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
    if read(buffer.as_mut_ptr(), &mut size) != ERROR_SUCCESS {
        return None;
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

/// The small-icon size for the taskbar's monitor, which can differ from the
/// primary monitor and changes when the user changes display scaling.
fn tray_icon_size() -> u32 {
    let class = wide("Shell_TrayWnd");
    let size = unsafe {
        let taskbar = FindWindowW(class.as_ptr(), std::ptr::null());
        let dpi = if taskbar.is_null() { 0 } else { GetDpiForWindow(taskbar) };
        if dpi == 0 {
            GetSystemMetrics(SM_CXSMICON)
        } else {
            GetSystemMetricsForDpi(SM_CXSMICON, dpi)
        }
    };
    size.max(16) as u32
}

/// Loads an embedded icon at the given size so Windows picks the hand-rendered
/// image for that size rather than shrinking a large one.
fn load_icon(resource_id: u16, size: u32) -> Icon {
    Icon::from_resource(resource_id, Some((size, size))).expect("embedded icon is missing")
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
             Click the tray icon to pause or resume; right-click it for options.",
            env!("CARGO_PKG_VERSION")
        ),
        MB_OK | MB_ICONINFORMATION,
    );
}

/// Shows a message box. MB_SETFOREGROUND stops it opening behind other windows,
/// since a tray app has no window of its own to own the dialog.
fn message_box(text: &str, style: u32) -> i32 {
    let text = wide(text);
    let caption = wide(APP_NAME);
    unsafe {
        MessageBoxW(
            null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            style | MB_SETFOREGROUND,
        )
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
