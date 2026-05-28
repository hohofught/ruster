use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_BROWSING_DATA_KINDS_CACHE_STORAGE, COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE,
    COREWEBVIEW2_BROWSING_DATA_KINDS_FILE_SYSTEMS, COREWEBVIEW2_BROWSING_DATA_KINDS_INDEXED_DB,
    COREWEBVIEW2_BROWSING_DATA_KINDS_LOCAL_STORAGE, COREWEBVIEW2_PROCESS_FAILED_KIND,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL, CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2,
    ICoreWebView2_13, ICoreWebView2Controller, ICoreWebView2Environment,
    ICoreWebView2EnvironmentOptions, ICoreWebView2Profile2,
};
use webview2_com::{
    AddScriptToExecuteOnDocumentCreatedCompletedHandler, ClearBrowsingDataCompletedHandler,
    CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, ExecuteScriptCompletedHandler,
    NavigationCompletedEventHandler, ProcessFailedEventHandler, WebResourceRequestedEventHandler,
};
use windows::Win32::Foundation::{
    COLORREF, CloseHandle, E_POINTER, E_UNEXPECTED, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader;
use windows::Win32::System::Threading;
use windows::Win32::UI::HiDpi::{PROCESS_PER_MONITOR_DPI_AWARE, SetProcessDpiAwareness};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWL_EXSTYLE, GWLP_USERDATA, GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowRect,
    LWA_ALPHA, MSG, PostQuitMessage, PostThreadMessageW, RegisterClassW, SC_MINIMIZE, SC_RESTORE,
    SET_WINDOW_POS_FLAGS, SW_RESTORE, SW_SHOWNA, SW_SHOWNORMAL, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetLayeredWindowAttributes,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, WA_INACTIVE, WINDOW_EX_STYLE,
    WM_ACTIVATE, WM_APP, WM_CLOSE, WM_DESTROY, WM_NCCREATE, WM_NCDESTROY, WM_SIZE, WM_SYSCOMMAND,
    WNDCLASSW, WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{Error as WindowsError, HSTRING, Interface, PCWSTR, w};

use crate::logging::LogBuffer;

type NativeResult<T> = Result<T, String>;

pub struct NativeWebView2Session {
    tx: mpsc::Sender<NativeCommand>,
    thread_id: u32,
    shared: Arc<NativeWebView2SharedState>,
}

#[derive(Clone, Debug)]
pub struct NativeWebView2Health {
    pub browser_process_id: u32,
    pub browser_process_alive: bool,
    pub process_failed: bool,
    pub critical_process_failed: bool,
    pub process_failed_kind: Option<i32>,
    pub navigation_completed_count: u64,
    pub last_navigation_succeeded: bool,
    pub native_request_guard_hits: u64,
}

impl NativeWebView2Health {
    pub fn is_operational(&self) -> bool {
        !self.critical_process_failed
            && (self.browser_process_id == 0 || self.browser_process_alive)
            && self.last_navigation_succeeded
    }

    pub fn summary(&self) -> String {
        let kind = self
            .process_failed_kind
            .map(format_webview2_process_kind)
            .unwrap_or("none");
        format!(
            "pid={}, alive={}, process_failed={}, critical={}, kind={}, navs={}, nav_ok={}, guard_hits={}",
            self.browser_process_id,
            self.browser_process_alive,
            self.process_failed,
            self.critical_process_failed,
            kind,
            self.navigation_completed_count,
            self.last_navigation_succeeded,
            self.native_request_guard_hits
        )
    }
}

impl NativeWebView2Session {
    pub fn launch(
        label: &str,
        url: &str,
        profile_dir: PathBuf,
        init_script: &str,
        logs: LogBuffer,
    ) -> NativeResult<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let label = label.to_owned();
        let url = url.to_owned();
        let init_script = init_script.to_owned();
        let shared = Arc::new(NativeWebView2SharedState::default());
        let thread_shared = shared.clone();
        let thread_name = format!("ruster-webview2-{}", label.replace(' ', "-"));

        thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                run_webview2_thread(WebView2ThreadConfig {
                    label,
                    url,
                    profile_dir,
                    init_script,
                    logs,
                    rx,
                    ready_tx,
                    shared: thread_shared,
                });
            })
            .map_err(|error| format!("WebView2 스레드 시작 실패: {error}"))?;

        let thread_id = ready_rx
            .recv_timeout(Duration::from_secs(45))
            .map_err(|_| "WebView2 초기화 시간이 초과되었습니다.".to_owned())??;

        Ok(Self {
            tx,
            thread_id,
            shared,
        })
    }

    pub async fn evaluate(&self, script: &str) -> NativeResult<Value> {
        self.request(|reply| NativeCommand::Evaluate(script.to_owned(), reply))
            .await
    }

    pub async fn navigate(&self, url: &str) -> NativeResult<()> {
        self.request(|reply| NativeCommand::Navigate(url.to_owned(), reply))
            .await
    }

    pub async fn stop_loading(&self) -> NativeResult<()> {
        self.request(NativeCommand::StopLoading).await
    }

    pub async fn bring_to_front(&self) -> NativeResult<()> {
        self.request(NativeCommand::BringToFront).await
    }

    pub async fn show_window(&self) -> NativeResult<bool> {
        self.request(NativeCommand::ShowWindow).await
    }

    pub async fn hide_window(&self) -> NativeResult<bool> {
        self.request(NativeCommand::HideWindow).await
    }

    pub async fn toggle_window(&self) -> NativeResult<bool> {
        self.request(NativeCommand::ToggleWindow).await
    }

    pub async fn health(&self) -> NativeResult<NativeWebView2Health> {
        self.request(NativeCommand::Health).await
    }

    pub async fn clear_cache(&self) -> NativeResult<()> {
        self.request(NativeCommand::ClearCache).await
    }

    pub async fn activate_request_guard(&self, until_unix_ms: u64) -> NativeResult<()> {
        self.shared
            .request_guard_until_ms
            .store(until_unix_ms, Ordering::SeqCst);
        self.request(move |reply| NativeCommand::ActivateRequestGuard(until_unix_ms, reply))
            .await
    }

    async fn request<T, F>(&self, build: F) -> NativeResult<T>
    where
        T: Send + 'static,
        F: FnOnce(mpsc::Sender<NativeResult<T>>) -> NativeCommand,
    {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(build(reply_tx))
            .map_err(|_| "WebView2 명령 채널이 종료되었습니다.".to_owned())?;
        self.poke();

        tokio::task::spawn_blocking(move || {
            reply_rx
                .recv()
                .map_err(|_| "WebView2 응답 채널이 종료되었습니다.".to_owned())?
        })
        .await
        .map_err(|error| format!("WebView2 응답 대기 실패: {error}"))?
    }

    fn poke(&self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_APP, WPARAM(0), LPARAM(0));
        }
    }
}

impl Drop for NativeWebView2Session {
    fn drop(&mut self) {
        let _ = self.tx.send(NativeCommand::Shutdown);
        self.poke();
    }
}

enum NativeCommand {
    Evaluate(String, mpsc::Sender<NativeResult<Value>>),
    Navigate(String, mpsc::Sender<NativeResult<()>>),
    StopLoading(mpsc::Sender<NativeResult<()>>),
    BringToFront(mpsc::Sender<NativeResult<()>>),
    ShowWindow(mpsc::Sender<NativeResult<bool>>),
    HideWindow(mpsc::Sender<NativeResult<bool>>),
    ToggleWindow(mpsc::Sender<NativeResult<bool>>),
    Health(mpsc::Sender<NativeResult<NativeWebView2Health>>),
    ClearCache(mpsc::Sender<NativeResult<()>>),
    ActivateRequestGuard(u64, mpsc::Sender<NativeResult<()>>),
    Shutdown,
}

struct NativeWebView2SharedState {
    browser_process_id: AtomicU32,
    browser_process_alive: AtomicBool,
    process_failed: AtomicBool,
    critical_process_failed: AtomicBool,
    process_failed_kind: AtomicI32,
    navigation_completed_count: AtomicU64,
    last_navigation_succeeded: AtomicBool,
    request_guard_until_ms: AtomicU64,
    native_request_guard_hits: AtomicU64,
}

impl Default for NativeWebView2SharedState {
    fn default() -> Self {
        Self {
            browser_process_id: AtomicU32::new(0),
            browser_process_alive: AtomicBool::new(false),
            process_failed: AtomicBool::new(false),
            critical_process_failed: AtomicBool::new(false),
            process_failed_kind: AtomicI32::new(-1),
            navigation_completed_count: AtomicU64::new(0),
            last_navigation_succeeded: AtomicBool::new(true),
            request_guard_until_ms: AtomicU64::new(0),
            native_request_guard_hits: AtomicU64::new(0),
        }
    }
}

struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

struct WindowState {
    controller: Option<ICoreWebView2Controller>,
    user_requested_visible: bool,
    pseudo_minimized_by_user: bool,
    last_visible_bounds: Option<RECT>,
}

struct WebView2ThreadConfig {
    label: String,
    url: String,
    profile_dir: PathBuf,
    init_script: String,
    logs: LogBuffer,
    rx: mpsc::Receiver<NativeCommand>,
    ready_tx: mpsc::Sender<NativeResult<u32>>,
    shared: Arc<NativeWebView2SharedState>,
}

fn run_webview2_thread(config: WebView2ThreadConfig) {
    let WebView2ThreadConfig {
        label,
        url,
        profile_dir,
        init_script,
        logs,
        rx,
        ready_tx,
        shared,
    } = config;

    let setup = unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(format_windows_error)
            .map(|_| ComApartment)
    }
    .and_then(|com| {
        let _com = com;
        let thread_id = unsafe { Threading::GetCurrentThreadId() };
        let _ = unsafe { SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE) };
        std::fs::create_dir_all(&profile_dir)
            .map_err(|error| format!("WebView2 profile directory 생성 실패: {error}"))?;

        logs.push(format!(
            "[WebView2] {label} 네이티브 WebView2 시작 (profile={})",
            profile_dir.display()
        ));

        let hwnd = create_frame_window(&format!("ruster - {label}"))?;
        let environment = create_environment(&profile_dir)?;
        let controller = create_controller(hwnd, &environment)?;
        let webview = unsafe { controller.CoreWebView2().map_err(format_windows_error)? };
        set_controller(hwnd, controller.clone());
        configure_webview(&webview);
        let _ = update_browser_process_snapshot(&webview, &shared);
        install_webview_event_hooks(&webview, &environment, &logs, shared.clone());
        resize_controller_to_window(hwnd, &controller);
        unsafe {
            controller
                .SetIsVisible(true)
                .map_err(format_windows_error)?;
            let _ = Gdi::UpdateWindow(hwnd);
        }
        apply_offscreen_background_state(hwnd, false);

        add_init_script(&webview, &init_script)?;
        navigate_webview(&webview, &url)?;
        ready_tx
            .send(Ok(thread_id))
            .map_err(|_| "WebView2 초기화 결과 전송 실패".to_owned())?;
        message_loop(hwnd, webview, controller, rx, shared);
        Ok::<(), String>(())
    });

    if let Err(error) = setup {
        let _ = ready_tx.send(Err::<u32, String>(error.clone()));
        logs.push(format!("[WebView2] 초기화/실행 오류: {error}"));
    }
}

fn create_environment(profile_dir: &Path) -> NativeResult<ICoreWebView2Environment> {
    let (tx, rx) = mpsc::channel();
    let options = CoreWebView2EnvironmentOptions::default();
    unsafe {
        options.set_additional_browser_arguments(
            "--disable-features=Translate --disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows".to_owned(),
        );
        options.set_language("ko-KR".to_owned());
    }

    let data_dir = HSTRING::from(profile_dir.to_string_lossy().as_ref());
    let options = ICoreWebView2EnvironmentOptions::from(options);
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |error_code: windows::core::Result<()>,
              environment: Option<ICoreWebView2Environment>| {
            error_code?;
            tx.send(environment.ok_or_else(|| WindowsError::from(E_POINTER)))
                .map_err(|_| WindowsError::from(E_UNEXPECTED))
        },
    ));

    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &data_dir, &options, &handler)
            .map_err(format_windows_error)?;
    }

    webview2_com::wait_with_pump(rx)
        .map_err(format_webview2_error)?
        .map_err(format_windows_error)
}

fn create_controller(
    hwnd: HWND,
    environment: &ICoreWebView2Environment,
) -> NativeResult<ICoreWebView2Controller> {
    let (tx, rx) = mpsc::channel();
    let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
        move |error_code: windows::core::Result<()>,
              controller: Option<ICoreWebView2Controller>| {
            error_code?;
            tx.send(controller.ok_or_else(|| WindowsError::from(E_POINTER)))
                .map_err(|_| WindowsError::from(E_UNEXPECTED))
        },
    ));

    unsafe {
        environment
            .CreateCoreWebView2Controller(hwnd, &handler)
            .map_err(format_windows_error)?;
    }

    webview2_com::wait_with_pump(rx)
        .map_err(format_webview2_error)?
        .map_err(format_windows_error)
}

fn configure_webview(webview: &ICoreWebView2) {
    if let Ok(settings) = unsafe { webview.Settings() } {
        unsafe {
            let _ = settings.SetAreDevToolsEnabled(true);
            let _ = settings.SetAreDefaultContextMenusEnabled(true);
            let _ = settings.SetIsStatusBarEnabled(false);
        }
    }
}

fn install_webview_event_hooks(
    webview: &ICoreWebView2,
    environment: &ICoreWebView2Environment,
    logs: &LogBuffer,
    shared: Arc<NativeWebView2SharedState>,
) {
    let mut token = 0;
    let nav_shared = shared.clone();
    let nav_logs = logs.clone();
    let nav_result = unsafe {
        webview.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |_sender, args| {
                let mut success: windows::core::BOOL = true.into();
                if let Some(args) = args {
                    let _ = args.IsSuccess(&mut success as *mut _);
                }
                let success = success.as_bool();
                nav_shared
                    .navigation_completed_count
                    .fetch_add(1, Ordering::SeqCst);
                nav_shared
                    .last_navigation_succeeded
                    .store(success, Ordering::SeqCst);
                nav_logs.push(format!("[WebView2] NavigationCompleted success={success}"));
                Ok(())
            })),
            &mut token,
        )
    };
    if let Err(error) = nav_result {
        logs.push(format!(
            "[WebView2] NavigationCompleted hook 등록 실패: {}",
            format_windows_error(error)
        ));
    }

    let failed_shared = shared.clone();
    let failed_logs = logs.clone();
    let failed_result = unsafe {
        webview.add_ProcessFailed(
            &ProcessFailedEventHandler::create(Box::new(move |_sender, args| {
                let kind = args
                    .and_then(|args| {
                        let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND(0);
                        args.ProcessFailedKind(&mut kind).ok()?;
                        Some(kind.0)
                    })
                    .unwrap_or(-1);
                let critical = matches!(kind, 0..=3);
                failed_shared.process_failed.store(true, Ordering::SeqCst);
                failed_shared
                    .critical_process_failed
                    .store(critical, Ordering::SeqCst);
                failed_shared
                    .process_failed_kind
                    .store(kind, Ordering::SeqCst);
                failed_logs.push(format!(
                    "[WebView2] ProcessFailed kind={} critical={}",
                    format_webview2_process_kind(kind),
                    critical
                ));
                Ok(())
            })),
            &mut token,
        )
    };
    if let Err(error) = failed_result {
        logs.push(format!(
            "[WebView2] ProcessFailed hook 등록 실패: {}",
            format_windows_error(error)
        ));
    }

    let filter = HSTRING::from("*");
    if let Err(error) = unsafe {
        webview.AddWebResourceRequestedFilter(&filter, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)
    } {
        logs.push(format!(
            "[WebView2] WebResourceRequested filter 등록 실패: {}",
            format_windows_error(error)
        ));
        return;
    }

    let env = environment.clone();
    let guard_shared = shared;
    let guard_result = unsafe {
        webview.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_sender, args| {
                if guard_shared.request_guard_until_ms.load(Ordering::SeqCst) <= now_unix_ms() {
                    return Ok(());
                }

                guard_shared
                    .native_request_guard_hits
                    .fetch_add(1, Ordering::SeqCst);
                if let Some(args) = args {
                    let status = HSTRING::from("Service Unavailable");
                    let headers =
                        HSTRING::from("Content-Type: text/plain\r\nCache-Control: no-store\r\n");
                    let response = env.CreateWebResourceResponse(None, 503, &status, &headers)?;
                    args.SetResponse(&response)?;
                }
                Ok(())
            })),
            &mut token,
        )
    };
    if let Err(error) = guard_result {
        logs.push(format!(
            "[WebView2] WebResourceRequested hook 등록 실패: {}",
            format_windows_error(error)
        ));
    }
}

fn add_init_script(webview: &ICoreWebView2, script: &str) -> NativeResult<()> {
    let webview = webview.clone();
    let script = HSTRING::from(script);
    AddScriptToExecuteOnDocumentCreatedCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            webview
                .AddScriptToExecuteOnDocumentCreated(&script, &handler)
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(|error_code, _id| error_code),
    )
    .map_err(format_webview2_error)?;
    Ok(())
}

fn navigate_webview(webview: &ICoreWebView2, url: &str) -> NativeResult<()> {
    let url = HSTRING::from(url);
    unsafe { webview.Navigate(&url).map_err(format_windows_error) }
}

fn execute_script(webview: &ICoreWebView2, script: &str) -> NativeResult<Value> {
    let webview = webview.clone();
    let script = HSTRING::from(script);
    let (tx, rx) = mpsc::channel();
    ExecuteScriptCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            webview
                .ExecuteScript(&script, &handler)
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(
            move |error_code: windows::core::Result<()>, result: String| {
                error_code?;
                tx.send(result)
                    .map_err(|_| WindowsError::from(E_UNEXPECTED))?;
                Ok(())
            },
        ),
    )
    .map_err(format_webview2_error)?;
    let raw = rx
        .recv()
        .map_err(|_| "WebView2 script 결과 채널이 종료되었습니다.".to_owned())?;

    if raw.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&raw).map_err(|error| {
        format!(
            "WebView2 script result JSON 파싱 실패: {error}; raw={}",
            crate::logging::summarize_text(&raw, 220)
        )
    })
}

fn message_loop(
    hwnd: HWND,
    webview: ICoreWebView2,
    controller: ICoreWebView2Controller,
    rx: mpsc::Receiver<NativeCommand>,
    shared: Arc<NativeWebView2SharedState>,
) {
    let mut msg = MSG::default();
    loop {
        while let Ok(command) = rx.try_recv() {
            if !handle_command(command, hwnd, &webview, &controller, &shared) {
                return;
            }
        }

        let result = unsafe { GetMessageW(&mut msg, None, 0, 0).0 };
        match result {
            -1 | 0 => return,
            _ => {
                if msg.message != WM_APP {
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
        }
    }
}

fn handle_command(
    command: NativeCommand,
    hwnd: HWND,
    webview: &ICoreWebView2,
    _controller: &ICoreWebView2Controller,
    shared: &Arc<NativeWebView2SharedState>,
) -> bool {
    match command {
        NativeCommand::Evaluate(script, reply) => {
            let _ = reply.send(execute_script(webview, &script));
            true
        }
        NativeCommand::Navigate(url, reply) => {
            let _ = reply.send(navigate_webview(webview, &url));
            true
        }
        NativeCommand::StopLoading(reply) => {
            let result = unsafe { webview.Stop().map_err(format_windows_error) };
            let _ = reply.send(result);
            true
        }
        NativeCommand::BringToFront(reply) => {
            ensure_operational_window_state(hwnd);
            let _ = reply.send(Ok(()));
            true
        }
        NativeCommand::ShowWindow(reply) => {
            apply_visible_state(hwnd);
            let _ = reply.send(Ok(true));
            true
        }
        NativeCommand::HideWindow(reply) => {
            apply_offscreen_background_state(hwnd, true);
            let _ = reply.send(Ok(false));
            true
        }
        NativeCommand::ToggleWindow(reply) => {
            let visible = if let Some(state) = unsafe { window_state_mut(hwnd) } {
                state.user_requested_visible && !state.pseudo_minimized_by_user
            } else {
                false
            };
            if visible {
                apply_offscreen_background_state(hwnd, true);
                let _ = reply.send(Ok(false));
            } else {
                apply_visible_state(hwnd);
                let _ = reply.send(Ok(true));
            }
            true
        }
        NativeCommand::Health(reply) => {
            let _ = reply.send(Ok(capture_webview_health(webview, shared)));
            true
        }
        NativeCommand::ClearCache(reply) => {
            let _ = reply.send(clear_browsing_data(webview));
            true
        }
        NativeCommand::ActivateRequestGuard(until_ms, reply) => {
            shared
                .request_guard_until_ms
                .store(until_ms, Ordering::SeqCst);
            let _ = unsafe { webview.Stop() };
            let _ = reply.send(Ok(()));
            true
        }
        NativeCommand::Shutdown => {
            unsafe {
                let _ = DestroyWindow(hwnd);
                PostQuitMessage(0);
            }
            false
        }
    }
}

fn capture_webview_health(
    webview: &ICoreWebView2,
    shared: &NativeWebView2SharedState,
) -> NativeWebView2Health {
    let _ = update_browser_process_snapshot(webview, shared);
    let kind = shared.process_failed_kind.load(Ordering::SeqCst);
    NativeWebView2Health {
        browser_process_id: shared.browser_process_id.load(Ordering::SeqCst),
        browser_process_alive: shared.browser_process_alive.load(Ordering::SeqCst),
        process_failed: shared.process_failed.load(Ordering::SeqCst),
        critical_process_failed: shared.critical_process_failed.load(Ordering::SeqCst),
        process_failed_kind: (kind >= 0).then_some(kind),
        navigation_completed_count: shared.navigation_completed_count.load(Ordering::SeqCst),
        last_navigation_succeeded: shared.last_navigation_succeeded.load(Ordering::SeqCst),
        native_request_guard_hits: shared.native_request_guard_hits.load(Ordering::SeqCst),
    }
}

fn update_browser_process_snapshot(
    webview: &ICoreWebView2,
    shared: &NativeWebView2SharedState,
) -> NativeResult<()> {
    let mut pid = 0u32;
    unsafe {
        webview
            .BrowserProcessId(&mut pid)
            .map_err(format_windows_error)?;
    }
    let alive = pid != 0 && is_process_alive(pid);
    shared.browser_process_id.store(pid, Ordering::SeqCst);
    shared.browser_process_alive.store(alive, Ordering::SeqCst);
    Ok(())
}

fn is_process_alive(pid: u32) -> bool {
    let Ok(handle) = (unsafe {
        Threading::OpenProcess(Threading::PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
    }) else {
        return false;
    };
    let mut exit_code = 0u32;
    let alive = unsafe { Threading::GetExitCodeProcess(handle, &mut exit_code).is_ok() }
        && exit_code == 259;
    let _ = unsafe { CloseHandle(handle) };
    alive
}

fn clear_browsing_data(webview: &ICoreWebView2) -> NativeResult<()> {
    let webview13 = webview
        .cast::<ICoreWebView2_13>()
        .map_err(format_windows_error)?;
    let profile = unsafe { webview13.Profile().map_err(format_windows_error)? };
    let profile2 = profile
        .cast::<ICoreWebView2Profile2>()
        .map_err(format_windows_error)?;
    let kinds = COREWEBVIEW2_BROWSING_DATA_KINDS_CACHE_STORAGE
        | COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE
        | COREWEBVIEW2_BROWSING_DATA_KINDS_FILE_SYSTEMS
        | COREWEBVIEW2_BROWSING_DATA_KINDS_INDEXED_DB
        | COREWEBVIEW2_BROWSING_DATA_KINDS_LOCAL_STORAGE;

    ClearBrowsingDataCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            profile2
                .ClearBrowsingData(kinds, &handler)
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(|error_code| error_code),
    )
    .map_err(format_webview2_error)?;
    Ok(())
}

fn create_frame_window(title: &str) -> NativeResult<HWND> {
    let hinstance = unsafe {
        LibraryLoader::GetModuleHandleW(None)
            .map(|module| HINSTANCE(module.0))
            .map_err(format_windows_error)?
    };
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: hinstance,
        lpszClassName: w!("RusterWebView2Frame"),
        ..Default::default()
    };
    unsafe {
        let _ = RegisterClassW(&class);
    }

    let title = HSTRING::from(title);
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("RusterWebView2Frame"),
            &title,
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1120,
            860,
            None,
            None,
            Some(hinstance),
            Some(Box::into_raw(Box::new(WindowState {
                controller: None,
                user_requested_visible: false,
                pseudo_minimized_by_user: false,
                last_visible_bounds: None,
            })) as *const _),
        )
        .map_err(format_windows_error)?
    };
    if hwnd.0.is_null() {
        return Err("WebView2 frame window 생성 실패".to_owned());
    }
    Ok(hwnd)
}

fn set_controller(hwnd: HWND, controller: ICoreWebView2Controller) {
    if let Some(state) = unsafe { window_state_mut(hwnd) } {
        state.controller = Some(controller);
    }
}

fn resize_controller_to_window(hwnd: HWND, controller: &ICoreWebView2Controller) {
    let mut rect = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rect);
        let _ = controller.SetBounds(rect);
    }
}

fn ensure_operational_window_state(hwnd: HWND) {
    let Some(state) = (unsafe { window_state_mut(hwnd) }) else {
        return;
    };
    if state.user_requested_visible && !state.pseudo_minimized_by_user {
        set_taskbar_and_alpha(hwnd, true, 255);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        }
    } else if state.pseudo_minimized_by_user {
        apply_pseudo_minimize_from_user(hwnd);
    } else {
        apply_offscreen_background_state(hwnd, false);
    }
}

fn apply_visible_state(hwnd: HWND) {
    let rect = visible_restore_rect(hwnd);
    if let Some(state) = unsafe { window_state_mut(hwnd) } {
        state.user_requested_visible = true;
        state.pseudo_minimized_by_user = false;
    }
    set_taskbar_and_alpha(hwnd, true, 255);
    set_window_rect(hwnd, rect, SET_WINDOW_POS_FLAGS(SWP_NOZORDER.0));
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
    }
}

fn apply_offscreen_background_state(hwnd: HWND, remember_bounds: bool) {
    if remember_bounds {
        capture_visible_bounds(hwnd);
    }
    if let Some(state) = unsafe { window_state_mut(hwnd) } {
        state.user_requested_visible = false;
        state.pseudo_minimized_by_user = false;
    }
    set_taskbar_and_alpha(hwnd, false, 3);
    move_window_offscreen(hwnd, false);
}

fn apply_pseudo_minimize_from_user(hwnd: HWND) {
    capture_visible_bounds(hwnd);
    if let Some(state) = unsafe { window_state_mut(hwnd) } {
        state.user_requested_visible = true;
        state.pseudo_minimized_by_user = true;
    }
    set_taskbar_and_alpha(hwnd, true, 3);
    move_window_offscreen(hwnd, true);
}

fn restore_from_pseudo_minimize(hwnd: HWND) {
    if let Some(state) = unsafe { window_state_mut(hwnd) }
        && !state.pseudo_minimized_by_user
    {
        return;
    }
    apply_visible_state(hwnd);
}

fn capture_visible_bounds(hwnd: HWND) {
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return;
    }
    if rect.left <= -5000 || rect.top <= -5000 {
        return;
    }
    if rect.right - rect.left <= 0 || rect.bottom - rect.top <= 0 {
        return;
    }
    if let Some(state) = unsafe { window_state_mut(hwnd) } {
        state.last_visible_bounds = Some(rect);
    }
}

fn visible_restore_rect(hwnd: HWND) -> RECT {
    if let Some(state) = unsafe { window_state_mut(hwnd) }
        && let Some(rect) = state.last_visible_bounds
    {
        return rect;
    }
    RECT {
        left: 80,
        top: 80,
        right: 1200,
        bottom: 940,
    }
}

fn move_window_offscreen(hwnd: HWND, _keep_taskbar: bool) {
    let rect = current_window_rect(hwnd).unwrap_or(RECT {
        left: 0,
        top: 0,
        right: 1120,
        bottom: 860,
    });
    let width = (rect.right - rect.left).max(800);
    let height = (rect.bottom - rect.top).max(600);
    let flags = SET_WINDOW_POS_FLAGS(SWP_NOZORDER.0 | SWP_NOACTIVATE.0);
    unsafe {
        let _ = SetWindowPos(hwnd, None, -10000, -10000, width, height, flags);
        let _ = ShowWindow(hwnd, SW_SHOWNA);
    }
}

fn current_window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(rect)
}

fn set_window_rect(hwnd: HWND, rect: RECT, flags: SET_WINDOW_POS_FLAGS) {
    let width = (rect.right - rect.left).max(800);
    let height = (rect.bottom - rect.top).max(600);
    unsafe {
        let _ = SetWindowPos(hwnd, None, rect.left, rect.top, width, height, flags);
    }
}

fn set_taskbar_and_alpha(hwnd: HWND, show_in_taskbar: bool, alpha: u8) {
    let mut style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    style |= WS_EX_LAYERED.0;
    if show_in_taskbar {
        style |= WS_EX_APPWINDOW.0;
        style &= !WS_EX_TOOLWINDOW.0;
    } else {
        style |= WS_EX_TOOLWINDOW.0;
        style &= !WS_EX_APPWINDOW.0;
    }
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style as isize);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SET_WINDOW_POS_FLAGS(
                SWP_NOMOVE.0
                    | SWP_NOSIZE.0
                    | SWP_NOZORDER.0
                    | SWP_NOACTIVATE.0
                    | SWP_FRAMECHANGED.0,
            ),
        );
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
    }
}

unsafe fn window_state_mut(hwnd: HWND) -> Option<&'static mut WindowState> {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowState;
    if ptr.is_null() {
        None
    } else {
        unsafe { ptr.as_mut() }
    }
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let createstruct = l_param.0 as *const CREATESTRUCTW;
            if !createstruct.is_null() {
                let state = unsafe { (*createstruct).lpCreateParams } as *mut WindowState;
                if !state.is_null() {
                    unsafe {
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
                    }
                    return LRESULT(1);
                }
            }
        }
        WM_SIZE => {
            if let Some(state) = unsafe { window_state_mut(hwnd) }
                && let Some(controller) = state.controller.as_ref()
            {
                resize_controller_to_window(hwnd, controller);
            }
            return LRESULT(0);
        }
        WM_ACTIVATE => {
            let active_state = (w_param.0 & 0xffff) as u32;
            if active_state != WA_INACTIVE {
                restore_from_pseudo_minimize(hwnd);
            }
        }
        WM_SYSCOMMAND => {
            let command = (w_param.0 as u32) & 0xfff0;
            if command == SC_MINIMIZE {
                if let Some(state) = unsafe { window_state_mut(hwnd) }
                    && state.user_requested_visible
                {
                    if state.pseudo_minimized_by_user {
                        restore_from_pseudo_minimize(hwnd);
                    } else {
                        apply_pseudo_minimize_from_user(hwnd);
                    }
                    return LRESULT(0);
                }
            } else if command == SC_RESTORE {
                restore_from_pseudo_minimize(hwnd);
                return LRESULT(0);
            }
        }
        WM_CLOSE => unsafe {
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        },
        WM_DESTROY => unsafe {
            PostQuitMessage(0);
            return LRESULT(0);
        },
        WM_NCDESTROY => unsafe {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
            if !ptr.is_null() {
                let _ = Box::from_raw(ptr);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
        },
        _ => {}
    }

    unsafe { DefWindowProcW(hwnd, msg, w_param, l_param) }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn format_webview2_process_kind(kind: i32) -> &'static str {
    match kind {
        0 => "BrowserProcessExited",
        1 => "RenderProcessExited",
        2 => "RenderProcessUnresponsive",
        3 => "FrameRenderProcessExited",
        4 => "UtilityProcessExited",
        5 => "SandboxHelperProcessExited",
        6 => "GpuProcessExited",
        7 => "PpapiPluginProcessExited",
        8 => "PpapiBrokerProcessExited",
        9 => "UnknownProcessExited",
        -1 => "Unknown",
        _ => "Other",
    }
}

fn format_windows_error(error: WindowsError) -> String {
    format!("{error}")
}

fn format_webview2_error(error: webview2_com::Error) -> String {
    format!("{error}")
}
