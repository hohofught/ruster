use std::sync::Arc;

use tokio::runtime::Handle;

use crate::host::{TranslationMode, TranslatorHost};
use crate::logging::LogBuffer;

#[cfg(windows)]
pub fn spawn(host: Arc<TranslatorHost>, logs: LogBuffer, runtime: Handle) {
    if !matches!(
        host.mode(),
        TranslationMode::WebView | TranslationMode::ChatGptWebView
    ) {
        return;
    }

    let _ = std::thread::Builder::new()
        .name("ruster-console-hotkeys".to_owned())
        .spawn(move || run_windows_console_hotkeys(host, logs, runtime));
}

#[cfg(not(windows))]
pub fn spawn(_host: Arc<TranslatorHost>, _logs: LogBuffer, _runtime: Handle) {}

#[cfg(windows)]
fn run_windows_console_hotkeys(host: Arc<TranslatorHost>, logs: LogBuffer, runtime: Handle) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use windows::Win32::System::Console::{
        GetStdHandle, INPUT_RECORD, KEY_EVENT, ReadConsoleInputW, STD_INPUT_HANDLE,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_F5, VK_F8, VK_F9, VK_F10, VK_F12};

    let handle = match unsafe { GetStdHandle(STD_INPUT_HANDLE) } {
        Ok(handle) => handle,
        Err(error) => {
            logs.push(format!("[Hotkey] 콘솔 입력 핸들을 열 수 없습니다: {error}"));
            return;
        }
    };

    logs.push("[Hotkey] 단축키 활성화: F5=세션복구, F8=WebView 표시/숨김, F9=숨김, F10=표시, F12=요청 취소/1초 차단+복구");
    let recovery_in_progress = Arc::new(AtomicBool::new(false));

    loop {
        let mut records = [INPUT_RECORD::default(); 8];
        let mut read = 0u32;
        if let Err(error) = unsafe { ReadConsoleInputW(handle, &mut records, &mut read) } {
            logs.push(format!("[Hotkey] 콘솔 입력 대기 오류: {error}"));
            std::thread::sleep(Duration::from_millis(250));
            return;
        }

        for record in records.iter().take(read as usize) {
            if record.EventType != KEY_EVENT as u16 {
                continue;
            }
            let key = unsafe { record.Event.KeyEvent };
            if !key.bKeyDown.as_bool() {
                continue;
            }

            match key.wVirtualKeyCode {
                code if code == VK_F5.0 => {
                    if recovery_in_progress.swap(true, Ordering::SeqCst) {
                        logs.push("[Hotkey] 세션 복구가 이미 진행 중입니다.");
                        continue;
                    }
                    let host = host.clone();
                    let logs = logs.clone();
                    let recovery_in_progress = recovery_in_progress.clone();
                    runtime.spawn(async move {
                        logs.push("[Hotkey] WebView 세션 복구 요청");
                        let recovered = host.request_session_recovery().await;
                        logs.push(if recovered {
                            "[Hotkey] WebView 세션 복구 완료"
                        } else {
                            "[Hotkey] WebView 세션 복구 실패"
                        });
                        recovery_in_progress.store(false, Ordering::SeqCst);
                    });
                }
                code if code == VK_F12.0 => {
                    let host = host.clone();
                    let logs = logs.clone();
                    runtime.spawn(async move {
                        let remaining = host
                            .activate_request_guard_with_recovery("console-f12")
                            .await;
                        logs.push(format!(
                            "[Hotkey] F12 처리 - 현재 요청 취소, {:.1}초 차단 후 세션 복구",
                            remaining.as_secs_f32()
                        ));
                    });
                }
                code if code == VK_F8.0 => {
                    let host = host.clone();
                    let logs = logs.clone();
                    runtime.spawn(async move {
                        let visible = host.toggle_webview_visibility().await;
                        logs.push(if visible {
                            "[Hotkey] WebView 표시 요청"
                        } else {
                            "[Hotkey] WebView 숨김 요청"
                        });
                    });
                }
                code if code == VK_F9.0 => {
                    let host = host.clone();
                    let logs = logs.clone();
                    runtime.spawn(async move {
                        let _ = host.hide_webview().await;
                        logs.push("[Hotkey] WebView 숨김 요청");
                    });
                }
                code if code == VK_F10.0 => {
                    let host = host.clone();
                    let logs = logs.clone();
                    runtime.spawn(async move {
                        let _ = host.show_webview().await;
                        logs.push("[Hotkey] WebView 표시 요청");
                    });
                }
                _ => {}
            }
        }
    }
}
