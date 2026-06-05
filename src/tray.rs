use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use crate::app_icon::rtr_icon_rgba;
use crate::app_paths::AppPaths;
use crate::host::{TranslationMode, TranslatorHost};
use crate::logging::LogBuffer;
use crate::settings::AppSettings;
use crate::usage_metrics::{UsageMetrics, UsageSnapshot, UsageStatsPeriod};

pub type ShutdownSignal = Arc<Mutex<Option<oneshot::Sender<()>>>>;

const CMD_DASHBOARD: u16 = 1001;
const CMD_TOGGLE_WEBVIEW: u16 = 1002;
const CMD_REQUEST_GUARD: u16 = 1004;
const CMD_LOGS: u16 = 1005;
const CMD_STATS: u16 = 1006;
const CMD_EXIT: u16 = 1007;
const CMD_CLOSE_WINDOW: u16 = 1008;
const CMD_REFRESH_WINDOW: u16 = 1009;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrayMenuEntry {
    Action { command: u16, label: &'static str },
    Separator,
}

const TRAY_MENU: &[TrayMenuEntry] = &[
    TrayMenuEntry::Action {
        command: CMD_DASHBOARD,
        label: "대시보드",
    },
    TrayMenuEntry::Action {
        command: CMD_TOGGLE_WEBVIEW,
        label: "WebView 표시/숨김",
    },
    TrayMenuEntry::Action {
        command: CMD_REQUEST_GUARD,
        label: "요청 취소/복구",
    },
    TrayMenuEntry::Separator,
    TrayMenuEntry::Action {
        command: CMD_LOGS,
        label: "로그 창 띄우기",
    },
    TrayMenuEntry::Action {
        command: CMD_STATS,
        label: "통계 보기",
    },
    TrayMenuEntry::Separator,
    TrayMenuEntry::Action {
        command: CMD_EXIT,
        label: "프로그램 종료",
    },
];

fn build_dashboard_text(
    mode: TranslationMode,
    settings: &AppSettings,
    usage: &UsageSnapshot,
) -> String {
    let webview_status = if matches!(
        mode,
        TranslationMode::WebView | TranslationMode::ChatGptWebView
    ) {
        "트레이 메뉴에서 표시/숨김 토글 가능"
    } else {
        "Gemini CLI 모드"
    };

    format!(
        "Mode: {}\nBase URL: {}\nRaw Prompt: {}\nWebView visibility: {}\nStatus: 실행 중\n\nRequests: {} total / {} ok / {} failed / {} cancelled\nSuccess rate: {:.1}%\nStarted: {}\nLast updated: {}\nLast failure: {}",
        mode.label(),
        settings.base_url,
        on_off(settings.raw_prompt_mode),
        webview_status,
        usage.total_requests,
        usage.succeeded_requests,
        usage.failed_requests,
        usage.cancelled_requests,
        usage.success_rate(),
        empty_dash(&usage.started_at_local),
        empty_dash(&usage.last_updated_at_local),
        empty_dash(&usage.last_failure)
    )
}

fn build_stats_text(usage: &UsageSnapshot) -> String {
    format!(
        "Total: {}\nSucceeded: {}\nFailed: {}\nCancelled: {}\nSuccess rate: {:.1}%\n\nGemini: {}\nOpenAI: {}\n호환 API: {}\nOther: {}\n\nInput tokens: {}\nOutput tokens: {}\nInput chars: {}\nOutput chars: {}\n\nLast failure: {}",
        usage.total_requests,
        usage.succeeded_requests,
        usage.failed_requests,
        usage.cancelled_requests,
        usage.success_rate(),
        usage.gemini_requests,
        usage.open_ai_requests,
        usage.mort_requests,
        usage.other_requests,
        usage.input_tokens,
        usage.successful_output_tokens,
        usage.input_characters,
        usage.successful_output_characters,
        empty_dash(&usage.last_failure)
    )
}

fn build_log_text(lines: &[String]) -> String {
    if lines.is_empty() {
        "최근 로그가 없습니다.".to_owned()
    } else {
        lines.join("\n")
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "On" } else { "Off" }
}

fn empty_dash(value: &str) -> &str {
    if value.trim().is_empty() { "-" } else { value }
}

fn format_count(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::with_capacity(raw.len() + raw.len() / 3);
    for (index, ch) in raw.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn send_shutdown(signal: &ShutdownSignal) {
    if let Some(tx) = signal.lock().take() {
        let _ = tx.send(());
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::sync::mpsc;
    use std::thread;

    use windows::Win32::Foundation::{
        COLORREF, ERROR_SUCCESS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
    };
    use windows::Win32::Graphics::Dwm::{
        DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
        DwmSetWindowAttribute,
    };
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
        CreateBitmap, CreateDIBSection, CreateFontW, CreatePen, CreateSolidBrush, DEFAULT_CHARSET,
        DEFAULT_GUI_FONT, DEFAULT_PITCH, DIB_RGB_COLORS, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX,
        DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint, FF_DONTCARE, FW_NORMAL,
        FW_SEMIBOLD, FillRect, GetMonitorInfoW, GetStockObject, HBITMAP, HFONT, HGDIOBJ, HPEN,
        InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, RGBQUAD, RoundRect, SelectObject, SetBkMode,
        SetTextColor, TRANSPARENT, UpdateWindow,
    };
    use windows::Win32::System::LibraryLoader;
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows::Win32::UI::Shell::{
        NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
        NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, BS_FLAT, BS_PUSHBUTTON, CREATESTRUCTW, CW_USEDEFAULT, CreateIconIndirect,
        CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow,
        DispatchMessageW, ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE, ES_READONLY, GWLP_USERDATA,
        GetClientRect, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, HICON,
        HMENU, ICONINFO, IDI_APPLICATION, LoadIconW, MB_ICONINFORMATION, MB_OK, MENU_ITEM_FLAGS,
        MF_SEPARATOR, MF_STRING, MINMAXINFO, MSG, MessageBoxW, MoveWindow, PostMessageW,
        PostQuitMessage, RegisterClassW, SM_CXSMICON, SM_CYSMICON, SW_SHOWNORMAL, SendMessageW,
        SetForegroundWindow, SetWindowLongPtrW, SetWindowTextW, ShowWindow, TPM_NONOTIFY,
        TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WA_INACTIVE,
        WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY,
        WM_GETMINMAXINFO, WM_LBUTTONDBLCLK, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_RBUTTONUP,
        WM_SETFONT, WM_SIZE, WM_USER, WNDCLASSW, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
        WS_HSCROLL, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
    };
    use windows::core::{HSTRING, PCWSTR, w};

    use super::*;

    const TRAY_ICON_ID: u32 = 1;
    const WM_TRAYICON: u32 = WM_USER + 42;

    pub struct TrayHandle {
        hwnd: isize,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Drop for TrayHandle {
        fn drop(&mut self) {
            if self.hwnd != 0 {
                let hwnd = HWND(self.hwnd as *mut _);
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
            let _ = self.thread.take();
        }
    }

    struct OwnedIcon(HICON);

    impl OwnedIcon {
        fn handle(&self) -> HICON {
            self.0
        }
    }

    impl Drop for OwnedIcon {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = DestroyIcon(self.0);
                }
            }
        }
    }

    struct TrayState {
        paths: AppPaths,
        settings: Arc<RwLock<AppSettings>>,
        host: Arc<TranslatorHost>,
        logs: LogBuffer,
        runtime: Handle,
        shutdown: ShutdownSignal,
        tray_icon: Option<OwnedIcon>,
    }

    struct TextWindowState {
        body: String,
        edit_hwnd: HWND,
    }

    struct OwnedFont(HFONT);

    impl OwnedFont {
        fn handle(&self) -> HFONT {
            self.0
        }

        fn is_valid(&self) -> bool {
            !self.0.0.is_null()
        }
    }

    impl Drop for OwnedFont {
        fn drop(&mut self) {
            if self.is_valid() {
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(self.0.0));
                }
            }
        }
    }

    struct UiFonts {
        title: OwnedFont,
        body: OwnedFont,
        small: OwnedFont,
        button: OwnedFont,
        mono: OwnedFont,
    }

    impl UiFonts {
        fn new() -> Self {
            Self {
                title: create_font(26, FW_SEMIBOLD.0 as i32, false),
                body: create_font(15, FW_NORMAL.0 as i32, false),
                small: create_font(13, FW_NORMAL.0 as i32, false),
                button: create_font(14, FW_NORMAL.0 as i32, false),
                mono: create_font(13, FW_NORMAL.0 as i32, true),
            }
        }
    }

    #[derive(Clone, Debug)]
    struct TrayMenuSnapshot {
        mode: String,
        requests: String,
        success_rate: String,
        last_failure: String,
    }

    struct TrayMenuWindowState {
        owner: HWND,
        snapshot: TrayMenuSnapshot,
        theme: TrayTheme,
        fonts: UiFonts,
        buttons: Vec<HWND>,
        close_requested: bool,
    }

    #[derive(Clone, Debug)]
    struct DashboardData {
        mode_label: String,
        base_url: String,
        raw_prompt: String,
        webview_status: String,
        runtime_status: String,
        total_requests: String,
        success_summary: String,
        failure_summary: String,
        token_summary: String,
        provider_summary: String,
        started: String,
        updated: String,
        last_failure: String,
    }

    struct DashboardWindowState {
        owner: HWND,
        data: DashboardData,
        notice: String,
        theme: TrayTheme,
        fonts: UiFonts,
        buttons: Vec<HWND>,
    }

    #[derive(Clone, Debug)]
    struct StatsData {
        total_requests: String,
        success_summary: String,
        failure_summary: String,
        token_summary: String,
        provider_summary: String,
        updated_summary: String,
        detail: String,
    }

    struct StatsWindowState {
        owner: HWND,
        data: StatsData,
        theme: TrayTheme,
        edit_hwnd: HWND,
        fonts: UiFonts,
        buttons: Vec<HWND>,
    }

    #[derive(Clone, Copy)]
    struct ButtonBounds {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    }

    #[derive(Clone, Copy)]
    struct TrayTheme {
        dark: bool,
        bg: COLORREF,
        surface: COLORREF,
        border: COLORREF,
        text: COLORREF,
        muted: COLORREF,
        muted_soft: COLORREF,
        accent: COLORREF,
        accent_soft: COLORREF,
        success: COLORREF,
    }

    impl TrayTheme {
        fn from_settings(settings: &AppSettings) -> Self {
            let dark = match settings.theme_mode.as_str() {
                "Dark" => true,
                "Light" => false,
                _ => windows_apps_use_dark_theme(),
            };

            if dark {
                Self {
                    dark,
                    bg: color(24, 24, 27),
                    surface: color(36, 38, 45),
                    border: color(69, 73, 85),
                    text: color(244, 244, 245),
                    muted: color(181, 186, 197),
                    muted_soft: color(135, 141, 153),
                    accent: color(96, 165, 250),
                    accent_soft: color(37, 42, 52),
                    success: color(74, 222, 128),
                }
            } else {
                Self {
                    dark,
                    bg: color(247, 249, 252),
                    surface: color(255, 255, 255),
                    border: color(214, 223, 235),
                    text: color(27, 36, 48),
                    muted: color(101, 113, 132),
                    muted_soft: color(113, 113, 122),
                    accent: color(217, 70, 135),
                    accent_soft: color(252, 231, 240),
                    success: color(22, 163, 74),
                }
            }
        }
    }

    pub fn spawn(
        paths: AppPaths,
        settings: Arc<RwLock<AppSettings>>,
        host: Arc<TranslatorHost>,
        logs: LogBuffer,
        runtime: Handle,
        shutdown: ShutdownSignal,
    ) -> Option<TrayHandle> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_logs = logs.clone();
        let thread = thread::spawn(move || {
            if let Err(error) = run_tray_thread(
                paths,
                settings,
                host,
                thread_logs.clone(),
                runtime,
                shutdown,
                ready_tx,
            ) {
                thread_logs.push(format!("[Tray] 시작 실패: {error}"));
            }
        });

        match ready_rx.recv() {
            Ok(Ok(hwnd)) => {
                logs.push("[Tray] Windows 알림 영역 아이콘 시작");
                Some(TrayHandle {
                    hwnd,
                    thread: Some(thread),
                })
            }
            Ok(Err(error)) => {
                logs.push(format!("[Tray] 시작 실패: {error}"));
                let _ = thread.join();
                None
            }
            Err(error) => {
                logs.push(format!("[Tray] 시작 응답 실패: {error}"));
                let _ = thread.join();
                None
            }
        }
    }

    fn run_tray_thread(
        paths: AppPaths,
        settings: Arc<RwLock<AppSettings>>,
        host: Arc<TranslatorHost>,
        logs: LogBuffer,
        runtime: Handle,
        shutdown: ShutdownSignal,
        ready_tx: mpsc::Sender<Result<isize, String>>,
    ) -> Result<(), String> {
        let hinstance = unsafe {
            LibraryLoader::GetModuleHandleW(None)
                .map(|module| HINSTANCE(module.0))
                .map_err(|error| format!("{error}"))?
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: hinstance,
            lpszClassName: w!("RusterTrayWindow"),
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&class);
        }

        let state = Box::new(TrayState {
            paths,
            settings,
            host,
            logs,
            runtime,
            shutdown,
            tray_icon: create_rtr_tray_icon(),
        });
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("RusterTrayWindow"),
                w!("ruster tray"),
                WINDOW_STYLE::default(),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                Some(hinstance),
                Some(Box::into_raw(state) as *const _),
            )
            .map_err(|error| format!("{error}"))?
        };
        if hwnd.0.is_null() {
            let _ = ready_tx.send(Err("트레이 숨김 창 생성 실패".to_owned()));
            return Ok(());
        }

        if !add_tray_icon(hwnd) {
            let _ = ready_tx.send(Err("Shell_NotifyIconW(NIM_ADD) 실패".to_owned()));
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return Ok(());
        }

        show_balloon(hwnd, "ruster", "트레이 모드로 실행 중입니다.");
        let _ = ready_tx.send(Ok(hwnd.0 as isize));

        let mut msg = MSG::default();
        loop {
            let result = unsafe { GetMessageW(&mut msg, None, 0, 0).0 };
            match result {
                -1 | 0 => break,
                _ => unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                },
            }
        }
        remove_tray_icon(hwnd);
        Ok(())
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        w_param: WPARAM,
        l_param: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_NCCREATE => {
                let createstruct = l_param.0 as *const CREATESTRUCTW;
                if !createstruct.is_null() {
                    let state = unsafe { (*createstruct).lpCreateParams } as *mut TrayState;
                    if !state.is_null() {
                        unsafe {
                            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
                        }
                        return LRESULT(1);
                    }
                }
            }
            WM_TRAYICON => {
                match l_param.0 as u32 {
                    WM_RBUTTONUP => {
                        if show_tray_menu_window(hwnd).is_err() {
                            show_context_menu(hwnd);
                        }
                    }
                    WM_LBUTTONDBLCLK => handle_command(hwnd, CMD_DASHBOARD),
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_COMMAND => {
                handle_command(hwnd, (w_param.0 & 0xffff) as u16);
                return LRESULT(0);
            }
            WM_CLOSE => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
            WM_DESTROY => {
                remove_tray_icon(hwnd);
                unsafe {
                    PostQuitMessage(0);
                }
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut TrayState;
                if !ptr.is_null() {
                    unsafe {
                        let _ = Box::from_raw(ptr);
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    }
                }
            }
            _ => {}
        }

        unsafe { DefWindowProcW(hwnd, msg, w_param, l_param) }
    }

    unsafe extern "system" fn text_window_proc(
        hwnd: HWND,
        msg: u32,
        w_param: WPARAM,
        l_param: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_NCCREATE => {
                let createstruct = l_param.0 as *const CREATESTRUCTW;
                if !createstruct.is_null() {
                    let state = unsafe { (*createstruct).lpCreateParams } as *mut TextWindowState;
                    if !state.is_null() {
                        unsafe {
                            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
                        }
                        return LRESULT(1);
                    }
                }
            }
            WM_CREATE => {
                if let Some(state) = text_window_state_mut(hwnd) {
                    let body = HSTRING::from(&state.body);
                    let edit_style = WS_CHILD
                        | WS_VISIBLE
                        | WS_VSCROLL
                        | WS_HSCROLL
                        | WINDOW_STYLE(
                            (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL | ES_AUTOHSCROLL) as u32,
                        );
                    let edit_hwnd = unsafe {
                        CreateWindowExW(
                            WINDOW_EX_STYLE::default(),
                            w!("EDIT"),
                            &body,
                            edit_style,
                            0,
                            0,
                            0,
                            0,
                            Some(hwnd),
                            None,
                            None,
                            None,
                        )
                        .unwrap_or_default()
                    };
                    state.edit_hwnd = edit_hwnd;
                    if !edit_hwnd.0.is_null() {
                        let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
                        unsafe {
                            SendMessageW(
                                edit_hwnd,
                                WM_SETFONT,
                                Some(WPARAM(font.0 as usize)),
                                Some(LPARAM(1)),
                            );
                        }
                    }
                    resize_text_window(hwnd, state);
                    return LRESULT(0);
                }
            }
            WM_SIZE => {
                if let Some(state) = text_window_state_mut(hwnd) {
                    resize_text_window(hwnd, state);
                    return LRESULT(0);
                }
            }
            WM_CLOSE => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut TextWindowState;
                if !ptr.is_null() {
                    unsafe {
                        let _ = Box::from_raw(ptr);
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    }
                }
            }
            _ => {}
        }

        unsafe { DefWindowProcW(hwnd, msg, w_param, l_param) }
    }

    unsafe extern "system" fn tray_menu_window_proc(
        hwnd: HWND,
        msg: u32,
        w_param: WPARAM,
        l_param: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_NCCREATE => {
                let createstruct = l_param.0 as *const CREATESTRUCTW;
                if !createstruct.is_null() {
                    let state =
                        unsafe { (*createstruct).lpCreateParams } as *mut TrayMenuWindowState;
                    if !state.is_null() {
                        unsafe {
                            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
                        }
                        return LRESULT(1);
                    }
                }
            }
            WM_CREATE => {
                if let Some(state) = tray_menu_window_state_mut(hwnd) {
                    create_tray_menu_buttons(hwnd, state);
                    apply_window_frame(hwnd, state.theme.dark);
                    return LRESULT(0);
                }
            }
            WM_PAINT => {
                if let Some(state) = tray_menu_window_state_mut(hwnd) {
                    paint_tray_menu_window(hwnd, state);
                    return LRESULT(0);
                }
            }
            WM_ACTIVATE => {
                if (w_param.0 & 0xffff) as u32 == WA_INACTIVE {
                    close_tray_menu_window_once(hwnd);
                    return LRESULT(0);
                }
            }
            WM_COMMAND => {
                if let Some(state) = tray_menu_window_state_mut(hwnd) {
                    let command = (w_param.0 & 0xffff) as u16;
                    let owner = state.owner;
                    if close_tray_menu_window_once(hwnd) {
                        handle_command(owner, command);
                    }
                    return LRESULT(0);
                }
            }
            WM_CLOSE => {
                close_tray_menu_window_once(hwnd);
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                let ptr =
                    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut TrayMenuWindowState;
                if !ptr.is_null() {
                    unsafe {
                        let _ = Box::from_raw(ptr);
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    }
                }
            }
            _ => {}
        }

        unsafe { DefWindowProcW(hwnd, msg, w_param, l_param) }
    }

    unsafe extern "system" fn dashboard_window_proc(
        hwnd: HWND,
        msg: u32,
        w_param: WPARAM,
        l_param: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_NCCREATE => {
                let createstruct = l_param.0 as *const CREATESTRUCTW;
                if !createstruct.is_null() {
                    let state =
                        unsafe { (*createstruct).lpCreateParams } as *mut DashboardWindowState;
                    if !state.is_null() {
                        unsafe {
                            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
                        }
                        return LRESULT(1);
                    }
                }
            }
            WM_GETMINMAXINFO => {
                set_window_min_track_size(l_param, 560, 390);
                return LRESULT(0);
            }
            WM_CREATE => {
                if let Some(state) = dashboard_window_state_mut(hwnd) {
                    create_dashboard_buttons(hwnd, state);
                    apply_window_frame(hwnd, state.theme.dark);
                    return LRESULT(0);
                }
            }
            WM_SIZE => {
                if let Some(state) = dashboard_window_state_mut(hwnd) {
                    resize_dashboard_buttons(hwnd, state);
                    return LRESULT(0);
                }
            }
            WM_PAINT => {
                if let Some(state) = dashboard_window_state_mut(hwnd) {
                    paint_dashboard_window(hwnd, state);
                    return LRESULT(0);
                }
            }
            WM_COMMAND => {
                handle_dashboard_window_command(hwnd, (w_param.0 & 0xffff) as u16);
                return LRESULT(0);
            }
            WM_CLOSE => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                let ptr =
                    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut DashboardWindowState;
                if !ptr.is_null() {
                    unsafe {
                        let _ = Box::from_raw(ptr);
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    }
                }
            }
            _ => {}
        }

        unsafe { DefWindowProcW(hwnd, msg, w_param, l_param) }
    }

    unsafe extern "system" fn stats_window_proc(
        hwnd: HWND,
        msg: u32,
        w_param: WPARAM,
        l_param: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_NCCREATE => {
                let createstruct = l_param.0 as *const CREATESTRUCTW;
                if !createstruct.is_null() {
                    let state = unsafe { (*createstruct).lpCreateParams } as *mut StatsWindowState;
                    if !state.is_null() {
                        unsafe {
                            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
                        }
                        return LRESULT(1);
                    }
                }
            }
            WM_GETMINMAXINFO => {
                set_window_min_track_size(l_param, 640, 420);
                return LRESULT(0);
            }
            WM_CREATE => {
                if let Some(state) = stats_window_state_mut(hwnd) {
                    create_stats_controls(hwnd, state);
                    apply_window_frame(hwnd, state.theme.dark);
                    return LRESULT(0);
                }
            }
            WM_SIZE => {
                if let Some(state) = stats_window_state_mut(hwnd) {
                    resize_stats_controls(hwnd, state);
                    return LRESULT(0);
                }
            }
            WM_PAINT => {
                if let Some(state) = stats_window_state_mut(hwnd) {
                    paint_stats_window(hwnd, state);
                    return LRESULT(0);
                }
            }
            WM_COMMAND => {
                match (w_param.0 & 0xffff) as u16 {
                    CMD_REFRESH_WINDOW => {
                        if let Some(state) = stats_window_state_mut(hwnd) {
                            refresh_stats_window(state);
                            update_stats_detail_text(state);
                            unsafe {
                                let _ = InvalidateRect(Some(hwnd), None, true);
                            }
                        }
                    }
                    CMD_CLOSE_WINDOW => unsafe {
                        let _ = DestroyWindow(hwnd);
                    },
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                let ptr =
                    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut StatsWindowState;
                if !ptr.is_null() {
                    unsafe {
                        let _ = Box::from_raw(ptr);
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    }
                }
            }
            _ => {}
        }

        unsafe { DefWindowProcW(hwnd, msg, w_param, l_param) }
    }

    fn text_window_state_mut(hwnd: HWND) -> Option<&'static mut TextWindowState> {
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut TextWindowState;
        if ptr.is_null() {
            None
        } else {
            unsafe { ptr.as_mut() }
        }
    }

    fn close_tray_menu_window_once(hwnd: HWND) -> bool {
        let Some(state) = tray_menu_window_state_mut(hwnd) else {
            return false;
        };
        if state.close_requested {
            return false;
        }

        state.close_requested = true;
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        true
    }

    fn tray_menu_window_state_mut(hwnd: HWND) -> Option<&'static mut TrayMenuWindowState> {
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut TrayMenuWindowState;
        if ptr.is_null() {
            None
        } else {
            unsafe { ptr.as_mut() }
        }
    }

    fn dashboard_window_state_mut(hwnd: HWND) -> Option<&'static mut DashboardWindowState> {
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut DashboardWindowState;
        if ptr.is_null() {
            None
        } else {
            unsafe { ptr.as_mut() }
        }
    }

    fn stats_window_state_mut(hwnd: HWND) -> Option<&'static mut StatsWindowState> {
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut StatsWindowState;
        if ptr.is_null() {
            None
        } else {
            unsafe { ptr.as_mut() }
        }
    }

    fn resize_text_window(hwnd: HWND, state: &TextWindowState) {
        if state.edit_hwnd.0.is_null() {
            return;
        }
        let mut rect = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut rect) }.is_err() {
            return;
        }
        let margin = 12;
        let width = (rect.right - rect.left - margin * 2).max(120);
        let height = (rect.bottom - rect.top - margin * 2).max(80);
        unsafe {
            let _ = MoveWindow(state.edit_hwnd, margin, margin, width, height, true);
        }
    }

    fn state(hwnd: HWND) -> Option<&'static TrayState> {
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const TrayState;
        if ptr.is_null() {
            None
        } else {
            unsafe { ptr.as_ref() }
        }
    }

    fn add_tray_icon(hwnd: HWND) -> bool {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAYICON,
            ..Default::default()
        };
        data.hIcon = state(hwnd)
            .and_then(|state| state.tray_icon.as_ref().map(OwnedIcon::handle))
            .unwrap_or_else(|| unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() });
        copy_wide_fixed(&mut data.szTip, "ruster");
        unsafe { Shell_NotifyIconW(NIM_ADD, &data).as_bool() }
    }

    fn create_rtr_tray_icon() -> Option<OwnedIcon> {
        let width = unsafe { GetSystemMetrics(SM_CXSMICON) };
        let height = unsafe { GetSystemMetrics(SM_CYSMICON) };
        let size = if width > 0 && height > 0 {
            width.max(height)
        } else {
            32
        }
        .clamp(16, 64) as usize;
        let rgba = rtr_icon_rgba(size);
        let color_bitmap = create_argb_bitmap(size, &rgba)?;
        let mask_bitmap = create_empty_mask_bitmap(size);
        if mask_bitmap.is_invalid() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ::from(color_bitmap));
            }
            return None;
        }

        let icon_info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bitmap,
            hbmColor: color_bitmap,
        };
        let icon = unsafe { CreateIconIndirect(&icon_info) };
        unsafe {
            let _ = DeleteObject(HGDIOBJ::from(color_bitmap));
            let _ = DeleteObject(HGDIOBJ::from(mask_bitmap));
        }

        match icon {
            Ok(icon) if !icon.is_invalid() => Some(OwnedIcon(icon)),
            _ => None,
        }
    }

    fn create_argb_bitmap(size: usize, rgba: &[u8]) -> Option<HBITMAP> {
        let bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default()],
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = unsafe {
            CreateDIBSection(None, &bitmap_info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?
        };
        if bitmap.is_invalid() || bits.is_null() {
            if !bitmap.is_invalid() {
                unsafe {
                    let _ = DeleteObject(HGDIOBJ::from(bitmap));
                }
            }
            return None;
        }

        let bytes = size * size * 4;
        let dest = unsafe { std::slice::from_raw_parts_mut(bits as *mut u8, bytes) };
        for (source, target) in rgba.chunks_exact(4).zip(dest.chunks_exact_mut(4)) {
            target[0] = source[2];
            target[1] = source[1];
            target[2] = source[0];
            target[3] = source[3];
        }
        Some(bitmap)
    }

    fn create_empty_mask_bitmap(size: usize) -> HBITMAP {
        let stride = size.div_ceil(32) * 4;
        let mask_bits = vec![0u8; stride * size];
        unsafe {
            CreateBitmap(
                size as i32,
                size as i32,
                1,
                1,
                Some(mask_bits.as_ptr() as *const c_void),
            )
        }
    }

    fn remove_tray_icon(hwnd: HWND) {
        let data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ICON_ID,
            ..Default::default()
        };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }

    fn show_balloon(hwnd: HWND, title: &str, message: &str) {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ICON_ID,
            uFlags: NIF_INFO,
            dwInfoFlags: NIIF_INFO,
            ..Default::default()
        };
        copy_wide_fixed(&mut data.szInfoTitle, title);
        copy_wide_fixed(&mut data.szInfo, message);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }

    fn show_tray_menu_window(owner: HWND) -> Result<(), String> {
        let tray_state =
            state(owner).ok_or_else(|| "트레이 상태를 찾을 수 없습니다.".to_owned())?;
        let hinstance = current_hinstance()?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(tray_menu_window_proc),
            hInstance: hinstance,
            lpszClassName: w!("RusterTrayMenuPopup"),
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&class);
        }

        let mut point = POINT::default();
        unsafe {
            GetCursorPos(&mut point).map_err(|error| format!("{error}"))?;
        }
        let width = 260;
        let height = 300;
        let (x, y) = place_window_near_point(point, width, height, true);
        let state = Box::new(TrayMenuWindowState {
            owner,
            snapshot: build_tray_menu_snapshot(tray_state),
            theme: current_theme(tray_state),
            fonts: UiFonts::new(),
            buttons: Vec::new(),
            close_requested: false,
        });
        let state_ptr = Box::into_raw(state);
        let create_result = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                w!("RusterTrayMenuPopup"),
                w!("ruster"),
                WS_POPUP | WS_VISIBLE,
                x,
                y,
                width,
                height,
                Some(owner),
                None,
                Some(hinstance),
                Some(state_ptr as *const _),
            )
        };
        let hwnd = match create_result {
            Ok(hwnd) => hwnd,
            Err(error) => {
                unsafe {
                    let _ = Box::from_raw(state_ptr);
                }
                return Err(format!("{error}"));
            }
        };
        if hwnd.0.is_null() {
            unsafe {
                let _ = Box::from_raw(state_ptr);
            }
            return Err("커스텀 트레이 메뉴 창 생성 실패".to_owned());
        }

        unsafe {
            let _ = SetForegroundWindow(hwnd);
            let _ = UpdateWindow(hwnd);
        }
        Ok(())
    }

    fn show_dashboard_window(owner: HWND, tray_state: &TrayState) -> Result<(), String> {
        let hinstance = current_hinstance()?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(dashboard_window_proc),
            hInstance: hinstance,
            lpszClassName: w!("RusterTrayDashboardWindow"),
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&class);
        }

        let state = Box::new(DashboardWindowState {
            owner,
            data: build_dashboard_data(tray_state),
            notice: "트레이의 WebView 제어는 표시/숨김 토글 하나로 처리합니다.".to_owned(),
            theme: current_theme(tray_state),
            fonts: UiFonts::new(),
            buttons: Vec::new(),
        });
        let state_ptr = Box::into_raw(state);
        let create_result = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("RusterTrayDashboardWindow"),
                w!("ruster 대시보드"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                620,
                430,
                Some(owner),
                None,
                Some(hinstance),
                Some(state_ptr as *const _),
            )
        };
        let hwnd = match create_result {
            Ok(hwnd) => hwnd,
            Err(error) => {
                unsafe {
                    let _ = Box::from_raw(state_ptr);
                }
                return Err(format!("{error}"));
            }
        };
        if hwnd.0.is_null() {
            unsafe {
                let _ = Box::from_raw(state_ptr);
            }
            return Err("대시보드 창 생성 실패".to_owned());
        }

        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            let _ = UpdateWindow(hwnd);
            let _ = SetForegroundWindow(hwnd);
        }
        Ok(())
    }

    fn show_stats_window(owner: HWND, tray_state: &TrayState) -> Result<(), String> {
        let hinstance = current_hinstance()?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(stats_window_proc),
            hInstance: hinstance,
            lpszClassName: w!("RusterTrayStatsWindow"),
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&class);
        }

        let state = Box::new(StatsWindowState {
            owner,
            data: build_stats_data(tray_state),
            theme: current_theme(tray_state),
            edit_hwnd: HWND::default(),
            fonts: UiFonts::new(),
            buttons: Vec::new(),
        });
        let state_ptr = Box::into_raw(state);
        let create_result = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("RusterTrayStatsWindow"),
                w!("ruster 통계"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                700,
                500,
                Some(owner),
                None,
                Some(hinstance),
                Some(state_ptr as *const _),
            )
        };
        let hwnd = match create_result {
            Ok(hwnd) => hwnd,
            Err(error) => {
                unsafe {
                    let _ = Box::from_raw(state_ptr);
                }
                return Err(format!("{error}"));
            }
        };
        if hwnd.0.is_null() {
            unsafe {
                let _ = Box::from_raw(state_ptr);
            }
            return Err("통계 창 생성 실패".to_owned());
        }

        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            let _ = UpdateWindow(hwnd);
            let _ = SetForegroundWindow(hwnd);
        }
        Ok(())
    }

    fn current_hinstance() -> Result<HINSTANCE, String> {
        unsafe {
            LibraryLoader::GetModuleHandleW(None)
                .map(|module| HINSTANCE(module.0))
                .map_err(|error| format!("{error}"))
        }
    }

    fn current_theme(tray_state: &TrayState) -> TrayTheme {
        let settings = tray_state.settings.read();
        TrayTheme::from_settings(&settings)
    }

    fn windows_apps_use_dark_theme() -> bool {
        let subkey = wide_null(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
        let value_name = wide_null("AppsUseLightTheme");
        let mut value = 1u32;
        let mut byte_len = size_of::<u32>() as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                PCWSTR(value_name.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some(&mut value as *mut _ as *mut c_void),
                Some(&mut byte_len),
            )
        };
        status == ERROR_SUCCESS && value == 0
    }

    fn build_tray_menu_snapshot(tray_state: &TrayState) -> TrayMenuSnapshot {
        let usage = UsageMetrics::new(&tray_state.paths, tray_state.logs.clone()).snapshot();
        TrayMenuSnapshot {
            mode: tray_state.host.mode().label().to_owned(),
            requests: format_count(usage.total_requests),
            success_rate: format!("{:.1}%", usage.success_rate()),
            last_failure: empty_dash(&usage.last_failure).to_owned(),
        }
    }

    fn build_dashboard_data(tray_state: &TrayState) -> DashboardData {
        let usage = UsageMetrics::new(&tray_state.paths, tray_state.logs.clone()).snapshot();
        let settings = tray_state.settings.read().clone();
        let mode = tray_state.host.mode();
        let webview_status = if matches!(
            mode,
            TranslationMode::WebView | TranslationMode::ChatGptWebView
        ) {
            "표시/숨김 토글 가능"
        } else {
            "Gemini CLI 모드"
        };
        let runtime_status = tray_state
            .host
            .request_guard()
            .is_active()
            .map(|remaining| format!("Request Guard 활성 ({:.1}s)", remaining.as_secs_f32()))
            .unwrap_or_else(|| "번역 대기 중".to_owned());

        DashboardData {
            mode_label: mode.label().to_owned(),
            base_url: settings.base_url,
            raw_prompt: on_off(settings.raw_prompt_mode).to_owned(),
            webview_status: webview_status.to_owned(),
            runtime_status,
            total_requests: format_count(usage.total_requests),
            success_summary: format!(
                "{} ({:.1}%)",
                format_count(usage.succeeded_requests),
                usage.success_rate()
            ),
            failure_summary: format!(
                "{} / {}",
                format_count(usage.failed_requests),
                format_count(usage.cancelled_requests)
            ),
            token_summary: format!(
                "{} / {}",
                format_count(usage.input_tokens),
                format_count(usage.successful_output_tokens)
            ),
            provider_summary: format!(
                "Gemini {} / OpenAI {} / 호환 API {} / 기타 {}",
                format_count(usage.gemini_requests),
                format_count(usage.open_ai_requests),
                format_count(usage.mort_requests),
                format_count(usage.other_requests)
            ),
            started: empty_dash(&usage.started_at_local).to_owned(),
            updated: empty_dash(&usage.last_updated_at_local).to_owned(),
            last_failure: empty_dash(&usage.last_failure).to_owned(),
        }
    }

    fn build_stats_data(tray_state: &TrayState) -> StatsData {
        let metrics = UsageMetrics::new(&tray_state.paths, tray_state.logs.clone());
        let usage = metrics.snapshot();
        let buckets = metrics.buckets(UsageStatsPeriod::Daily);
        let bucket_lines: Vec<String> = buckets
            .iter()
            .map(|bucket| {
                format!(
                    "{}  요청 {}, 성공 {} ({:.1}%), 실패 {}, 취소 {}, 입력 {}, 출력 {}",
                    bucket.label,
                    format_count(bucket.total_requests),
                    format_count(bucket.succeeded_requests),
                    bucket.success_rate(),
                    format_count(bucket.failed_requests),
                    format_count(bucket.cancelled_requests),
                    format_count(bucket.input_tokens),
                    format_count(bucket.successful_output_tokens)
                )
            })
            .collect();
        let detail = [
            format!(
                "저장 위치: {}",
                tray_state.paths.usage_metrics_path().display()
            ),
            format!("집계 시작: {}", empty_dash(&usage.started_at_local)),
            format!(
                "최근 업데이트: {}",
                empty_dash(&usage.last_updated_at_local)
            ),
            format!("입력 문자: {}", format_count(usage.input_characters)),
            format!(
                "성공 출력 문자: {}",
                format_count(usage.successful_output_characters)
            ),
            format!("최근 실패: {}", empty_dash(&usage.last_failure)),
            String::new(),
            "최근 14일 일별 통계".to_owned(),
            if bucket_lines.is_empty() {
                "기간별 통계: -".to_owned()
            } else {
                bucket_lines.join("\n")
            },
        ]
        .join("\n");

        StatsData {
            total_requests: format_count(usage.total_requests),
            success_summary: format!(
                "{} ({:.1}%)",
                format_count(usage.succeeded_requests),
                usage.success_rate()
            ),
            failure_summary: format!(
                "{} / {}",
                format_count(usage.failed_requests),
                format_count(usage.cancelled_requests)
            ),
            token_summary: format!(
                "{} / {}",
                format_count(usage.input_tokens),
                format_count(usage.successful_output_tokens)
            ),
            provider_summary: format!(
                "Gemini {} / OpenAI {} / 호환 API {} / 기타 {}",
                format_count(usage.gemini_requests),
                format_count(usage.open_ai_requests),
                format_count(usage.mort_requests),
                format_count(usage.other_requests)
            ),
            updated_summary: format!(
                "집계 시작 {}  |  최근 업데이트 {}",
                empty_dash(&usage.started_at_local),
                empty_dash(&usage.last_updated_at_local)
            ),
            detail,
        }
    }

    fn refresh_dashboard_window(window_state: &mut DashboardWindowState) {
        if let Some(tray_state) = state(window_state.owner) {
            window_state.data = build_dashboard_data(tray_state);
            window_state.theme = current_theme(tray_state);
        }
    }

    fn refresh_stats_window(window_state: &mut StatsWindowState) {
        if let Some(tray_state) = state(window_state.owner) {
            window_state.data = build_stats_data(tray_state);
            window_state.theme = current_theme(tray_state);
        }
    }

    fn place_window_near_point(
        point: POINT,
        width: i32,
        height: i32,
        align_right: bool,
    ) -> (i32, i32) {
        let mut work = RECT {
            left: 0,
            top: 0,
            right: unsafe {
                GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SM_CXSCREEN)
            },
            bottom: unsafe {
                GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SM_CYSCREEN)
            },
        };
        let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        if !monitor.is_invalid() {
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
                work = info.rcWork;
            }
        }

        let mut x = if align_right {
            point.x - width + 8
        } else {
            point.x
        };
        let mut y = point.y - 8;
        if x < work.left {
            x = work.left;
        }
        if y < work.top {
            y = work.top;
        }
        if x + width > work.right {
            x = work.right - width;
        }
        if y + height > work.bottom {
            y = work.bottom - height;
        }
        (x.max(work.left), y.max(work.top))
    }

    fn create_tray_menu_buttons(hwnd: HWND, state: &mut TrayMenuWindowState) {
        let mut y = 86;
        for entry in TRAY_MENU {
            match *entry {
                TrayMenuEntry::Action { command, label } => {
                    let button = create_button(
                        hwnd,
                        command,
                        label,
                        ButtonBounds {
                            x: 12,
                            y,
                            width: 236,
                            height: 28,
                        },
                        &state.fonts.button,
                    );
                    if !button.0.is_null() {
                        state.buttons.push(button);
                    }
                    y += 30;
                }
                TrayMenuEntry::Separator => {
                    y += 9;
                }
            }
        }
    }

    fn create_dashboard_buttons(hwnd: HWND, state: &mut DashboardWindowState) {
        for (command, label, _) in dashboard_button_specs() {
            let button = create_button(
                hwnd,
                command,
                label,
                ButtonBounds {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
                &state.fonts.button,
            );
            if !button.0.is_null() {
                state.buttons.push(button);
            }
        }
        resize_dashboard_buttons(hwnd, state);
    }

    fn resize_dashboard_buttons(hwnd: HWND, state: &DashboardWindowState) {
        let mut rect = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut rect) }.is_err() {
            return;
        }
        let specs = dashboard_button_specs();
        let gap = 8;
        let total_width: i32 = specs.iter().map(|(_, _, width)| *width).sum::<i32>()
            + gap * (specs.len().saturating_sub(1) as i32);
        let mut x = (rect.right - 18 - total_width).max(18);
        let y = (rect.bottom - 42).max(318);
        for (index, (_, _, width)) in specs.iter().enumerate() {
            if let Some(button) = state.buttons.get(index) {
                unsafe {
                    let _ = MoveWindow(*button, x, y, *width, 30, true);
                }
            }
            x += *width + gap;
        }
    }

    fn dashboard_button_specs() -> [(u16, &'static str, i32); 6] {
        [
            (CMD_TOGGLE_WEBVIEW, "WebView", 82),
            (CMD_REQUEST_GUARD, "취소/복구", 82),
            (CMD_LOGS, "로그", 60),
            (CMD_STATS, "통계", 60),
            (CMD_CLOSE_WINDOW, "닫기", 60),
            (CMD_EXIT, "종료", 66),
        ]
    }

    fn handle_dashboard_window_command(hwnd: HWND, command: u16) {
        let Some(state) = dashboard_window_state_mut(hwnd) else {
            return;
        };
        match command {
            CMD_CLOSE_WINDOW => unsafe {
                let _ = DestroyWindow(hwnd);
            },
            CMD_TOGGLE_WEBVIEW | CMD_REQUEST_GUARD => {
                handle_command(state.owner, command);
                state.notice = match command {
                    CMD_TOGGLE_WEBVIEW => "WebView 표시/숨김 토글을 요청했습니다.".to_owned(),
                    _ => "현재 요청을 취소하고 WebView 세션 복구를 준비합니다.".to_owned(),
                };
                refresh_dashboard_window(state);
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
            }
            CMD_LOGS | CMD_STATS => handle_command(state.owner, command),
            CMD_EXIT => {
                handle_command(state.owner, command);
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
            _ => {}
        }
    }

    fn create_stats_controls(hwnd: HWND, state: &mut StatsWindowState) {
        let body = HSTRING::from(state.data.detail.replace('\n', "\r\n"));
        let edit_style = WS_CHILD
            | WS_VISIBLE
            | WS_VSCROLL
            | WS_HSCROLL
            | WINDOW_STYLE((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL | ES_AUTOHSCROLL) as u32);
        state.edit_hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("EDIT"),
                &body,
                edit_style,
                0,
                0,
                0,
                0,
                Some(hwnd),
                None,
                None,
                None,
            )
            .unwrap_or_default()
        };
        if !state.edit_hwnd.0.is_null() {
            set_control_font(state.edit_hwnd, &state.fonts.mono);
        }
        for (command, label, width) in [
            (CMD_REFRESH_WINDOW, "새로고침", 92),
            (CMD_CLOSE_WINDOW, "닫기", 78),
        ] {
            let button = create_button(
                hwnd,
                command,
                label,
                ButtonBounds {
                    x: 0,
                    y: 0,
                    width,
                    height: 30,
                },
                &state.fonts.button,
            );
            if !button.0.is_null() {
                state.buttons.push(button);
            }
        }
        resize_stats_controls(hwnd, state);
    }

    fn resize_stats_controls(hwnd: HWND, state: &StatsWindowState) {
        let mut rect = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut rect) }.is_err() {
            return;
        }
        let margin = 18;
        let edit_top = 246;
        let footer_y = (rect.bottom - 46).max(edit_top + 120);
        if !state.edit_hwnd.0.is_null() {
            unsafe {
                let _ = MoveWindow(
                    state.edit_hwnd,
                    margin,
                    edit_top,
                    (rect.right - margin * 2).max(220),
                    (footer_y - edit_top - 12).max(80),
                    true,
                );
            }
        }
        let mut x = rect.right - margin - 78;
        if let Some(close) = state.buttons.get(1) {
            unsafe {
                let _ = MoveWindow(*close, x, footer_y, 78, 30, true);
            }
        }
        x -= 8 + 92;
        if let Some(refresh) = state.buttons.first() {
            unsafe {
                let _ = MoveWindow(*refresh, x, footer_y, 92, 30, true);
            }
        }
    }

    fn update_stats_detail_text(state: &StatsWindowState) {
        if state.edit_hwnd.0.is_null() {
            return;
        }
        let detail = HSTRING::from(state.data.detail.replace('\n', "\r\n"));
        unsafe {
            let _ = SetWindowTextW(state.edit_hwnd, &detail);
        }
    }

    fn paint_tray_menu_window(hwnd: HWND, state: &TrayMenuWindowState) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
        let mut rect = RECT::default();
        let _ = unsafe { GetClientRect(hwnd, &mut rect) };
        let theme = state.theme;
        fill_rect(hdc, rect, theme.bg);
        draw_round_rect(hdc, inset(rect, 0, 0, 1, 1), theme.bg, theme.border, 12);

        draw_text(
            hdc,
            "RTR",
            RECT {
                left: 16,
                top: 10,
                right: 76,
                bottom: 40,
            },
            theme.accent,
            &state.fonts.title,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        draw_text(
            hdc,
            "ruster 트레이",
            RECT {
                left: 72,
                top: 11,
                right: rect.right - 14,
                bottom: 36,
            },
            theme.text,
            &state.fonts.body,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        draw_text(
            hdc,
            &format!(
                "{} | 요청 {} | 성공률 {}",
                state.snapshot.mode, state.snapshot.requests, state.snapshot.success_rate
            ),
            RECT {
                left: 16,
                top: 43,
                right: rect.right - 16,
                bottom: 63,
            },
            theme.muted,
            &state.fonts.small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        draw_text(
            hdc,
            &format!("최근 실패: {}", state.snapshot.last_failure),
            RECT {
                left: 16,
                top: 63,
                right: rect.right - 16,
                bottom: 83,
            },
            theme.muted_soft,
            &state.fonts.small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );

        let mut y = 86;
        for entry in TRAY_MENU {
            match entry {
                TrayMenuEntry::Action { .. } => y += 30,
                TrayMenuEntry::Separator => {
                    fill_rect(
                        hdc,
                        RECT {
                            left: 18,
                            top: y + 4,
                            right: rect.right - 18,
                            bottom: y + 5,
                        },
                        theme.border,
                    );
                    y += 9;
                }
            }
        }

        unsafe {
            let _ = EndPaint(hwnd, &ps);
        }
    }

    fn paint_dashboard_window(hwnd: HWND, state: &DashboardWindowState) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
        let mut rect = RECT::default();
        let _ = unsafe { GetClientRect(hwnd, &mut rect) };
        let theme = state.theme;
        fill_rect(hdc, rect, theme.bg);
        let width = rect.right - rect.left;
        let header = RECT {
            left: 18,
            top: 18,
            right: width - 18,
            bottom: 82,
        };
        draw_round_rect(hdc, header, theme.accent_soft, theme.border, 12);
        draw_text(
            hdc,
            "ruster",
            RECT {
                left: 34,
                top: 28,
                right: width - 38,
                bottom: 54,
            },
            theme.text,
            &state.fonts.title,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        draw_text(
            hdc,
            &format!("트레이 모드 실행 중 | {}", state.data.mode_label),
            RECT {
                left: 34,
                top: 54,
                right: width - 38,
                bottom: 74,
            },
            theme.muted,
            &state.fonts.small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );

        let card_gap = 10;
        let info = RECT {
            left: 18,
            top: 96,
            right: width - 18,
            bottom: 204,
        };
        draw_round_rect(hdc, info, theme.surface, theme.border, 12);
        draw_info_line(
            hdc,
            theme,
            &state.fonts,
            "Base URL",
            &state.data.base_url,
            RECT {
                left: 36,
                top: 112,
                right: width - 36,
                bottom: 132,
            },
        );
        draw_info_line(
            hdc,
            theme,
            &state.fonts,
            "Raw Prompt",
            &state.data.raw_prompt,
            RECT {
                left: 36,
                top: 134,
                right: width - 36,
                bottom: 154,
            },
        );
        draw_info_line(
            hdc,
            theme,
            &state.fonts,
            "WebView",
            &state.data.webview_status,
            RECT {
                left: 36,
                top: 156,
                right: width - 36,
                bottom: 176,
            },
        );
        draw_info_line(
            hdc,
            theme,
            &state.fonts,
            "상태",
            &state.data.runtime_status,
            RECT {
                left: 36,
                top: 178,
                right: width - 36,
                bottom: 198,
            },
        );

        let metric_top = 220;
        let metric_width = ((width - 36) - card_gap * 3) / 4;
        for (index, (label, value, emphasis)) in [
            ("총 요청", state.data.total_requests.as_str(), false),
            ("성공", state.data.success_summary.as_str(), true),
            ("실패 / 취소", state.data.failure_summary.as_str(), false),
            ("입력 / 출력 토큰", state.data.token_summary.as_str(), false),
        ]
        .iter()
        .enumerate()
        {
            let left = 18 + index as i32 * (metric_width + card_gap);
            draw_metric_tile(
                hdc,
                theme,
                RECT {
                    left,
                    top: metric_top,
                    right: left + metric_width,
                    bottom: metric_top + 64,
                },
                label,
                value,
                *emphasis,
                &state.fonts,
            );
        }

        draw_text(
            hdc,
            &state.data.provider_summary,
            RECT {
                left: 20,
                top: 296,
                right: width - 20,
                bottom: 316,
            },
            theme.muted,
            &state.fonts.small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        draw_text(
            hdc,
            &format!(
                "시작 {} | 최근 업데이트 {} | 최근 실패 {}",
                state.data.started, state.data.updated, state.data.last_failure
            ),
            RECT {
                left: 20,
                top: 316,
                right: width - 20,
                bottom: 336,
            },
            theme.muted_soft,
            &state.fonts.small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        draw_text(
            hdc,
            &state.notice,
            RECT {
                left: 20,
                top: 336,
                right: width - 20,
                bottom: 356,
            },
            theme.muted,
            &state.fonts.small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );

        unsafe {
            let _ = EndPaint(hwnd, &ps);
        }
    }

    fn paint_stats_window(hwnd: HWND, state: &StatsWindowState) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
        let mut rect = RECT::default();
        let _ = unsafe { GetClientRect(hwnd, &mut rect) };
        let theme = state.theme;
        fill_rect(hdc, rect, theme.bg);

        let width = rect.right - rect.left;
        draw_text(
            hdc,
            "요청 통계",
            RECT {
                left: 18,
                top: 18,
                right: width - 18,
                bottom: 48,
            },
            theme.text,
            &state.fonts.title,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );

        let gap = 10;
        let metric_top = 68;
        let metric_width = ((width - 36) - gap * 3) / 4;
        for (index, (label, value, emphasis)) in [
            ("총 요청", state.data.total_requests.as_str(), false),
            ("성공", state.data.success_summary.as_str(), true),
            ("실패 / 취소", state.data.failure_summary.as_str(), false),
            ("입력 / 출력 토큰", state.data.token_summary.as_str(), false),
        ]
        .iter()
        .enumerate()
        {
            let left = 18 + index as i32 * (metric_width + gap);
            draw_metric_tile(
                hdc,
                theme,
                RECT {
                    left,
                    top: metric_top,
                    right: left + metric_width,
                    bottom: metric_top + 78,
                },
                label,
                value,
                *emphasis,
                &state.fonts,
            );
        }

        let summary = RECT {
            left: 18,
            top: 164,
            right: width - 18,
            bottom: 226,
        };
        draw_round_rect(hdc, summary, theme.surface, theme.border, 12);
        draw_text(
            hdc,
            &state.data.provider_summary,
            RECT {
                left: 34,
                top: 174,
                right: width - 34,
                bottom: 196,
            },
            theme.muted,
            &state.fonts.body,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        draw_text(
            hdc,
            &state.data.updated_summary,
            RECT {
                left: 34,
                top: 198,
                right: width - 34,
                bottom: 218,
            },
            theme.muted_soft,
            &state.fonts.small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );

        unsafe {
            let _ = EndPaint(hwnd, &ps);
        }
    }

    fn draw_info_line(
        hdc: windows::Win32::Graphics::Gdi::HDC,
        theme: TrayTheme,
        fonts: &UiFonts,
        label: &str,
        value: &str,
        bounds: RECT,
    ) {
        draw_text(
            hdc,
            label,
            RECT {
                left: bounds.left,
                top: bounds.top,
                right: bounds.left + 130,
                bottom: bounds.bottom,
            },
            theme.muted,
            &fonts.small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        draw_text(
            hdc,
            value,
            RECT {
                left: bounds.left + 138,
                top: bounds.top,
                right: bounds.right,
                bottom: bounds.bottom,
            },
            theme.text,
            &fonts.small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
    }

    fn draw_metric_tile(
        hdc: windows::Win32::Graphics::Gdi::HDC,
        theme: TrayTheme,
        rect: RECT,
        label: &str,
        value: &str,
        emphasis: bool,
        fonts: &UiFonts,
    ) {
        draw_round_rect(hdc, rect, theme.surface, theme.border, 12);
        draw_text(
            hdc,
            label,
            RECT {
                left: rect.left + 12,
                top: rect.top + 10,
                right: rect.right - 12,
                bottom: rect.top + 30,
            },
            theme.muted_soft,
            &fonts.small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        draw_text(
            hdc,
            value,
            RECT {
                left: rect.left + 12,
                top: rect.top + 34,
                right: rect.right - 12,
                bottom: rect.bottom - 10,
            },
            if emphasis { theme.success } else { theme.text },
            &fonts.body,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
    }

    fn create_button(
        parent: HWND,
        id: u16,
        label: &str,
        bounds: ButtonBounds,
        font: &OwnedFont,
    ) -> HWND {
        let text = HSTRING::from(label);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("BUTTON"),
                &text,
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE((BS_PUSHBUTTON | BS_FLAT) as u32),
                bounds.x,
                bounds.y,
                bounds.width,
                bounds.height,
                Some(parent),
                Some(HMENU(id as usize as *mut c_void)),
                None,
                None,
            )
            .unwrap_or_default()
        };
        if !hwnd.0.is_null() {
            set_control_font(hwnd, font);
        }
        hwnd
    }

    fn set_control_font(hwnd: HWND, font: &OwnedFont) {
        if !font.is_valid() {
            return;
        }
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETFONT,
                Some(WPARAM(font.handle().0 as usize)),
                Some(LPARAM(1)),
            );
        }
    }

    fn create_font(height: i32, weight: i32, mono: bool) -> OwnedFont {
        let face = if mono {
            w!("Consolas")
        } else {
            w!("Malgun Gothic")
        };
        let pitch_and_family = u32::from(DEFAULT_PITCH.0 | FF_DONTCARE.0);
        let font = unsafe {
            CreateFontW(
                -height,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                pitch_and_family,
                face,
            )
        };
        OwnedFont(font)
    }

    fn fill_rect(hdc: windows::Win32::Graphics::Gdi::HDC, rect: RECT, fill: COLORREF) {
        let brush = unsafe { CreateSolidBrush(fill) };
        unsafe {
            let _ = FillRect(hdc, &rect, brush);
            let _ = DeleteObject(HGDIOBJ(brush.0));
        }
    }

    fn draw_round_rect(
        hdc: windows::Win32::Graphics::Gdi::HDC,
        rect: RECT,
        fill: COLORREF,
        stroke: COLORREF,
        radius: i32,
    ) {
        let brush = unsafe { CreateSolidBrush(fill) };
        let pen: HPEN = unsafe { CreatePen(PS_SOLID, 1, stroke) };
        unsafe {
            let old_brush = SelectObject(hdc, HGDIOBJ(brush.0));
            let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
            let _ = RoundRect(
                hdc,
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                radius,
                radius,
            );
            if !old_pen.0.is_null() {
                let _ = SelectObject(hdc, old_pen);
            }
            if !old_brush.0.is_null() {
                let _ = SelectObject(hdc, old_brush);
            }
            let _ = DeleteObject(HGDIOBJ(pen.0));
            let _ = DeleteObject(HGDIOBJ(brush.0));
        }
    }

    fn draw_text(
        hdc: windows::Win32::Graphics::Gdi::HDC,
        text: &str,
        mut rect: RECT,
        color_ref: COLORREF,
        font: &OwnedFont,
        format: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
    ) {
        if text.is_empty() {
            return;
        }
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let _ = SetBkMode(hdc, TRANSPARENT);
            let _ = SetTextColor(hdc, color_ref);
            let old_font = if font.is_valid() {
                SelectObject(hdc, HGDIOBJ(font.handle().0))
            } else {
                HGDIOBJ::default()
            };
            let _ = DrawTextW(hdc, &mut wide, &mut rect, format);
            if !old_font.0.is_null() {
                let _ = SelectObject(hdc, old_font);
            }
        }
    }

    fn inset(mut rect: RECT, left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        rect.left += left;
        rect.top += top;
        rect.right -= right;
        rect.bottom -= bottom;
        rect
    }

    fn color(red: u8, green: u8, blue: u8) -> COLORREF {
        COLORREF(u32::from(red) | (u32::from(green) << 8) | (u32::from(blue) << 16))
    }

    fn apply_window_frame(hwnd: HWND, dark: bool) {
        let corner = DWMWCP_ROUND.0;
        let dark_value: i32 = i32::from(dark);
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark_value as *const _ as *const c_void,
                size_of::<i32>() as u32,
            );
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const _ as *const c_void,
                size_of::<i32>() as u32,
            );
        }
    }

    fn set_window_min_track_size(l_param: LPARAM, width: i32, height: i32) {
        let info = l_param.0 as *mut MINMAXINFO;
        if info.is_null() {
            return;
        }
        unsafe {
            (*info).ptMinTrackSize.x = width;
            (*info).ptMinTrackSize.y = height;
        }
    }

    fn show_context_menu(hwnd: HWND) {
        let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
            return;
        };

        for entry in TRAY_MENU {
            match *entry {
                TrayMenuEntry::Action { command, label } => {
                    append_menu(menu, MF_STRING, usize::from(command), label);
                }
                TrayMenuEntry::Separator => {
                    append_menu(menu, MF_SEPARATOR, 0, "");
                }
            }
        }

        let mut point = POINT::default();
        if unsafe { GetCursorPos(&mut point) }.is_ok() {
            unsafe {
                let _ = SetForegroundWindow(hwnd);
            }
            let flags = TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY;
            let selected =
                unsafe { TrackPopupMenu(menu, flags, point.x, point.y, None, hwnd, None) };
            if selected.0 != 0 {
                handle_command(hwnd, selected.0 as u16);
            }
        }

        unsafe {
            let _ = DestroyMenu(menu);
        }
    }

    fn append_menu(
        menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
        flags: MENU_ITEM_FLAGS,
        id: usize,
        label: &str,
    ) {
        let text = HSTRING::from(label);
        unsafe {
            let _ = AppendMenuW(menu, flags, id, &text);
        }
    }

    fn handle_command(hwnd: HWND, command: u16) {
        let Some(state) = state(hwnd) else {
            return;
        };

        match command {
            CMD_DASHBOARD => show_dashboard(hwnd, state),
            CMD_TOGGLE_WEBVIEW => {
                let host = state.host.clone();
                let logs = state.logs.clone();
                state.runtime.spawn(async move {
                    let visible = host.toggle_webview_visibility().await;
                    logs.push(format!("[Tray] WebView 표시/숨김 토글 결과: {visible}"));
                });
            }
            CMD_REQUEST_GUARD => {
                let host = state.host.clone();
                let logs = state.logs.clone();
                state.runtime.spawn(async move {
                    let remaining = host.activate_request_guard_with_recovery("tray").await;
                    logs.push(format!(
                        "[Tray] 요청 취소/복구 실행 ({:.1}s 차단)",
                        remaining.as_secs_f32()
                    ));
                });
            }
            CMD_LOGS => {
                show_message(hwnd, "ruster 로그", &build_log_text(&state.logs.recent(80)));
            }
            CMD_STATS => {
                let usage = UsageMetrics::new(&state.paths, state.logs.clone()).snapshot();
                if show_stats_window(hwnd, state).is_err() {
                    show_message(hwnd, "ruster 통계", &build_stats_text(&usage));
                }
            }
            CMD_EXIT => {
                state.logs.push("[Tray] 프로그램 종료 요청");
                send_shutdown(&state.shutdown);
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
            _ => {}
        }
    }

    fn show_dashboard(hwnd: HWND, state: &TrayState) {
        let usage = UsageMetrics::new(&state.paths, state.logs.clone()).snapshot();
        let settings = state.settings.read().clone();
        let text = build_dashboard_text(state.host.mode(), &settings, &usage);
        if show_dashboard_window(hwnd, state).is_err() {
            show_message(hwnd, "ruster 대시보드", &text);
        }
    }

    fn show_message(hwnd: HWND, title: &str, body: &str) {
        let dark = state(hwnd)
            .map(|tray_state| current_theme(tray_state).dark)
            .unwrap_or_else(windows_apps_use_dark_theme);
        if show_text_window(title, body, dark).is_ok() {
            return;
        }

        let title = HSTRING::from(title);
        let body = HSTRING::from(body);
        unsafe {
            let _ = MessageBoxW(Some(hwnd), &body, &title, MB_OK | MB_ICONINFORMATION);
        }
    }

    fn show_text_window(title: &str, body: &str, dark: bool) -> Result<(), String> {
        let hinstance = unsafe {
            LibraryLoader::GetModuleHandleW(None)
                .map(|module| HINSTANCE(module.0))
                .map_err(|error| format!("{error}"))?
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(text_window_proc),
            hInstance: hinstance,
            lpszClassName: w!("RusterTrayTextWindow"),
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&class);
        }

        let state = Box::new(TextWindowState {
            body: body.replace('\n', "\r\n"),
            edit_hwnd: HWND::default(),
        });
        let state_ptr = Box::into_raw(state);
        let title = HSTRING::from(title);
        let create_result = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("RusterTrayTextWindow"),
                &title,
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                640,
                440,
                None,
                None,
                Some(hinstance),
                Some(state_ptr as *const _),
            )
        };
        let hwnd = match create_result {
            Ok(hwnd) => hwnd,
            Err(error) => {
                unsafe {
                    let _ = Box::from_raw(state_ptr);
                }
                return Err(format!("{error}"));
            }
        };
        if hwnd.0.is_null() {
            unsafe {
                let _ = Box::from_raw(state_ptr);
            }
            return Err("텍스트 창 생성 실패".to_owned());
        }

        unsafe {
            apply_window_frame(hwnd, dark);
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            let _ = UpdateWindow(hwnd);
            let _ = SetForegroundWindow(hwnd);
        }
        Ok(())
    }

    fn copy_wide_fixed<const N: usize>(dest: &mut [u16; N], value: &str) {
        dest.fill(0);
        for (slot, unit) in dest
            .iter_mut()
            .take(N.saturating_sub(1))
            .zip(value.encode_utf16())
        {
            *slot = unit;
        }
    }

    fn wide_null(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub struct TrayHandle;

    pub fn spawn(
        _paths: AppPaths,
        _settings: Arc<RwLock<AppSettings>>,
        _host: Arc<TranslatorHost>,
        logs: LogBuffer,
        _runtime: Handle,
        _shutdown: ShutdownSignal,
    ) -> Option<TrayHandle> {
        logs.push("[Tray] Windows 전용 기능이라 이 플랫폼에서는 트레이를 시작하지 않습니다.");
        None
    }
}

pub(crate) use platform::spawn;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_menu_covers_ruster_native_tray_actions() {
        let labels: Vec<&str> = TRAY_MENU
            .iter()
            .filter_map(|entry| match entry {
                TrayMenuEntry::Action { label, .. } => Some(*label),
                TrayMenuEntry::Separator => None,
            })
            .collect();

        assert!(labels.contains(&"로그 창 띄우기"));
        assert!(labels.contains(&"통계 보기"));
        assert!(labels.contains(&"프로그램 종료"));
        assert!(labels.contains(&"대시보드"));
        assert!(labels.contains(&"WebView 표시/숨김"));
        assert!(!labels.contains(&"WebView 다시 불러오기"));
    }

    #[test]
    fn tray_dashboard_text_contains_runtime_summary_fields() {
        let settings = AppSettings {
            base_url: "http://localhost:5000".to_owned(),
            raw_prompt_mode: true,
            ..Default::default()
        };
        let usage = UsageSnapshot {
            total_requests: 3,
            succeeded_requests: 2,
            failed_requests: 1,
            started_at_local: "2026-05-28 10:00:00".to_owned(),
            ..Default::default()
        };

        let text = build_dashboard_text(TranslationMode::WebView, &settings, &usage);

        assert!(text.contains("Mode:"));
        assert!(text.contains("Base URL: http://localhost:5000"));
        assert!(text.contains("Raw Prompt: On"));
        assert!(text.contains("WebView visibility:"));
        assert!(text.contains("Status: 실행 중"));
    }

    #[test]
    fn tray_log_text_uses_ruster_empty_state() {
        assert_eq!(build_log_text(&[]), "최근 로그가 없습니다.");
        assert_eq!(
            build_log_text(&["one".to_owned(), "two".to_owned()]),
            "one\ntwo"
        );
    }
}
