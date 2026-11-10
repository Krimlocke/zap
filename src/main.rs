//! Zap: keeps Windows awake while it runs in the system tray.

#![windows_subsystem = "windows"]

use std::cell::RefCell;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tray_icon::menu::{
    CheckMenuItem, ContextMenu, Menu, MenuEvent, MenuItem, MenuItemKind, PredefinedMenuItem,
};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::InvalidateRect;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, REG_SZ,
    RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, GetCurrentThreadId, SetProcessWorkingSetSize,
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
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW, GetMenuItemID,
    GetMenuState, GetMessageW, GetSystemMetrics, KillTimer, MenuItemFromPoint, MessageBoxW,
    PostQuitMessage, RegisterClassW, SendMessageW, SetTimer, SetWindowsHookExW, TranslateMessage,
    HMENU, IDYES, MB_DEFBUTTON2, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND,
    MB_YESNO, MF_BYPOSITION, MF_DISABLED, MF_GRAYED, MSG, MSGF_MENU, SM_CXSMICON, WH_MSGFILTER,
    WM_COMMAND, WM_LBUTTONUP, WM_TIMER, WM_WTSSESSION_CHANGE, WNDCLASSW, WTS_SESSION_LOCK,
    WTS_SESSION_UNLOCK,
};

const APP_NAME: &str = "Zap";
/// Icon resource IDs, embedded by build.rs.
const ICON_ACTIVE: u16 = 1;
const ICON_PAUSED: u16 = 2;
/// Settings live under HKEY_CURRENT_USER\Software\Zap.
const SETTINGS_KEY: &str = "Software\\Zap";
const KEEP_DISPLAY_ON_VALUE: &str = "KeepDisplayOn";
const KEEP_TEAMS_ACTIVE_VALUE: &str = "KeepTeamsActive";
const RIK_MODE_VALUE: &str = "RikMode";
/// The user's last Start with Windows choice. Its absence marks a first run.
const START_WITH_WINDOWS_VALUE: &str = "StartWithWindows";
/// Teams shows you as Away after a few minutes without keyboard or mouse input.
/// Zap presses F15 once input has been idle this long, and again every time
/// another interval passes without input.
const TEAMS_NUDGE_INTERVAL_MS: u32 = 99 * 1000;
/// Rik Mode: how long Zap keeps Teams active between breaks, picked at random
/// each time so breaks don't land at the same point every hour.
const RIK_GAP_MIN: Duration = Duration::from_secs(45 * 60);
const RIK_GAP_MAX: Duration = Duration::from_secs(75 * 60);
/// Rik Mode: how long each break lasts, counted from the last input. Teams waits
/// 5 minutes before going Away, so you show as Away for roughly 5 to 15 minutes.
const RIK_BREAK_MIN: Duration = Duration::from_secs(10 * 60);
const RIK_BREAK_MAX: Duration = Duration::from_secs(20 * 60);
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
    let rik = load_setting(RIK_MODE_VALUE).unwrap_or(false);
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
    let rik_mode = CheckMenuItem::new("Rik Mode", teams, rik, None);
    let start_with_windows =
        CheckMenuItem::new("Start with Windows", true, starts_with_windows(), None);
    let about = MenuItem::new("About Zap", true, None);
    let exit = MenuItem::new("Exit", true, None);

    let menu = Menu::new();
    menu.append_items(&[
        &pause,
        &keep_display_on,
        &keep_teams_active,
        &rik_mode,
        &start_with_windows,
        &PredefinedMenuItem::separator(),
        &about,
        &exit,
    ])
    .expect("failed to build tray menu");

    // Which menu positions hold tick boxes, read before the menu moves into the
    // tray icon. The native menu has one entry per item, separators included.
    let menu_handle = menu.hpopupmenu();
    let tick_positions = menu
        .items()
        .iter()
        .map(|item| matches!(item, MenuItemKind::Check(_)))
        .collect();

    // Left click toggles pause; right click opens the menu.
    let icon_size = tray_icon_size();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_icon(load_icon(ICON_ACTIVE, icon_size))
        .build()
        .expect("failed to create tray icon");

    let zap = Zap {
        tray,
        pause,
        keep_display_on,
        keep_teams_active,
        rik_mode,
        start_with_windows,
        about,
        exit,
        menu_handle,
        tick_positions,
        paused: false,
        icon_size,
        teams_timer: 0,
        rik: None,
        trim_timer: 0,
        locked: false,
    };
    ZAP.with_borrow_mut(|cell| {
        let zap = cell.insert(zap);
        zap.apply();
        zap.schedule_trim();
    });
    watch_session_lock();

    // Sees the open tray menu's clicks so tick boxes can toggle without closing it.
    // Only this thread's menus and dialogs pass through it.
    unsafe { SetWindowsHookExW(WH_MSGFILTER, Some(menu_filter), null_mut(), GetCurrentThreadId()) };

    // tray-icon needs a Win32 message loop on this thread. Tray and menu events
    // are queued while a message is dispatched, so drain them after each one.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        // Our thread timers have no window; tray-icon's timers belong to its window.
        // Dispatching a windowless timer message is a no-op, and falling through
        // still drains any tray click queued while a dialog was open.
        if msg.message == WM_TIMER && msg.hwnd.is_null() {
            with_zap(|zap| zap.on_timer(msg.wParam));
        }

        // Zap isn't borrowed here, so the menu hook can reach it while the
        // menu is open inside this dispatch.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        with_zap(Zap::handle_events);
    }

    // Hand sleep control back to Windows. This also happens when the process exits.
    unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
}

thread_local! {
    /// The app's state, kept here so the menu hook can reach it.
    static ZAP: RefCell<Option<Zap>> = const { RefCell::new(None) };
}

/// Whether the session is locked, as Windows last reported it. Kept apart from
/// Zap so a report that arrives while Zap is busy (a message box is open) isn't
/// lost; Zap catches up after the next message it handles.
static LOCKED: AtomicBool = AtomicBool::new(false);

/// Creates a hidden window that Windows tells when the session locks or unlocks.
fn watch_session_lock() {
    let class = wide("ZapSession");
    unsafe {
        let instance = GetModuleHandleW(null());
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(session_proc);
        wc.hInstance = instance;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        // A plain hidden top-level window, not a message-only one, which is
        // the documented way to receive session change notifications.
        let window = CreateWindowExW(
            0,
            class.as_ptr(),
            null(),
            0,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        WTSRegisterSessionNotification(window, NOTIFY_FOR_THIS_SESSION);
    }
}

unsafe extern "system" fn session_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_WTSSESSION_CHANGE {
        match wparam as u32 {
            WTS_SESSION_LOCK => LOCKED.store(true, Ordering::Relaxed),
            WTS_SESSION_UNLOCK => LOCKED.store(false, Ordering::Relaxed),
            _ => return 0,
        }
        with_zap(Zap::sync_lock);
        return 0;
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

/// Runs f on the app's state, or does nothing if it's already in use, as it is
/// while a message box Zap opened is showing.
fn with_zap<R>(f: impl FnOnce(&mut Zap) -> R) -> Option<R> {
    ZAP.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

/// Message hook for the open tray menu. A click on a tick box toggles it and is
/// then swallowed, so the menu stays open for the next change. Other items
/// close the menu as usual.
unsafe extern "system" fn menu_filter(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == MSGF_MENU as i32 {
        let msg = unsafe { &*(lparam as *const MSG) };
        if msg.message == WM_LBUTTONUP && with_zap(|zap| zap.toggle_in_menu(msg.pt)) == Some(true) {
            // Repaint the menu so ticks and greyed items show the new state.
            let class = wide("#32768");
            unsafe {
                let menu_window = FindWindowW(class.as_ptr(), std::ptr::null());
                InvalidateRect(menu_window, std::ptr::null(), 0);
            }
            return 1;
        }
    }
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

/// Rik Mode lets Teams go Away now and then, so you look like you stepped away.
#[derive(Clone, Copy)]
enum Rik {
    /// Keeping Teams active; the next break starts at the first nudge after this.
    Waiting { break_at: Instant },
    /// Not nudging until this time, then Teams is woken and the gap starts again.
    OnBreak { until: Instant },
}

struct Zap {
    tray: TrayIcon,
    pause: MenuItem,
    keep_display_on: CheckMenuItem,
    keep_teams_active: CheckMenuItem,
    rik_mode: CheckMenuItem,
    start_with_windows: CheckMenuItem,
    about: MenuItem,
    exit: MenuItem,
    /// The tray menu's native handle, and which of its positions are tick boxes.
    menu_handle: isize,
    tick_positions: Vec<bool>,
    paused: bool,
    icon_size: u32,
    /// Thread timer ID for the Teams idle check, or 0 when it isn't running.
    teams_timer: usize,
    /// Where Rik Mode is in its cycle, or None while it isn't running.
    rik: Option<Rik>,
    /// One-shot thread timer ID for the next memory trim, or 0 when none is due.
    trim_timer: usize,
    /// The lock state last applied. While locked Zap doesn't nudge: Teams shows
    /// you as Away on lock anyway, and each press would count as activity and
    /// stop the locked screen turning off.
    locked: bool,
}

impl Zap {
    fn on_timer(&mut self, id: usize) {
        if id == self.teams_timer {
            if self.teams_tick() {
                self.schedule_trim();
            }
        } else if id == self.trim_timer {
            self.trim_memory();
        }
    }

    /// Applies a lock or unlock that hasn't been applied yet.
    fn sync_lock(&mut self) {
        let locked = LOCKED.load(Ordering::Relaxed);
        if locked != self.locked {
            self.locked = locked;
            self.apply();
        }
    }

    /// Handles queued tray clicks and menu choices.
    fn handle_events(&mut self) {
        self.sync_lock();
        let mut had_events = false;
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            had_events = true;
            match event {
                // On press, not release: a double-click sends two releases but
                // only one press, so it would otherwise pause and resume.
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Down,
                    ..
                } => self.toggle_pause(),
                // The menu opens on release, so refresh the tick first in case
                // the entry was changed in Task Manager while Zap was running.
                TrayIconEvent::Click {
                    button: MouseButton::Right,
                    button_state: MouseButtonState::Down,
                    ..
                } => self.start_with_windows.set_checked(starts_with_windows()),
                // Display scaling may have changed since the icon was loaded.
                TrayIconEvent::Enter { .. } => self.refresh_icon_size(),
                _ => {}
            }
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            had_events = true;
            if event.id == *self.pause.id() {
                self.toggle_pause();
            } else if event.id == *self.keep_display_on.id() {
                save_setting(KEEP_DISPLAY_ON_VALUE, self.keep_display_on.is_checked());
                self.apply();
            } else if event.id == *self.keep_teams_active.id() {
                save_setting(KEEP_TEAMS_ACTIVE_VALUE, self.keep_teams_active.is_checked());
                self.apply();
            } else if event.id == *self.rik_mode.id() {
                save_setting(RIK_MODE_VALUE, self.rik_mode.is_checked());
                self.apply();
            } else if event.id == *self.start_with_windows.id() {
                set_start_with_windows(self.start_with_windows.is_checked());
                // Show what actually took effect, in case the registry write failed.
                self.start_with_windows.set_checked(starts_with_windows());
                save_setting(START_WITH_WINDOWS_VALUE, self.start_with_windows.is_checked());
            } else if event.id == *self.about.id() {
                show_about();
            } else if event.id == *self.exit.id() && (self.paused || confirm_exit()) {
                unsafe { PostQuitMessage(0) };
            }
        }

        if had_events {
            self.schedule_trim();
        }
    }

    /// Called by the menu hook for a click at pt. If it's on a tick box, toggles
    /// it and applies the change now, and returns true so the click is swallowed
    /// and the menu stays open. A greyed tick box swallows the click and does
    /// nothing.
    fn toggle_in_menu(&mut self, pt: POINT) -> bool {
        let menu = self.menu_handle as HMENU;
        let position = unsafe { MenuItemFromPoint(null_mut(), menu, pt) };
        let Ok(index) = usize::try_from(position) else {
            return false;
        };
        if !self.tick_positions.get(index).copied().unwrap_or(false) {
            return false;
        }
        let state = unsafe { GetMenuState(menu, index as u32, MF_BYPOSITION) };
        if state & (MF_GRAYED | MF_DISABLED) == 0 {
            // The same command the menu would send: the menu library flips the
            // tick and queues the event, which is handled straight away.
            let command = unsafe { GetMenuItemID(menu, position) };
            unsafe { SendMessageW(self.tray.window_handle(), WM_COMMAND, command as usize, 0) };
            self.handle_events();
        }
        true
    }

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
    /// when the idle time next reaches it. In Rik Mode it starts and ends
    /// breaks instead of nudging when they're due. Returns true if it nudged.
    fn teams_tick(&mut self) -> bool {
        let idle = idle_ms();
        let now = Instant::now();
        let due = idle >= TEAMS_NUDGE_INTERVAL_MS;

        match self.rik {
            Some(Rik::OnBreak { until }) if now < until => {
                self.arm_teams_timer(duration_ms(until - now));
                return false;
            }
            // Break over: start the next gap and fall through, so an idle PC
            // is nudged straight away and Teams comes back.
            Some(Rik::OnBreak { .. }) => self.start_rik_gap(now),
            // Only start a break once you've been away long enough for a nudge;
            // while you're using the PC a break would change nothing.
            Some(Rik::Waiting { break_at }) if due && now >= break_at => {
                // Count the break from your last input, which is when Teams'
                // own 5-minute countdown started.
                let length = random_between(RIK_BREAK_MIN, RIK_BREAK_MAX)
                    .saturating_sub(Duration::from_millis(idle.into()));
                self.rik = Some(Rik::OnBreak { until: now + length });
                self.refresh_status();
                self.arm_teams_timer(duration_ms(length));
                return false;
            }
            _ => {}
        }

        let nudged = due && nudge();
        let delay = if due {
            TEAMS_NUDGE_INTERVAL_MS
        } else {
            TEAMS_NUDGE_INTERVAL_MS - idle
        };
        self.arm_teams_timer(delay);
        nudged
    }

    /// Starts or resets the Teams timer. Passing the existing ID resets that
    /// timer instead of creating another.
    fn arm_teams_timer(&mut self, delay_ms: u32) {
        self.teams_timer = unsafe { SetTimer(null_mut(), self.teams_timer, delay_ms, None) };
    }

    fn start_rik_gap(&mut self, now: Instant) {
        let gap = random_between(RIK_GAP_MIN, RIK_GAP_MAX);
        self.rik = Some(Rik::Waiting { break_at: now + gap });
        self.refresh_status();
    }

    fn on_rik_break(&self) -> bool {
        matches!(self.rik, Some(Rik::OnBreak { .. }))
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        self.apply();
    }

    /// Brings the power request, Teams timer, Rik Mode, icon, tooltip and menu
    /// in line with the current state.
    fn apply(&mut self) {
        // The timer only runs while it's needed, so Zap otherwise never wakes up.
        // Locking stops it and ends any Rik Mode cycle (a lock is a real break);
        // unlocking starts both afresh.
        let want_timer = !self.paused && !self.locked && self.keep_teams_active.is_checked();
        let want_rik = want_timer && self.rik_mode.is_checked();
        self.rik_mode.set_enabled(self.keep_teams_active.is_checked());

        let was_on_break = self.on_rik_break();
        if !want_rik {
            self.rik = None;
        } else if self.rik.is_none() {
            self.start_rik_gap(Instant::now());
        }

        // Also re-checks straight away when Rik Mode is turned off mid-break.
        if want_timer && (self.teams_timer == 0 || was_on_break && !self.on_rik_break()) {
            self.teams_tick();
        } else if !want_timer && self.teams_timer != 0 {
            unsafe { KillTimer(null_mut(), self.teams_timer) };
            self.teams_timer = 0;
        }

        let _ = self.tray.set_icon(Some(load_icon(self.icon_id(), self.icon_size)));
        self.pause.set_text(if self.paused { "Resume" } else { "Pause" });
        self.refresh_status();
    }

    /// Updates the power request and tooltip, which change when a Rik Mode
    /// break starts or ends as well as from the menu.
    fn refresh_status(&self) {
        if self.paused {
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        } else {
            // During a break nothing presses a key, so keep the screen on to stop
            // it locking, which would block the nudge that brings Teams back.
            set_awake(self.keep_display_on.is_checked() || self.on_rik_break());
        }
        let state = if self.paused {
            "inactive"
        } else if self.on_rik_break() {
            "active, Rik Mode break"
        } else {
            "active"
        };
        let _ = self.tray.set_tooltip(Some(format!("{APP_NAME}: {state}")));
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

/// A random duration from min to max, using the randomly seeded keys std gives
/// each new hasher, so Zap needs no extra dependency.
fn random_between(min: Duration, max: Duration) -> Duration {
    let span = (max - min).as_millis() as u64;
    let random = RandomState::new().hash_one(0u8);
    min + Duration::from_millis(random % (span + 1))
}

/// A duration as timer milliseconds, at least 1.
fn duration_ms(d: Duration) -> u32 {
    d.as_millis().clamp(1, u32::MAX.into()) as u32
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
