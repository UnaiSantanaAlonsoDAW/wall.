//! Windows owns global input, the tray and composition. This thread sleeps in
//! GetMessage; it only wakes egui for actual user actions.
pub use crate::core::Hotkey;
use crate::core::TrayAppearance;
use eframe::egui;
use std::{
    cell::RefCell,
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};
use windows::{
    UI::ViewManagement::UISettings,
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*, OpenGL::*},
        System::{
            Com::*,
            LibraryLoader::GetModuleHandleW,
            Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
            Threading::{CreateMutexW, GetCurrentProcessId},
            WinRT::*,
        },
        UI::{
            Controls::MARGINS, HiDpi::*, Input::KeyboardAndMouse::*, Shell::*,
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, PWSTR, w},
};

#[derive(Debug)]
pub enum PlatformEvent {
    Toggle(Option<(i32, i32)>),
    Show,
    Import,
    Settings,
    Quit,
    Error(String),
    OutsideClick { explorer: bool },
    MotionChanged(bool),
}

const SHOW: u32 = WM_APP + 1;
const COMMAND: u32 = WM_APP + 2;
const TRAY: u32 = WM_APP + 3;
const MOUSE_DOWN: u32 = WM_APP + 4;
const VISIBILITY_INTENT: PCWSTR = w!("Wall.OverlayVisibilityIntent");

// Window properties keep this synchronous on the UI thread, independently of
// the native thread's queued mouse-hook commands. Values are plain tags, not
// allocated pointers or kernel handles: 1 = hidden, 2 = visible.
fn set_visibility_intent(window: HWND, visible: bool) -> Result<(), String> {
    unsafe {
        SetPropW(
            window,
            VISIBILITY_INTENT,
            Some(HANDLE(if visible { 2usize } else { 1usize } as *mut _)),
        )
    }
    .map_err(|e| format!("Cannot update overlay visibility: {e}"))
}

unsafe fn visibility_blocked(window: HWND) -> bool {
    unsafe { GetPropW(window, VISIBILITY_INTENT).0 as usize == 1 }
}

/// Request a graceful exit through the existing instance's own tray command.
pub fn quit_existing() -> Result<bool, String> {
    unsafe {
        match FindWindowW(w!("Wall.NativeEvents"), PCWSTR::null()) {
            Ok(window) => {
                PostMessageW(Some(window), WM_COMMAND, WPARAM(4), LPARAM(0))
                    .map_err(|e| e.to_string())?;
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }
}

fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut _)
}
fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

pub struct Instance(HANDLE);
impl Instance {
    pub fn acquire(reveal_existing: bool) -> Result<Option<Self>, String> {
        // The session-local mutex is not held with ownership: the handle's
        // lifetime, including crash cleanup, provides single-instance semantics.
        unsafe {
            let mutex = CreateMutexW(None, false, w!("Local\\Wall.WallpaperSwitcher.v1"))
                .map_err(|e| e.to_string())?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                #[cfg(debug_assertions)]
                eprintln!("[native] existing instance");
                if reveal_existing
                    && let Ok(window) = FindWindowW(w!("Wall.NativeEvents"), PCWSTR::null())
                {
                    #[cfg(debug_assertions)]
                    eprintln!("[native] notifying existing event window {:?}", window);
                    let mut process = 0;
                    GetWindowThreadProcessId(window, Some(&mut process));
                    let _ = AllowSetForegroundWindow(process);
                    let _ = PostMessageW(Some(window), SHOW, WPARAM(0), LPARAM(0));
                }
                let _ = CloseHandle(mutex);
                Ok(None)
            } else {
                #[cfg(debug_assertions)]
                eprintln!("[native] acquired instance");
                Ok(Some(Self(mutex)))
            }
        }
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

enum Command {
    Hotkey(Hotkey, mpsc::SyncSender<Result<(), String>>),
    Overlay { hwnd: isize, visible: bool },
    TrayAppearance(TrayAppearance),
}
pub struct Platform {
    window: isize,
    commands: mpsc::Sender<Command>,
    thread: Option<thread::JoinHandle<()>>,
}
struct State {
    context: egui::Context,
    sender: mpsc::Sender<PlatformEvent>,
    commands: mpsc::Receiver<Command>,
    icon: NOTIFYICONDATAW,
    tray_appearance: TrayAppearance,
    tray_style: TrayStyle,
    hotkey: Hotkey,
    hotkey_id: i32,
    taskbar_created: u32,
    overlay: isize,
    mouse_hook: Option<HHOOK>,
}
thread_local! { static STATE: RefCell<Option<State>> = const { RefCell::new(None) }; }

impl State {
    fn emit(&self, event: PlatformEvent) {
        #[cfg(debug_assertions)]
        eprintln!("[native] event {event:?}");
        let _ = self.sender.send(event);
        self.context.request_repaint();
    }
    fn change_hotkey(&mut self, new: Hotkey) -> Result<(), String> {
        if new == self.hotkey && self.hotkey_id != 0 {
            return Ok(());
        }
        validate_hotkey(new)?;
        let id = if self.hotkey_id == 1 { 2 } else { 1 };
        unsafe { RegisterHotKey(Some(self.icon.hWnd), id, modifiers(new), new.key) }
            .map_err(|e| format!("Shortcut is already in use or unavailable: {e}"))?;
        if self.hotkey_id != 0 {
            unsafe {
                let _ = UnregisterHotKey(Some(self.icon.hWnd), self.hotkey_id);
            }
        }
        self.hotkey = new;
        self.hotkey_id = id;
        Ok(())
    }
    fn refresh_tray(&mut self, recover: bool) -> Result<(), String> {
        let style = TrayStyle::current(self.tray_appearance);
        let changed = style != self.tray_style;
        if !changed && !recover {
            return Ok(());
        }
        let mut next = self.icon;
        if changed {
            next.hIcon = style
                .load()
                .map_err(|e| format!("Cannot load tray symbol: {e}"))?;
        }
        unsafe {
            // Modify the existing identity; register it again only after Explorer
            // lost it. A DPI-triggered TaskbarCreated can leave it present.
            let updated = Shell_NotifyIconW(NIM_MODIFY, &next).as_bool()
                || (recover && Shell_NotifyIconW(NIM_ADD, &next).as_bool());
            if !updated {
                if changed {
                    let _ = DestroyIcon(next.hIcon);
                }
                return Err("Windows could not update the tray symbol.".into());
            }
            if changed {
                let _ = DestroyIcon(self.icon.hIcon);
                self.icon.hIcon = next.hIcon;
                self.tray_style = style;
            }
        }
        Ok(())
    }
    fn set_overlay(&mut self, raw: isize, visible: bool) {
        self.overlay = if visible { raw } else { 0 };
        unsafe {
            if !visible {
                if let Some(hook) = self.mouse_hook.take() {
                    let _ = UnhookWindowsHookEx(hook);
                }
            } else if self.mouse_hook.is_none() {
                let module = GetModuleHandleW(PCWSTR::null())
                    .ok()
                    .map(|module| HINSTANCE(module.0));
                match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0) {
                    Ok(hook) => self.mouse_hook = Some(hook),
                    Err(error) => self.emit(PlatformEvent::Error(format!("Outside-click dismissal is unavailable; Escape still closes wall. ({error})"))),
                }
            }
        }
    }
    fn outside_click(&self, point: POINT) {
        unsafe {
            let window = hwnd(self.overlay);
            if self.overlay == 0 || !IsWindowVisible(window).as_bool() {
                return;
            }
            let mut bounds = RECT::default();
            if GetWindowRect(window, &mut bounds).is_err() || point_inside(point, bounds) {
                return;
            }
            let target = WindowFromPoint(point);
            if target.0.is_null() {
                return;
            }
            let mut process = 0;
            GetWindowThreadProcessId(target, Some(&mut process));
            // Own preferences/popups/pickers are never outside interactions.
            if process == GetCurrentProcessId() {
                return;
            }
            let root = GetAncestor(target, GA_ROOT);
            let owner = GetAncestor(target, GA_ROOTOWNER);
            let explorer = [target, root, owner].into_iter().any(|window| {
                let mut name = [0u16; 256];
                let count = GetClassNameW(window, &mut name).max(0) as usize;
                is_import_surface(&String::from_utf16_lossy(&name[..count]))
            });
            self.emit(PlatformEvent::OutsideClick { explorer });
        }
    }
}

fn point_inside(point: POINT, bounds: RECT) -> bool {
    point.x >= bounds.left
        && point.x < bounds.right
        && point.y >= bounds.top
        && point.y < bounds.bottom
}

fn is_import_surface(class: &str) -> bool {
    matches!(
        class,
        "CabinetWClass" | "ExploreWClass" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0
        && matches!(
            wparam.0 as u32,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        )
        && lparam.0 != 0
    {
        // This hook is NOT injected into another process and never consumes an
        // input. Post two coordinates, then return immediately. Classification
        // and egui wake-up happen in the ordinary native message handler.
        let point = unsafe { (*(lparam.0 as *const MSLLHOOKSTRUCT)).pt };
        STATE.with(|cell| {
            if let Ok(state) = cell.try_borrow()
                && let Some(state) = state.as_ref().filter(|state| state.overlay != 0)
            {
                unsafe {
                    let _ = PostMessageW(
                        Some(state.icon.hWnd),
                        MOUSE_DOWN,
                        WPARAM(point.x as isize as usize),
                        LPARAM(point.y as isize),
                    );
                }
            }
        });
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn validate_hotkey(key: Hotkey) -> Result<(), String> {
    if !key.ctrl && !key.alt {
        return Err("Use Ctrl or Alt with a letter, number or function key.".into());
    }
    if !(0x30..=0x39).contains(&key.key)
        && !(0x41..=0x5a).contains(&key.key)
        && !(0x70..=0x87).contains(&key.key)
    {
        return Err("Choose a letter, number or F1–F24.".into());
    }
    Ok(())
}
fn modifiers(key: Hotkey) -> HOT_KEY_MODIFIERS {
    let mut flags = MOD_NOREPEAT;
    if key.ctrl {
        flags |= MOD_CONTROL;
    }
    if key.alt {
        flags |= MOD_ALT;
    }
    if key.shift {
        flags |= MOD_SHIFT;
    }
    flags
}

impl Platform {
    pub fn start(
        context: egui::Context,
        sender: mpsc::Sender<PlatformEvent>,
        hotkey: Hotkey,
        tray_appearance: TrayAppearance,
    ) -> Result<Self, String> {
        let (commands, receiver) = mpsc::channel();
        let (ready, initialized) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("wall-native-events".into())
            .spawn(move || {
                let result = unsafe {
                    create_event_window(context, sender, receiver, hotkey, tray_appearance)
                };
                match result {
                    Ok(window) => {
                        let _ = ready.send(Ok(window.0 as isize));
                        let mut message = MSG::default();
                        loop {
                            let result = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
                            if result <= 0 {
                                #[cfg(debug_assertions)]
                                eprintln!("[native] message loop ended: {result}");
                                break;
                            }
                            unsafe {
                                let _ = TranslateMessage(&message);
                                DispatchMessageW(&message);
                            }
                        }
                        STATE.with(|cell| {
                            if let Some(state) = cell.borrow_mut().take() {
                                unsafe {
                                    if let Some(hook) = state.mouse_hook {
                                        let _ = UnhookWindowsHookEx(hook);
                                    }
                                    #[cfg(debug_assertions)]
                                    eprintln!("[native] removing tray icon");
                                    let _ = Shell_NotifyIconW(NIM_DELETE, &state.icon);
                                    #[cfg(debug_assertions)]
                                    eprintln!("[native] releasing hotkey/icon");
                                    if state.hotkey_id != 0 {
                                        let _ = UnregisterHotKey(Some(window), state.hotkey_id);
                                    }
                                    let _ = DestroyIcon(state.icon.hIcon);
                                }
                            }
                        });
                        #[cfg(debug_assertions)]
                        eprintln!("[native] thread finished");
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let window = initialized.recv().map_err(|e| e.to_string())??;
        Ok(Self {
            window,
            commands,
            thread: Some(thread),
        })
    }
    pub fn set_hotkey(&self, hotkey: Hotkey) -> Result<(), String> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Hotkey(hotkey, sender))
            .map_err(|e| e.to_string())?;
        unsafe { PostMessageW(Some(hwnd(self.window)), COMMAND, WPARAM(0), LPARAM(0)) }
            .map_err(|e| e.to_string())?;
        receiver
            .recv_timeout(Duration::from_secs(3))
            .map_err(|e| e.to_string())?
    }
    pub fn set_overlay(&self, raw: isize, visible: bool) -> Result<(), String> {
        set_visibility_intent(hwnd(raw), visible)?;
        self.commands
            .send(Command::Overlay { hwnd: raw, visible })
            .map_err(|e| e.to_string())?;
        unsafe { PostMessageW(Some(hwnd(self.window)), COMMAND, WPARAM(0), LPARAM(0)) }
            .map_err(|e| e.to_string())
    }
    pub fn set_tray_appearance(&self, appearance: TrayAppearance) -> Result<(), String> {
        self.commands
            .send(Command::TrayAppearance(appearance))
            .map_err(|e| e.to_string())?;
        unsafe { PostMessageW(Some(hwnd(self.window)), COMMAND, WPARAM(0), LPARAM(0)) }
            .map_err(|e| e.to_string())
    }
}
impl Drop for Platform {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        eprintln!("[native] drop: posting close");
        unsafe {
            let result = PostMessageW(Some(hwnd(self.window)), WM_CLOSE, WPARAM(0), LPARAM(0));
            #[cfg(debug_assertions)]
            eprintln!("[native] close posted: {result:?}");
            #[cfg(not(debug_assertions))]
            let _ = result;
        }
        if let Some(thread) = self.thread.take() {
            #[cfg(debug_assertions)]
            eprintln!("[native] joining thread");
            let _ = thread.join();
        }
        #[cfg(debug_assertions)]
        eprintln!("[native] drop complete");
    }
}

unsafe fn create_event_window(
    context: egui::Context,
    sender: mpsc::Sender<PlatformEvent>,
    commands: mpsc::Receiver<Command>,
    hotkey: Hotkey,
    tray_appearance: TrayAppearance,
) -> Result<HWND, String> {
    unsafe {
        let module = GetModuleHandleW(PCWSTR::null()).map_err(|e| e.to_string())?;
        let instance = HINSTANCE(module.0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: w!("Wall.NativeEvents"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(windows::core::Error::from_thread().to_string());
        }
        // An invisible top-level window (rather than HWND_MESSAGE) receives the
        // Explorer TaskbarCreated broadcast, so tray recovery needs no polling.
        let window = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("Wall.NativeEvents"),
            w!("wall. events"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .map_err(|e| e.to_string())?;
        let tray_style = TrayStyle::current(tray_appearance);
        let icon = tray_style
            .load()
            .map_err(|e| format!("Cannot load tray symbol: {e}"))?;
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: TRAY,
            hIcon: icon,
            ..Default::default()
        };
        let tip: Vec<u16> = "wall. — wallpapers within reach".encode_utf16().collect();
        data.szTip[..tip.len()].copy_from_slice(&tip);
        let tray_ok = Shell_NotifyIconW(NIM_ADD, &data).as_bool();
        let mut state = State {
            context,
            sender,
            commands,
            icon: data,
            tray_appearance,
            tray_style,
            hotkey,
            hotkey_id: 0,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            overlay: 0,
            mouse_hook: None,
        };
        if let Err(error) = state.change_hotkey(hotkey) {
            state.emit(PlatformEvent::Error(error));
        }
        if !tray_ok {
            state.emit(PlatformEvent::Error(
                "Windows could not add the tray icon. The shortcut remains available.".into(),
            ));
        }
        STATE.with(|cell| *cell.borrow_mut() = Some(state));
        #[cfg(debug_assertions)]
        eprintln!("[native] event window ready {:?}, tray={tray_ok}", window);
        Ok(window)
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CLOSE => {
            #[cfg(debug_assertions)]
            eprintln!("[native] received WM_CLOSE");
            unsafe {
                let result = DestroyWindow(window);
                #[cfg(debug_assertions)]
                eprintln!("[native] DestroyWindow: {result:?}");
                #[cfg(not(debug_assertions))]
                let _ = result;
            }
            return LRESULT(0);
        }
        WM_DESTROY => {
            #[cfg(debug_assertions)]
            eprintln!("[native] received WM_DESTROY");
            unsafe {
                PostQuitMessage(0);
            }
            return LRESULT(0);
        }
        _ => {}
    }
    // Do not hold RefCell across TrackPopupMenu: Windows may re-enter WndProc.
    if message == TRAY && (lparam.0 as u32 == WM_RBUTTONUP || lparam.0 as u32 == WM_CONTEXTMENU) {
        unsafe {
            tray_menu(window);
        }
        return LRESULT(0);
    }
    STATE.with(|cell| {
        // Cross-thread Win32 calls may dispatch a sent message while inside a
        // handler. Never let a reentrant callback panic across the system ABI.
        let Ok(mut borrow) = cell.try_borrow_mut() else {
            return;
        };
        if let Some(state) = borrow.as_mut() {
            match message {
                WM_HOTKEY if wparam.0 == state.hotkey_id as usize => {
                    state.emit(PlatformEvent::Toggle(physical_cursor()))
                }
                SHOW => state.emit(PlatformEvent::Show),
                TRAY if lparam.0 as u32 == WM_LBUTTONUP => {
                    state.emit(PlatformEvent::Toggle(physical_cursor()))
                }
                WM_COMMAND => match wparam.0 & 0xffff {
                    1 => state.emit(PlatformEvent::Show),
                    2 => state.emit(PlatformEvent::Import),
                    3 => state.emit(PlatformEvent::Settings),
                    4 => state.emit(PlatformEvent::Quit),
                    _ => {}
                },
                COMMAND => {
                    while let Ok(command) = state.commands.try_recv() {
                        match command {
                            Command::Hotkey(key, sender) => {
                                let _ = sender.send(state.change_hotkey(key));
                            }
                            Command::Overlay { hwnd, visible } => state.set_overlay(hwnd, visible),
                            Command::TrayAppearance(appearance) => {
                                state.tray_appearance = appearance;
                                if let Err(error) = state.refresh_tray(false) {
                                    state.emit(PlatformEvent::Error(error));
                                }
                            }
                        }
                    }
                }
                MOUSE_DOWN => state.outside_click(POINT {
                    x: wparam.0 as i32,
                    y: lparam.0 as i32,
                }),
                WM_SETTINGCHANGE => {
                    if let Err(error) = state.refresh_tray(false) {
                        diagnostics(error);
                    }
                    state.emit(PlatformEvent::MotionChanged(reduced_motion()))
                }
                WM_DWMCOMPOSITIONCHANGED => {
                    state.emit(PlatformEvent::MotionChanged(reduced_motion()))
                }
                WM_THEMECHANGED | WM_DPICHANGED | WM_DISPLAYCHANGE => {
                    if let Err(error) = state.refresh_tray(false) {
                        diagnostics(error);
                    }
                }
                _ if message == state.taskbar_created => {
                    if let Err(error) = state.refresh_tray(true) {
                        diagnostics(error);
                    }
                }
                _ => {}
            }
        }
    });
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

unsafe fn tray_menu(window: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let _ = AppendMenuW(menu, MF_STRING, 1, w!("Open wall."));
        let _ = AppendMenuW(menu, MF_STRING, 2, w!("Add images…"));
        let _ = AppendMenuW(menu, MF_STRING, 3, w!("Preferences…"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, 4, w!("Quit"));
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        let _ = SetForegroundWindow(window);
        let selected = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            point.x,
            point.y,
            None,
            window,
            None,
        )
        .0;
        if selected != 0 {
            let _ = PostMessageW(
                Some(window),
                WM_COMMAND,
                WPARAM(selected as usize),
                LPARAM(0),
            );
        }
        let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

const TRAY_CHALK: u16 = 102;
const TRAY_INK: u16 = 103;

fn tray_resource(appearance: TrayAppearance, system_light: bool) -> u16 {
    match appearance {
        TrayAppearance::Dark => TRAY_INK,
        TrayAppearance::Automatic if system_light => TRAY_INK,
        TrayAppearance::Automatic | TrayAppearance::Light => TRAY_CHALK,
    }
}

fn system_uses_light_theme() -> bool {
    // AppsUseLightTheme is independent in Custom mode and may disagree
    // with the taskbar. Read only the Windows shell's color preference.
    let mut value = 0_u32;
    let mut bytes = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut bytes),
        )
    };
    status == ERROR_SUCCESS && value == 1
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct TrayStyle {
    resource: u16,
    size: (i32, i32),
}

impl TrayStyle {
    fn current(appearance: TrayAppearance) -> Self {
        let system_light =
            matches!(appearance, TrayAppearance::Automatic) && system_uses_light_theme();
        let size = unsafe {
            let shell_dpi = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null())
                .map(|window| GetDpiForWindow(window))
                .unwrap_or(0);
            let dpi = if shell_dpi == 0 {
                GetDpiForSystem()
            } else {
                shell_dpi
            };
            (
                GetSystemMetricsForDpi(SM_CXSMICON, dpi).max(16),
                GetSystemMetricsForDpi(SM_CYSMICON, dpi).max(16),
            )
        };
        Self {
            resource: tray_resource(appearance, system_light),
            size,
        }
    }

    fn load(self) -> windows::core::Result<HICON> {
        unsafe {
            let module = GetModuleHandleW(PCWSTR::null())?;
            // Integer resource ID (Win32 MAKEINTRESOURCEW). Without LR_SHARED
            // this HICON is owned by State and released on replace/exit.
            let name = PCWSTR(usize::from(self.resource) as *const u16);
            LoadImageW(
                Some(HINSTANCE(module.0)),
                name,
                IMAGE_ICON,
                self.size.0,
                self.size.1,
                LR_DEFAULTCOLOR,
            )
            .map(|handle| HICON(handle.0))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Composition {
    /// DWM readback confirms Desktop Acrylic on an alpha-capable surface.
    /// Windows can still substitute its solid material while inactive or saving power.
    Acrylic,
    Solid(SolidReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SolidReason {
    UserDisabled,
    SystemEffectsDisabled,
    CompositionUnavailable,
    IncompatibleSurface,
    UnsupportedWindows,
    NativeApiFailure,
}

impl Composition {
    pub fn is_acrylic(self) -> bool {
        self == Self::Acrylic
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::Acrylic => "Native Acrylic · Windows may use a solid surface while inactive",
            Self::Solid(SolidReason::UserDisabled) => "Solid surface selected",
            Self::Solid(SolidReason::SystemEffectsDisabled) => {
                "Windows transparency effects are disabled"
            }
            Self::Solid(SolidReason::CompositionUnavailable) => {
                "Windows desktop composition is unavailable"
            }
            Self::Solid(SolidReason::IncompatibleSurface) => {
                "This graphics surface has no alpha channel"
            }
            Self::Solid(SolidReason::UnsupportedWindows) => {
                "Native Acrylic requires Windows 11 22H2 or later"
            }
            Self::Solid(SolidReason::NativeApiFailure) => {
                "Windows could not configure native Acrylic"
            }
        }
    }
}

fn diagnostics(message: impl std::fmt::Display) {
    if std::env::var_os("WALL_DIAGNOSTICS").is_some() {
        eprintln!("[wall composition] {message}");
    }
}

pub fn reduced_motion() -> bool {
    let mut enabled = windows::core::BOOL(1);
    unsafe {
        let result = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut enabled as *mut windows::core::BOOL).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        result.is_ok() && !enabled.as_bool()
    }
}

fn advanced_effects_enabled() -> Result<bool, String> {
    // A short, balanced WinRT initialization is compatible with winit's STA.
    unsafe { RoInitialize(RO_INIT_SINGLETHREADED) }.map_err(|e| e.to_string())?;
    let result = UISettings::new()
        .and_then(|settings| settings.AdvancedEffectsEnabled())
        .map_err(|e| e.to_string());
    unsafe {
        RoUninitialize();
    }
    result
}

unsafe fn surface_alpha_bits(window: HWND) -> Option<u8> {
    unsafe {
        let dc = GetDC(Some(window));
        if dc.0.is_null() {
            return None;
        }
        let format = GetPixelFormat(dc);
        let mut descriptor = PIXELFORMATDESCRIPTOR::default();
        let described = format > 0
            && DescribePixelFormat(
                dc,
                format,
                size_of::<PIXELFORMATDESCRIPTOR>() as u32,
                Some(&mut descriptor),
            ) > 0;
        let _ = ReleaseDC(Some(window), dc);
        described.then_some(descriptor.cAlphaBits)
    }
}

fn overlay_style(style: u32) -> u32 {
    // A DWM frame is necessary for the system material on Windows 11 23H2.
    // Keep its caption style while WM_NCCALCSIZE makes the entire area client.
    // Removing the style makes DWM report Acrylic yet paint an opaque surface.
    (style & !(WS_SYSMENU | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX).0) | WS_CAPTION.0
}

unsafe fn diagnose_composition(window: HWND, phase: &str) {
    if std::env::var_os("WALL_DIAGNOSTICS").is_none() {
        return;
    }
    unsafe {
        let mut nc = 0i32;
        let mut dark = 0i32;
        let mut backdrop = 0i32;
        let _ = DwmGetWindowAttribute(
            window,
            DWMWA_NCRENDERING_ENABLED,
            (&mut nc as *mut i32).cast(),
            4,
        );
        let _ = DwmGetWindowAttribute(
            window,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&mut dark as *mut i32).cast(),
            4,
        );
        let _ = DwmGetWindowAttribute(
            window,
            DWMWA_SYSTEMBACKDROP_TYPE,
            (&mut backdrop as *mut i32).cast(),
            4,
        );
        diagnostics(format_args!(
            "{phase}: caption+WGL-alpha, foreground={}, visible={}, nc={nc}, dark={dark}, backdrop={backdrop}, style=0x{:x}, exstyle=0x{:x}, alpha={:?}",
            GetForegroundWindow() == window,
            IsWindowVisible(window).as_bool(),
            GetWindowLongPtrW(window, GWL_STYLE),
            GetWindowLongPtrW(window, GWL_EXSTYLE),
            surface_alpha_bits(window)
        ));
    }
}

fn overlay_exstyle(style: u32) -> u32 {
    // Glow uses a redirected, alpha-capable OpenGL surface, not a layered GDI
    // bitmap. Layering also conflicts with OpenGL's own-DC window class. Keep
    // mouse hit testing normal across every transparent pixel of this overlay.
    let style = style & !(WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOREDIRECTIONBITMAP).0;
    // The UI automation enumerator excludes tool windows. A debug-only switch
    // makes the same overlay inspectable without changing the release behavior.
    #[cfg(debug_assertions)]
    if std::env::var_os("WALL_INSPECT").is_some() {
        return style & !(WS_EX_TOOLWINDOW | WS_EX_APPWINDOW).0;
    }
    (style | WS_EX_TOOLWINDOW.0) & !WS_EX_APPWINDOW.0
}

#[cfg(debug_assertions)]
thread_local! {
    static ENTER_AT: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

#[cfg(debug_assertions)]
pub fn take_enter_time() -> Option<std::time::Instant> {
    ENTER_AT.with(|at| at.take())
}

unsafe extern "system" fn overlay_subclass(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    unsafe {
        #[cfg(debug_assertions)]
        if message == WM_KEYDOWN && wparam.0 == VK_RETURN.0 as usize {
            ENTER_AT.with(|at| at.set(Some(std::time::Instant::now())));
        }
        if message == WM_STYLECHANGING && lparam.0 != 0 {
            let styles = &mut *(lparam.0 as *mut STYLESTRUCT);
            if wparam.0 as i32 == GWL_STYLE.0 {
                styles.styleNew = overlay_style(styles.styleNew);
                if visibility_blocked(window) {
                    styles.styleNew &= !WS_VISIBLE.0;
                }
            } else if wparam.0 as i32 == GWL_EXSTYLE.0 {
                styles.styleNew = overlay_exstyle(styles.styleNew);
            }
        }
        if message == WM_WINDOWPOSCHANGING && lparam.0 != 0 && visibility_blocked(window) {
            // eframe 0.37 unconditionally shows its root after first painting,
            // including a builder configured visible(false). Enforce our explicit
            // hidden intent before the native transition, without a visible flash
            // or per-frame polling. Leave geometry and Z-order flags untouched.
            let position = &mut *(lparam.0 as *mut WINDOWPOS);
            position.flags = (position.flags & !SWP_SHOWWINDOW) | SWP_HIDEWINDOW;
        }
        if message == WM_NCCALCSIZE {
            // Microsoft custom-frame pattern: retain the DWM frame style but
            // make the entire window client area; no conventional title bar.
            return LRESULT(0);
        }
        if message == WM_ACTIVATE {
            diagnose_composition(
                window,
                if wparam.0 & 0xffff == WA_INACTIVE as usize {
                    "deactivated"
                } else {
                    "activated"
                },
            );
        }
        if message == WM_NCDESTROY {
            let _ = RemovePropW(window, VISIBILITY_INTENT);
            let _ = RemoveWindowSubclass(window, Some(overlay_subclass), id);
        }
        DefSubclassProc(window, message, wparam, lparam)
    }
}

pub fn configure_window(raw: isize, glass: bool) -> Composition {
    unsafe {
        let window = hwnd(raw);
        if GetPropW(window, VISIBILITY_INTENT).0.is_null() {
            let _ = set_visibility_intent(window, false);
        }
        // Winit rebuilds native styles on visibility changes, reintroducing
        // WS_SYSMENU. Keep the overlay frameless across those transitions.
        let _ = SetWindowSubclass(window, Some(overlay_subclass), 0x57414c4c, 0);
        // DWM can expose caption buttons after extending glass over the client.
        let standard = GetWindowLongPtrW(window, GWL_STYLE);
        SetWindowLongPtrW(window, GWL_STYLE, overlay_style(standard as u32) as isize);
        let style = GetWindowLongPtrW(window, GWL_EXSTYLE);
        SetWindowLongPtrW(window, GWL_EXSTYLE, overlay_exstyle(style as u32) as isize);
        let _ = SetWindowPos(
            window,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        let dark: i32 = 1;
        let corner = DWMWCP_ROUND.0;
        let _ = DwmSetWindowAttribute(
            window,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark as *const i32).cast(),
            4,
        );
        let _ = DwmSetWindowAttribute(
            window,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&corner as *const i32).cast(),
            4,
        );
        let status = configure_composition(window, glass);
        diagnostics(format_args!(
            "status={status:?}, style=0x{:x}, exstyle=0x{:x}, alpha={:?}",
            GetWindowLongPtrW(window, GWL_STYLE),
            GetWindowLongPtrW(window, GWL_EXSTYLE),
            surface_alpha_bits(window)
        ));
        diagnose_composition(window, "configured");
        status
    }
}

unsafe fn configure_composition(window: HWND, glass: bool) -> Composition {
    unsafe {
        let set_material = |material: i32| {
            DwmSetWindowAttribute(
                window,
                DWMWA_SYSTEMBACKDROP_TYPE,
                (&material as *const i32).cast(),
                4,
            )
        };
        let solid = |reason| {
            let _ = set_material(DWMSBT_NONE.0);
            let _ = DwmExtendFrameIntoClientArea(window, &MARGINS::default());
            Composition::Solid(reason)
        };
        if !glass {
            return solid(SolidReason::UserDisabled);
        }
        if !DwmIsCompositionEnabled().is_ok_and(|enabled| enabled.as_bool()) {
            return solid(SolidReason::CompositionUnavailable);
        }
        match advanced_effects_enabled() {
            Ok(false) => return solid(SolidReason::SystemEffectsDisabled),
            Err(error) => {
                diagnostics(error);
                return solid(SolidReason::NativeApiFailure);
            }
            Ok(true) => {}
        }
        // WGL reports the actual chosen framebuffer, not a requested capability.
        // EGL fallback has no WGL pixel format; do not falsely reject that path.
        if surface_alpha_bits(window).is_some_and(|alpha| alpha == 0) {
            return solid(SolidReason::IncompatibleSurface);
        }
        let policy = DWMNCRP_ENABLED.0;
        let policy_result = DwmSetWindowAttribute(
            window,
            DWMWA_NCRENDERING_POLICY,
            (&policy as *const i32).cast(),
            4,
        );
        diagnostics(format_args!("nonclient policy={policy}: {policy_result:?}"));
        if policy_result.is_err() {
            return solid(SolidReason::NativeApiFailure);
        }
        // Preserve winit 0.30's alpha-compositing path for transparent WGL
        // windows. This handles alpha, not legacy blur (which Windows 8+ no
        // longer draws). Desktop Acrylic is requested separately below.
        let region = CreateRectRgn(0, 0, -1, -1);
        if region.0.is_null() {
            return solid(SolidReason::NativeApiFailure);
        }
        let legacy = DWM_BLURBEHIND {
            dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
            fEnable: true.into(),
            hRgnBlur: region,
            ..Default::default()
        };
        let legacy_result = DwmEnableBlurBehindWindow(window, &legacy);
        if !region.0.is_null() {
            let _ = DeleteObject(HGDIOBJ(region.0));
        }
        diagnostics(format_args!("WGL alpha path: {legacy_result:?}"));
        if legacy_result.is_err() {
            return solid(SolidReason::NativeApiFailure);
        }
        // Supported since Windows 11 24H2; not yet generated in windows 0.62.
        // Value 39 comes from Microsoft's dwmapi.h after SYSTEMBACKDROP_TYPE=38.
        // https://github.com/microsoft/win32metadata/blob/main/generation/WinSDK/RecompiledIdlHeaders/um/dwmapi.h
        // The framebuffer uses premultiplied alpha, as required by this flag.
        let alpha: i32 = 1;
        let alpha_result = DwmSetWindowAttribute(
            window,
            DWMWINDOWATTRIBUTE(39),
            (&alpha as *const i32).cast(),
            4,
        );
        diagnostics(format_args!("redirection alpha: {alpha_result:?}"));
        if DwmExtendFrameIntoClientArea(
            window,
            &MARGINS {
                cxLeftWidth: -1,
                cxRightWidth: -1,
                cyTopHeight: -1,
                cyBottomHeight: -1,
            },
        )
        .is_err()
        {
            return solid(SolidReason::NativeApiFailure);
        }
        if let Err(error) = set_material(DWMSBT_TRANSIENTWINDOW.0) {
            diagnostics(format_args!("set Desktop Acrylic: {error}"));
            return solid(if error.code() == E_INVALIDARG {
                SolidReason::UnsupportedWindows
            } else {
                SolidReason::NativeApiFailure
            });
        }
        let mut actual = 0i32;
        if DwmGetWindowAttribute(
            window,
            DWMWA_SYSTEMBACKDROP_TYPE,
            (&mut actual as *mut i32).cast(),
            4,
        )
        .is_err()
            || actual != DWMSBT_TRANSIENTWINDOW.0
        {
            return solid(SolidReason::NativeApiFailure);
        }
        let mut nonclient = 0i32;
        if DwmGetWindowAttribute(
            window,
            DWMWA_NCRENDERING_ENABLED,
            (&mut nonclient as *mut i32).cast(),
            4,
        )
        .is_err()
            || nonclient == 0
        {
            return solid(SolidReason::NativeApiFailure);
        }
        Composition::Acrylic
    }
}

fn physical_cursor() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetPhysicalCursorPos(&mut point) }.ok()?;
    Some((point.x, point.y))
}

fn centered_overlay(work: RECT, dpi: u32) -> RECT {
    let scale = f64::from(dpi.max(1)) / 96.0;
    let margin = (32.0 * scale).round() as i32;
    let available_width = work.right - work.left;
    let available_height = work.bottom - work.top;
    let fit = scale
        .min(f64::from((available_width - margin).max(1)) / 1160.0)
        .min(f64::from((available_height - margin).max(1)) / 500.0);
    let width = ((1160.0 * fit).round() as i32).max(1);
    let height = ((500.0 * fit).round() as i32).max(1);
    let left = work.left + (available_width - width) / 2;
    let top = work.top + (available_height - height) / 2;
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

fn place_overlay(window: HWND, cursor: Option<(i32, i32)>) -> Result<(), String> {
    // The hotkey's physical point remains valid if the mouse moves before the
    // UI wakes. Resolving its nearest live monitor here also tolerates unplug.
    let (x, y) = cursor.or_else(physical_cursor).unwrap_or((0, 0));
    unsafe {
        let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(monitor, &mut info)
            .ok()
            .map_err(|e| e.to_string())?;
        // Use the old HWND DPI for an initial, bounded move. Once the window
        // occupies its destination, GetDpiForWindow returns the target DPI.
        let dpi = match GetDpiForWindow(window) {
            0 => 96,
            current => current,
        };
        let place = |rect: RECT| {
            SetWindowPos(
                window,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOACTIVATE,
            )
        };
        place(centered_overlay(info.rcWork, dpi)).map_err(|e| e.to_string())?;
        // WM_DPICHANGED lets winit suggest its own bounds during the move.
        // Only correct a differing actual rectangle, before revealing the HWND.
        let actual_dpi = GetDpiForWindow(window);
        let expected =
            centered_overlay(info.rcWork, if actual_dpi == 0 { dpi } else { actual_dpi });
        let mut actual = RECT::default();
        GetWindowRect(window, &mut actual).map_err(|e| e.to_string())?;
        if actual != expected {
            place(expected).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

pub fn show_overlay(raw: isize, cursor: Option<(i32, i32)>) {
    unsafe {
        let window = hwnd(raw);
        if let Err(error) = set_visibility_intent(window, true) {
            diagnostics(error);
            return;
        }
        #[cfg(debug_assertions)]
        eprintln!(
            "[native] show {:?}, before visible={}",
            window,
            IsWindowVisible(window).as_bool()
        );
        if let Err(error) = place_overlay(window, cursor) {
            diagnostics(format!("Cannot position overlay: {error}"));
        }
        let _ = ShowWindow(window, SW_SHOWNORMAL);
        // STARTUPINFO can override the first ShowWindow call with SW_HIDE (for
        // example a background launcher). Consume that one-time instruction,
        // then honor the user's explicit reveal request.
        if !IsWindowVisible(window).as_bool() {
            let _ = ShowWindow(window, SW_SHOWNORMAL);
        }
        let activated = SetForegroundWindow(window).as_bool();
        let _ = SetFocus(Some(window));
        diagnose_composition(window, "shown");
        #[cfg(debug_assertions)]
        {
            let mut rect = RECT::default();
            let _ = GetWindowRect(window, &mut rect);
            eprintln!(
                "[native] shown visible={}, activated={activated}, rect={rect:?}",
                IsWindowVisible(window).as_bool()
            );
        }
        #[cfg(not(debug_assertions))]
        let _ = activated;
    }
}
pub fn hide_overlay(raw: isize) {
    unsafe {
        if let Err(error) = set_visibility_intent(hwnd(raw), false) {
            diagnostics(error);
        }
        let _ = ShowWindow(hwnd(raw), SW_HIDE);
    }
}

/// Capture the final desktop composition strictly inside this process's visible
/// foreground window. This debug-only comparison avoids isolated HWND capture.
/// Call on a worker: PNG encoding and disk writes never belong in egui's frame.
#[cfg(debug_assertions)]
pub fn capture_own_window(raw: isize, path: &Path) -> Result<(), String> {
    struct DpiGuard(DPI_AWARENESS_CONTEXT);
    impl Drop for DpiGuard {
        fn drop(&mut self) {
            if !self.0.0.is_null() {
                unsafe {
                    let _ = SetThreadDpiAwarenessContext(self.0);
                }
            }
        }
    }
    unsafe {
        let _dpi = DpiGuard(SetThreadDpiAwarenessContext(
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        ));
        let window = hwnd(raw);
        let mut owner = 0u32;
        GetWindowThreadProcessId(window, Some(&mut owner));
        if owner != GetCurrentProcessId()
            || !IsWindowVisible(window).as_bool()
            || IsIconic(window).as_bool()
            || GetForegroundWindow() != window
        {
            return Err(
                "Desktop diagnostics require this process's visible, foreground overlay.".into(),
            );
        }
        let mut bounds = RECT::default();
        GetWindowRect(window, &mut bounds).map_err(|e| e.to_string())?;
        let width = bounds
            .right
            .checked_sub(bounds.left)
            .ok_or("Invalid window width")?;
        let height = bounds
            .bottom
            .checked_sub(bounds.top)
            .ok_or("Invalid window height")?;
        if !(1..=4096).contains(&width) || !(1..=4096).contains(&height) {
            return Err("Diagnostic window bounds exceed the 4096 × 4096 safety limit.".into());
        }
        let screen_left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let screen_top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let screen_right = screen_left.saturating_add(GetSystemMetrics(SM_CXVIRTUALSCREEN));
        let screen_bottom = screen_top.saturating_add(GetSystemMetrics(SM_CYVIRTUALSCREEN));
        if bounds.left < screen_left
            || bounds.top < screen_top
            || bounds.right > screen_right
            || bounds.bottom > screen_bottom
        {
            return Err("Diagnostic window must be completely inside the visible desktop.".into());
        }
        // Desktop DC is sampled for this exact, validated HWND rectangle only.
        // No full-screen image, unrelated HWND or pixels outside it are retained.
        let screen = GetDC(None);
        if screen.0.is_null() {
            return Err(windows::core::Error::from_thread().to_string());
        }
        let memory = CreateCompatibleDC(Some(screen));
        if memory.0.is_null() {
            let error = windows::core::Error::from_thread().to_string();
            let _ = ReleaseDC(None, screen);
            return Err(error);
        }
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        if bitmap.0.is_null() {
            let error = windows::core::Error::from_thread().to_string();
            let _ = DeleteDC(memory);
            let _ = ReleaseDC(None, screen);
            return Err(error);
        }
        let previous = SelectObject(memory, HGDIOBJ(bitmap.0));
        if previous.0.is_null() || previous.0 as isize == -1 {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(memory);
            let _ = ReleaseDC(None, screen);
            return Err("Windows could not select the diagnostic bitmap.".into());
        }
        let copied = BitBlt(
            memory,
            0,
            0,
            width,
            height,
            Some(screen),
            bounds.left,
            bounds.top,
            SRCCOPY | CAPTUREBLT,
        )
        .map_err(|e| e.to_string());
        // GetDIBits requires that the bitmap is not selected into any DC.
        let _ = SelectObject(memory, previous);
        let result = (|| {
            copied?;
            let mut pixels = vec![0u8; width as usize * height as usize * 4];
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let rows = GetDIBits(
                screen,
                bitmap,
                0,
                height as u32,
                Some(pixels.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            );
            if rows != height {
                return Err("Windows could not read every desktop diagnostic row.".to_owned());
            }
            // BI_RGB supplies BGRA; GDI's unused alpha is not meaningful here.
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
            Ok(pixels)
        })();
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory);
        let _ = ReleaseDC(None, screen);
        let pixels = result?;
        image::save_buffer_with_format(
            path,
            &pixels,
            width as u32,
            height as u32,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .map_err(|e| e.to_string())?;
        diagnostics(format_args!(
            "Saved desktop composition for own HWND rectangle: {width} × {height}"
        ));
        Ok(())
    }
}

struct ComApartment;
impl ComApartment {
    fn new() -> Result<Self, String> {
        Self::initialize(COINIT_APARTMENTTHREADED)
    }

    fn initialize(model: COINIT) -> Result<Self, String> {
        unsafe { CoInitializeEx(None, model).ok() }.map_err(|e| e.to_string())?;
        Ok(Self)
    }
}
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
fn desktop() -> Result<IDesktopWallpaper, String> {
    unsafe { CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL) }.map_err(|e| e.to_string())
}
unsafe fn owned_com_string(raw: PWSTR) -> String {
    if raw.is_null() {
        return String::new();
    }
    unsafe {
        let value = raw.to_string().unwrap_or_default();
        CoTaskMemFree(Some(raw.0.cast()));
        value
    }
}

/// Confined to the serial file worker. The interface is released before its
/// apartment. An idle MTA client can sleep without dispatching Windows messages.
#[derive(Default)]
pub struct WallpaperClient {
    connection: Option<(IDesktopWallpaper, ComApartment)>,
}

impl WallpaperClient {
    fn desktop(&mut self) -> Result<&IDesktopWallpaper, String> {
        if self.connection.is_none() {
            let apartment = ComApartment::initialize(COINIT_MULTITHREADED)?;
            self.connection = Some((desktop()?, apartment));
        }
        Ok(&self.connection.as_ref().unwrap().0)
    }

    pub fn current(&mut self) -> Result<Option<PathBuf>, String> {
        // NULL returns an empty string when monitors have different backgrounds.
        match unsafe { self.desktop()?.GetWallpaper(PCWSTR::null()) } {
            Ok(raw) => {
                let path = unsafe { owned_com_string(raw) };
                Ok((!path.is_empty()).then(|| PathBuf::from(path)))
            }
            Err(error) => {
                self.connection = None;
                Err(error.to_string())
            }
        }
    }
}

fn prepare_wallpaper(path: &Path, cache: &Path) -> Result<PathBuf, String> {
    let path = std::fs::canonicalize(path).map_err(|e| format!("Image is unavailable: {e}"))?;
    // Recheck the file, since references may change after import.
    let file = std::fs::File::open(&path).map_err(|e| format!("Image is unavailable: {e}"))?;
    // image's JPEG decoder buffers the compressed stream during header inspection.
    if file.metadata().map_err(|e| e.to_string())?.len() > 512 * 1024 * 1024 {
        return Err("Image exceeds the 512 MiB file limit.".into());
    }
    let mut reader = image::ImageReader::new(std::io::BufReader::new(file))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let format = reader.format();
    if !matches!(
        format,
        Some(image::ImageFormat::Jpeg | image::ImageFormat::Png | image::ImageFormat::WebP)
    ) {
        return Err("Choose a JPEG, PNG or WebP image.".into());
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(32768);
    limits.max_image_height = Some(32768);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    // Windows codec availability for WebP is optional. Also normalize misnamed
    // files: Shell image loaders can infer the codec from the extension.
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    let shell_compatible = match format {
        Some(image::ImageFormat::Jpeg) => {
            extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg")
        }
        Some(image::ImageFormat::Png) => extension.eq_ignore_ascii_case("png"),
        _ => false,
    };
    use image::ImageDecoder;
    let decoder = reader
        .into_decoder()
        .map_err(|e| format!("Cannot read image: {e}"))?;
    let (width, height) = decoder.dimensions();
    if !crate::core::safe_image_dimensions(width, height) {
        return Err("Image exceeds safe dimensions (100 megapixels).".into());
    }
    if shell_compatible {
        // Shell decodes these formats itself. The application only reads headers.
        return Ok(path);
    }
    let target = converted_path(&path, cache)?;
    if !target.is_file() {
        std::fs::create_dir_all(cache).map_err(|e| e.to_string())?;
        if decoder.total_bytes() > 512 * 1024 * 1024 {
            return Err("Converted image exceeds the allocation limit.".into());
        }
        let image = image::DynamicImage::from_decoder(decoder)
            .map_err(|e| format!("Cannot decode image: {e}"))?;
        let temp = target.with_extension("tmp");
        image
            .save_with_format(&temp, image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        std::fs::rename(&temp, &target).map_err(|e| e.to_string())?;
    }
    let source = target.with_extension("source.json");
    if !source.is_file() {
        crate::core::atomic_write(
            &source,
            &serde_json::to_vec(&path).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(target)
}

fn converted_path(path: &Path, cache: &Path) -> Result<PathBuf, String> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    metadata.len().hash(&mut hasher);
    metadata.modified().ok().hash(&mut hasher);
    Ok(cache.join(format!("wallpaper-{:016x}.png", hasher.finish())))
}

/// Resolve this on the worker when synchronizing OS state, never per UI frame.
/// WebP uses a stable converted file while the collection keeps its original.
pub fn wallpaper_matches(original: &Path, current: &Path, cache_dir: &Path) -> bool {
    let Ok(original) = std::fs::canonicalize(original) else {
        return false;
    };
    let Ok(current) = std::fs::canonicalize(current) else {
        return false;
    };
    if original == current {
        return true;
    }
    converted_path(&original, cache_dir)
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .is_some_and(|path| path == current)
}

fn same_existing_file(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

pub struct AppliedWallpaper {
    ready: PathBuf,
    prepare: Duration,
    com_setup: Duration,
    native: Duration,
}

impl AppliedWallpaper {
    pub fn timing_line(&self, queue: Duration, total: Duration) -> String {
        let millis = |duration: Duration| duration.as_secs_f64() * 1000.0;
        format!(
            "queue_ms={:.3} prepare_ms={:.3} com_ms={:.3} native_ms={:.3} total_ms={:.3}",
            millis(queue),
            millis(self.prepare),
            millis(self.com_setup),
            millis(self.native),
            millis(total)
        )
    }
}

impl WallpaperClient {
    pub fn apply(&mut self, path: &Path, cache_dir: &Path) -> Result<AppliedWallpaper, String> {
        use std::time::Instant;
        let begin = Instant::now();
        let ready = prepare_wallpaper(path, cache_dir)?;
        let prepared = Instant::now();
        let desktop = self.desktop()?;
        let path = wide(ready.as_os_str());
        // NULL intentionally applies the same image across all displays. SetPosition
        // is never called, preserving the user's existing Windows fit preference.
        let call = Instant::now();
        if let Err(error) = unsafe { desktop.SetWallpaper(PCWSTR::null(), PCWSTR(path.as_ptr())) } {
            // Reconnect on the next explicit action. Replaying a failed RPC could
            // apply the same image twice if the server changed it before disconnect.
            self.connection = None;
            return Err(format!("Windows could not apply the wallpaper: {error}"));
        }
        Ok(AppliedWallpaper {
            ready,
            prepare: prepared.duration_since(begin),
            com_setup: call.duration_since(prepared),
            native: call.elapsed(),
        })
    }
}

#[derive(serde::Serialize)]
struct WallpaperSnapshot {
    wallpapers: Vec<(String, String)>,
    position: i32,
    enabled: bool,
}

/// Explicit reversible validation, including a complete interactive egui run.
/// Only plain data survives between begin/finish: COM never spans the UI loop.
pub struct WallpaperValidation {
    snapshot: WallpaperSnapshot,
    recovery: PathBuf,
    finished: bool,
}

impl WallpaperValidation {
    /// Capture and durably record the current static desktop without changing it.
    /// Slideshows, remote sessions and unavailable originals are refused.
    pub fn begin(cache_dir: &Path) -> Result<Self, String> {
        let _com = ComApartment::new()?;
        let desktop = desktop()?;
        let snapshot = unsafe {
            let status = desktop.GetStatus().map_err(|e| e.to_string())?;
            if status.0 & (DSS_SLIDESHOW.0 | DSS_DISABLED_BY_REMOTE_SESSION.0) != 0 {
                return Err(
                    "Validation refused: a slideshow or remote desktop must remain untouched."
                        .into(),
                );
            }
            let position = desktop.GetPosition().map_err(|e| e.to_string())?.0;
            let mut wallpapers = Vec::new();
            for index in 0..desktop
                .GetMonitorDevicePathCount()
                .map_err(|e| e.to_string())?
            {
                let monitor = owned_com_string(
                    desktop
                        .GetMonitorDevicePathAt(index)
                        .map_err(|e| e.to_string())?,
                );
                let monitor_wide = wide(OsStr::new(&monitor));
                let original = owned_com_string(
                    desktop
                        .GetWallpaper(PCWSTR(monitor_wide.as_ptr()))
                        .map_err(|e| e.to_string())?,
                );
                if original.is_empty() || !Path::new(&original).is_file() {
                    return Err(
                        "Validation refused: original wallpaper cannot be safely restored.".into(),
                    );
                }
                wallpapers.push((monitor, original));
            }
            if wallpapers.is_empty() {
                return Err("No monitors available for reversible validation.".into());
            }
            WallpaperSnapshot {
                wallpapers,
                position,
                enabled: status.0 & DSS_ENABLED.0 != 0,
            }
        };
        std::fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
        let recovery = cache_dir.join("validation-recovery.json");
        write_validation_recovery(&snapshot, &recovery)?;
        Ok(Self {
            snapshot,
            recovery,
            finished: false,
        })
    }

    /// Restore every monitor, fit and background-enabled state, verify each,
    /// then remove the recovery record. Failed attempts retain that record.
    /// This consumes the guard; an explicit failure is not silently retried by Drop.
    pub fn finish(mut self) -> Result<(), String> {
        self.finished = true;
        self.restore()
            .map_err(|error| format!("{error}; recovery record: {}", self.recovery.display()))
    }

    fn restore(&self) -> Result<(), String> {
        let _com = ComApartment::new()?;
        let desktop = desktop()?;
        unsafe {
            let mut errors = Vec::new();
            // Attempt every monitor even when an earlier restoration fails.
            for (monitor, original) in &self.snapshot.wallpapers {
                let monitor = wide(OsStr::new(monitor));
                let original = wide(OsStr::new(original));
                if let Err(error) =
                    desktop.SetWallpaper(PCWSTR(monitor.as_ptr()), PCWSTR(original.as_ptr()))
                {
                    errors.push(error.to_string());
                }
            }
            if let Err(error) =
                desktop.SetPosition(DESKTOP_WALLPAPER_POSITION(self.snapshot.position))
            {
                errors.push(error.to_string());
            }
            if let Err(error) = desktop.Enable(self.snapshot.enabled) {
                errors.push(error.to_string());
            }
            if !errors.is_empty() {
                return Err(format!(
                    "Wallpaper restoration failed: {}",
                    errors.join("; ")
                ));
            }
            for (monitor, original) in &self.snapshot.wallpapers {
                let monitor = wide(OsStr::new(monitor));
                let restored = owned_com_string(
                    desktop
                        .GetWallpaper(PCWSTR(monitor.as_ptr()))
                        .map_err(|e| e.to_string())?,
                );
                if !same_existing_file(Path::new(&restored), Path::new(original)) {
                    return Err("Wallpaper restoration verification failed".into());
                }
            }
            let restored_position = desktop.GetPosition().map_err(|e| e.to_string())?.0;
            let restored_enabled =
                desktop.GetStatus().map_err(|e| e.to_string())?.0 & DSS_ENABLED.0 != 0;
            if restored_position != self.snapshot.position
                || restored_enabled != self.snapshot.enabled
            {
                return Err(
                    "Wallpaper fit or enabled-state restoration verification failed".into(),
                );
            }
        }
        std::fs::remove_file(&self.recovery).map_err(|e| {
            format!("Original desktop restored, but recovery record could not be removed: {e}")
        })
    }
}

impl Drop for WallpaperValidation {
    fn drop(&mut self) {
        if !self.finished {
            self.finished = true;
            if let Err(error) = self.restore() {
                use std::io::Write;
                let _ = writeln!(
                    std::io::stderr(),
                    "wall: validation rollback failed: {error}; recovery record retained at {}",
                    self.recovery.display()
                );
            }
        }
    }
}

fn write_validation_recovery(snapshot: &WallpaperSnapshot, recovery: &Path) -> Result<(), String> {
    use std::io::Write;
    let bytes = serde_json::to_vec_pretty(snapshot).map_err(|e| e.to_string())?;
    // Never overwrite recovery information from an interrupted earlier run.
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(recovery)
        .map_err(|e| format!("Cannot create validation recovery record {} (an existing record must be resolved first): {e}", recovery.display()))?;
    let result = file.write_all(&bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = result {
        // No desktop mutation has occurred. This incomplete record belongs only
        // to this failed create_new call, so removing it cannot erase an old run.
        let _ = std::fs::remove_file(recovery);
        return Err(format!(
            "Cannot persist validation recovery record: {error}"
        ));
    }
    Ok(())
}

/// Deliberate CLI validation, using the same guard as interactive GUI validation.
pub fn validate_wallpaper_roundtrip(path: &Path, cache_dir: &Path) -> Result<(), String> {
    let validation = WallpaperValidation::begin(cache_dir)?;
    let applied = (|| {
        let mut client = WallpaperClient::default();
        // Browsing queries the actual desktop before the user commits a choice.
        let opening = std::time::Instant::now();
        client.current()?;
        let current_ms = opening.elapsed().as_secs_f64() * 1000.0;
        let begin = std::time::Instant::now();
        let applied = client.apply(path, cache_dir)?;
        let timing = applied.timing_line(Duration::ZERO, begin.elapsed());
        std::fs::write(
            cache_dir.join("apply-latency.txt"),
            format!("opening_query_ms={current_ms:.3} {timing}"),
        )
        .map_err(|e| e.to_string())?;
        let current = client
            .current()?
            .ok_or("Windows did not report the test wallpaper")?;
        if same_existing_file(&current, &applied.ready) {
            Ok(())
        } else {
            Err("Windows did not report the test wallpaper".into())
        }
    })();
    // Restoration errors take precedence over an application failure. The guard
    // still restores on early unwinding and does not hold either COM object here.
    validation.finish()?;
    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tray_uses_shell_contrast_unless_the_symbol_is_explicit() {
        assert_eq!(tray_resource(TrayAppearance::Automatic, true), TRAY_INK);
        assert_eq!(tray_resource(TrayAppearance::Automatic, false), TRAY_CHALK);
        for system_light in [false, true] {
            assert_eq!(
                tray_resource(TrayAppearance::Light, system_light),
                TRAY_CHALK
            );
            assert_eq!(tray_resource(TrayAppearance::Dark, system_light), TRAY_INK);
        }
    }
    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn places_hidden_hwnd_on_each_attached_monitor() {
        unsafe extern "system" fn collect(
            monitor: HMONITOR,
            _: HDC,
            _: *mut RECT,
            data: LPARAM,
        ) -> windows::core::BOOL {
            unsafe {
                (*(data.0 as *mut Vec<HMONITOR>)).push(monitor);
            }
            true.into()
        }
        unsafe {
            let prior = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            struct DesktopGuard(HWND, DPI_AWARENESS_CONTEXT);
            impl Drop for DesktopGuard {
                fn drop(&mut self) {
                    unsafe {
                        let _ = DestroyWindow(self.0);
                        let _ = SetThreadDpiAwarenessContext(self.1);
                    }
                }
            }
            let window = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("wall placement test"),
                WS_POPUP,
                0,
                0,
                100,
                100,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            let _guard = DesktopGuard(window, prior);
            let mut monitors = Vec::<HMONITOR>::new();
            EnumDisplayMonitors(
                None,
                None,
                Some(collect),
                LPARAM((&mut monitors as *mut Vec<HMONITOR>) as isize),
            )
            .ok()
            .unwrap();
            assert!(!monitors.is_empty());
            for &monitor in monitors.iter().chain(monitors.iter().rev()) {
                let mut info = MONITORINFO {
                    cbSize: size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                GetMonitorInfoW(monitor, &mut info).ok().unwrap();
                let work = info.rcWork;
                for point in [
                    (work.left + 1, work.top + 1),
                    (work.right - 2, work.bottom - 2),
                ] {
                    place_overlay(window, Some(point)).unwrap();
                    let mut actual = RECT::default();
                    GetWindowRect(window, &mut actual).unwrap();
                    let dpi = GetDpiForWindow(window);
                    assert_eq!(actual, centered_overlay(work, dpi));
                    assert_eq!(MonitorFromWindow(window, MONITOR_DEFAULTTONULL), monitor);
                    assert!(!IsWindowVisible(window).as_bool());
                    eprintln!("point={point:?} dpi={dpi} work={work:?} result={actual:?}");
                }
            }
        }
    }

    #[test]
    fn placement_centers_in_physical_work_area_for_all_monitor_arrangements() {
        for (left, top, width, height, dpi) in [
            (0, 0, 1920, 1040, 96),
            (-2560, -290, 2560, 1400, 144),
            (1920, 33, 1921, 1041, 120),
            (0, -1592, 2560, 1552, 192),
            (-317, 1080, 1080, 1880, 168),
            (0, 0, 800, 600, 216),
        ] {
            let work = RECT {
                left,
                top,
                right: left + width,
                bottom: top + height,
            };
            let rect = centered_overlay(work, dpi);
            assert!(rect.left >= left && rect.right <= work.right);
            assert!(rect.top >= top && rect.bottom <= work.bottom);
            assert!((rect.left - left - (work.right - rect.right)).abs() <= 1);
            assert!((rect.top - top - (work.bottom - rect.bottom)).abs() <= 1);
            let w = f64::from(rect.right - rect.left);
            let h = f64::from(rect.bottom - rect.top);
            assert!((w / h - 1160.0 / 500.0).abs() < 0.01);
        }
    }

    #[test]
    fn native_acrylic_retains_dwm_caption_without_system_buttons() {
        let original = (WS_VISIBLE
            | WS_CLIPCHILDREN
            | WS_SYSMENU
            | WS_THICKFRAME
            | WS_MINIMIZEBOX
            | WS_MAXIMIZEBOX)
            .0;
        let native = overlay_style(original);
        assert_eq!(native & WS_CAPTION.0, WS_CAPTION.0);
        assert_eq!(
            native & (WS_SYSMENU | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX).0,
            0
        );
        assert_eq!(
            native & (WS_VISIBLE | WS_CLIPCHILDREN).0,
            original & (WS_VISIBLE | WS_CLIPCHILDREN).0
        );
        assert_eq!(overlay_style(native), native);
    }

    #[test]
    fn validation_recovery_preserves_an_interrupted_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "wall-recovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let recovery = root.join("validation-recovery.json");
        let snapshot = WallpaperSnapshot {
            wallpapers: vec![("test monitor".into(), "C:\\generated-test.png".into())],
            position: 4,
            enabled: true,
        };
        write_validation_recovery(&snapshot, &recovery).unwrap();
        let before = std::fs::read(&recovery).unwrap();
        let replacement = WallpaperSnapshot {
            wallpapers: Vec::new(),
            position: 0,
            enabled: false,
        };
        assert!(write_validation_recovery(&replacement, &recovery).is_err());
        assert_eq!(std::fs::read(&recovery).unwrap(), before);
        let saved: serde_json::Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(saved["position"], 4);
        assert_eq!(saved["enabled"], true);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn outside_click_policy_preserves_explorer_but_dismisses_desktop() {
        assert!(is_import_surface("CabinetWClass"));
        assert!(is_import_surface("ExploreWClass"));
        assert!(is_import_surface("Shell_TrayWnd"));
        assert!(is_import_surface("Shell_SecondaryTrayWnd"));
        assert!(!is_import_surface("Progman"));
        assert!(!is_import_surface("WorkerW"));
        assert!(!is_import_surface("Chrome_WidgetWin_1"));
        let bounds = RECT {
            left: -1800,
            top: -500,
            right: -640,
            bottom: 0,
        };
        assert!(point_inside(POINT { x: -1800, y: -500 }, bounds));
        assert!(point_inside(POINT { x: -641, y: -1 }, bounds));
        assert!(!point_inside(POINT { x: -640, y: -1 }, bounds));
        assert!(!point_inside(POINT { x: -641, y: 0 }, bounds));
    }

    #[test]
    fn overlay_styles_preserve_drop_target_without_layered_click_through() {
        let style = overlay_exstyle(
            (WS_EX_ACCEPTFILES
                | WS_EX_TOPMOST
                | WS_EX_APPWINDOW
                | WS_EX_LAYERED
                | WS_EX_NOREDIRECTIONBITMAP
                | WS_EX_TRANSPARENT)
                .0,
        );
        assert_ne!(style & WS_EX_ACCEPTFILES.0, 0);
        assert_ne!(style & WS_EX_TOPMOST.0, 0);
        assert_eq!(
            style
                & (WS_EX_APPWINDOW | WS_EX_LAYERED | WS_EX_NOREDIRECTIONBITMAP | WS_EX_TRANSPARENT)
                    .0,
            0
        );
    }

    #[test]
    fn hotkey_validation_requires_intentional_modifiers_and_supported_key() {
        let mut key = Hotkey {
            ctrl: false,
            alt: true,
            shift: false,
            key: b'W' as u32,
        };
        assert!(validate_hotkey(key).is_ok());
        key.alt = false;
        assert!(validate_hotkey(key).is_err());
        key.ctrl = true;
        key.key = 0x70;
        assert!(validate_hotkey(key).is_ok());
        key.key = 0;
        assert!(validate_hotkey(key).is_err());
    }
    #[test]
    fn unavailable_wallpaper_is_reported_before_windows_mutation() {
        assert!(
            prepare_wallpaper(Path::new("Z:\\wall-no-such-image-9977.png"), Path::new("."))
                .is_err()
        );
    }

    #[test]
    fn webp_with_wrong_extension_is_converted_and_matches_original() {
        let root = std::env::temp_dir().join(format!(
            "wall-platform-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let original = root.join("misnamed.jpg");
        let image = image::RgbaImage::from_pixel(4, 2, image::Rgba([30, 50, 70, 255]));
        image
            .save_with_format(&original, image::ImageFormat::WebP)
            .unwrap();
        let cache = root.join("applied");
        let converted = prepare_wallpaper(&original, &cache).unwrap();
        assert_eq!(converted.extension().unwrap(), "png");
        assert!(wallpaper_matches(&original, &converted, &cache));
        assert_eq!(image::image_dimensions(&converted).unwrap(), (4, 2));
        // A second apply reuses both generated files. This also works when a
        // read-only backup tool has the source sidecar open.
        let sidecar = converted.with_extension("source.json");
        let locked_sidecar = std::os::windows::fs::OpenOptionsExt::share_mode(
            std::fs::OpenOptions::new().read(true),
            1,
        )
        .open(&sidecar)
        .unwrap();
        assert_eq!(prepare_wallpaper(&original, &cache).unwrap(), converted);
        drop(locked_sidecar);
        let direct = root.join("direct.PNG");
        image
            .save_with_format(&direct, image::ImageFormat::Png)
            .unwrap();
        assert_eq!(
            prepare_wallpaper(&direct, &cache).unwrap(),
            std::fs::canonicalize(&direct).unwrap()
        );
        let misnamed_png = root.join("png-with-jpeg-extension.jpeg");
        image
            .save_with_format(&misnamed_png, image::ImageFormat::Png)
            .unwrap();
        let converted_png = prepare_wallpaper(&misnamed_png, &cache).unwrap();
        assert_ne!(converted_png, misnamed_png);
        assert!(wallpaper_matches(&misnamed_png, &converted_png, &cache));
        let corrupted = root.join("broken.png");
        std::fs::write(&corrupted, b"not an image").unwrap();
        assert!(prepare_wallpaper(&corrupted, &cache).is_err());
        // Only this test's unique temporary fixture directory is removed.
        std::fs::remove_dir_all(root).unwrap();
    }
}
