//! Zap: keeps Windows awake while it runs in the system tray.

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
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, SetProcessWorkingSetSize,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetLastInputInfo, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    LASTINPUTINFO, VK_F15,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, FindWindowW, GetMessageW, GetSystemMetrics, KillTimer, MessageBoxW,
    PostQuitMessage, SetTimer, TranslateMessage, IDYES, MB_DEFBUTTON2, MB_ICONINFORMATION,
    MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_YESNO, MSG, SM_CXSMICON, WM_TIMER,
};

const APP_NAME: &str = "Zap";
/// Icon resource IDs, embedded by build.rs.
const ICON_ACTIVE: u16 = 1;
const ICON_PAUSED: u16 = 2;
/// Settings live under HKEY_CURRENT_USER\Software\Zap.
const SETTINGS_KEY: &str = "Software\\Zap";
const KEEP_DISPLAY_ON_VALUE: &str = "KeepDisplayOn";
const KEEP_TEAMS_ACTIVE_VALUE: &str = "KeepTeamsActive";
/// The user's last Start with Windows choice. Its absence marks a first run.
const START_WITH_WINDOWS_VALUE: &str = "StartWithWindows";
/// Teams shows you as Away after a few minutes without keyboard or mouse input.
/// Zap presses F15 once input has been idle this long, and again every time
/// another interval passes without input.
const TEAMS_NUDGE_INTERVAL_MS: u32 = 99 * 1000;
/// How long Zap waits after startup or the last click before trimming its memory.
const TRIM_DELAY_MS: u32 = 5 * 1000;
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
    let display = load_setting(KEEP_DISPLAY_ON_VALUE).unwrap_or(true);
    let teams = load_setting(KEEP_TEAMS_ACTIVE_VALUE).unwrap_or(true);
    // Start with Windows defaults to on, applied once on the first run. After
    // that the Run entry is left alone, so unticking it (here or in Task
    // Manager) sticks, and test copies run from elsewhere don't take it over.
    if load_setting(START_WITH_WINDOWS_VALUE).is_none() {
        set_start_with_windows(true);
        save_setting(START_WITH_WINDOWS_VALUE, true);
    }
    let pause = MenuItem::new("Pause", true, None);
    let keep_display_on = CheckMenuItem::new("Keep display on", true, display, None);
    let keep_teams_active = CheckMenuItem::new("Keep Teams active", true, teams, None);
    let start_with_windows =
        CheckMenuItem::new("Start with Windows", true, starts_with_windows(), None);
    let about = MenuItem::new("About Zap", true, None);
    let exit = MenuItem::new("Exit", true, None);

    let menu = Menu::new();
    menu.append_items(&[
        &pause,
        &keep_display_on,
        &keep_teams_active,
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
        keep_teams_active,
        paused: false,
        icon_size,
        teams_timer: 0,
        trim_timer: 0,
    };
    zap.apply();
    zap.schedule_trim();

    // tray-icon needs a Win32 message loop on this thread. Tray and menu events
    // are queued while a message is dispatched, so drain them after each one.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        // Our thread timers have no window; tray-icon's timers belong to its window.
        if msg.message == WM_TIMER && msg.hwnd.is_null() {
            if msg.wParam == zap.teams_timer {
                if zap.teams_tick() {
                    zap.schedule_trim();
                }
            } else if msg.wParam == zap.trim_timer {
                zap.trim_memory();
            }
            continue;
        }

        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let mut had_events = false;
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            had_events = true;
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
            had_events = true;
            if event.id == *zap.pause.id() {
                zap.toggle_pause();
            } else if event.id == *zap.keep_display_on.id() {
                save_setting(KEEP_DISPLAY_ON_VALUE, zap.keep_display_on.is_checked());
                zap.apply();
            } else if event.id == *zap.keep_teams_active.id() {
                save_setting(KEEP_TEAMS_ACTIVE_VALUE, zap.keep_teams_active.is_checked());
                zap.apply();
            } else if event.id == *start_with_windows.id() {
                set_start_with_windows(start_with_windows.is_checked());
                // Show what actually took effect, in case the registry write failed.
                start_with_windows.set_checked(starts_with_windows());
                save_setting(START_WITH_WINDOWS_VALUE, start_with_windows.is_checked());
            } else if event.id == *about.id() {
                show_about();
            } else if event.id == *exit.id() && (zap.paused || confirm_exit()) {
                unsafe { PostQuitMessage(0) };
            }
        }

        if had_events {
            zap.schedule_trim();
        }
    }

    // Hand sleep control back to Windows. This also happens when the process exits.
    unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
}

struct Zap {
    tray: TrayIcon,
    pause: MenuItem,
    keep_display_on: CheckMenuItem,
    keep_teams_active: CheckMenuItem,
    paused: bool,
    icon_size: u32,
    /// Thread timer ID for the Teams idle check, or 0 when it isn't running.
    teams_timer: usize,
    /// One-shot thread timer ID for the next memory trim, or 0 when none is due.
    trim_timer: usize,
}

impl Zap {
    /// Trims memory once Zap has been quiet for TRIM_DELAY_MS. Calling it again
    /// before then restarts the countdown, so a burst of clicks trims once.
    fn schedule_trim(&mut self) {
        self.trim_timer = unsafe { SetTimer(null_mut(), self.trim_timer, TRIM_DELAY_MS, None) };
    }

    /// Lets Windows move Zap's idle pages out of RAM. They page back in
    /// within microseconds when next needed. This is what keeps Task Manager's
    /// "Memory" figure for Zap well under 1 MB.
    fn trim_memory(&mut self) {
        unsafe {
            KillTimer(null_mut(), self.trim_timer);
            SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
        }
        self.trim_timer = 0;
    }

    /// Runs on each Teams timer tick: nudges if input has been idle for the
    /// whole interval, then re-arms the timer so the next tick lands exactly
    /// when the idle time next reaches it. Returns true if it nudged.
    fn teams_tick(&mut self) -> bool {
        let idle = idle_ms();
        let due = idle >= TEAMS_NUDGE_INTERVAL_MS;
        let nudged = due && nudge();
        let delay = if due {
            TEAMS_NUDGE_INTERVAL_MS
        } else {
            TEAMS_NUDGE_INTERVAL_MS - idle
        };
        // Passing the existing ID resets that timer instead of creating another.
        self.teams_timer = unsafe { SetTimer(null_mut(), self.teams_timer, delay, None) };
        nudged
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        self.apply();
    }

    /// Brings the power request, Teams timer, icon, tooltip and menu in line with
    /// the current state.
    fn apply(&mut self) {
        if self.paused {
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        } else {
            set_awake(self.keep_display_on.is_checked());
        }

        // The timer only runs while it's needed, so Zap otherwise never wakes up.
        let want_timer = !self.paused && self.keep_teams_active.is_checked();
        if want_timer && self.teams_timer == 0 {
            self.teams_tick();
        } else if !want_timer && self.teams_timer != 0 {
            unsafe { KillTimer(null_mut(), self.teams_timer) };
            self.teams_timer = 0;
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

/// Milliseconds since the last keyboard or mouse input. If Windows can't say,
/// reports a full interval so a nudge is sent rather than skipped.
fn idle_ms() -> u32 {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return TEAMS_NUDGE_INTERVAL_MS;
    }
    unsafe { GetTickCount() }.wrapping_sub(info.dwTime)
}

/// Presses and releases F15, which resets the idle time Teams watches. Almost no
/// keyboard has F15 and nothing uses it, so it doesn't type or trigger anything.
/// Returns true if the press was sent.
fn nudge() -> bool {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_F15,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [key(0), key(KEYEVENTF_KEYUP)];
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    sent > 0
}

/// Returns a saved on/off setting, or None if it has never been set.
fn load_setting(name: &str) -> Option<bool> {
    let key = wide(SETTINGS_KEY);
    let value = wide(name);
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

/// Saves an on/off setting, creating the key if needed. Failure is ignored:
/// the setting just won't be remembered next time.
fn save_setting(name: &str, on: bool) {
    let key = wide(SETTINGS_KEY);
    let value = wide(name);
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
        "Quit Zap?\n\nYour PC will be able to sleep again.",
        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
    ) == IDYES
}

fn show_about() {
    message_box(
        &format!(
            "{APP_NAME} v{}\n\
             \u{a9} 2026 Scott Buglar\n\n\
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
