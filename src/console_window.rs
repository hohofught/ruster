use crate::logging::LogBuffer;

#[cfg(windows)]
pub fn detach_for_tray(logs: &LogBuffer) {
    use std::fs::{File, OpenOptions};
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        FreeConsole, GetConsoleWindow, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};

    static NUL_HANDLES: OnceLock<Option<(File, File)>> = OnceLock::new();

    logs.set_stdout_enabled(false);

    let hwnd = unsafe { GetConsoleWindow() };
    if !hwnd.0.is_null() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }

    let handles = NUL_HANDLES.get_or_init(|| {
        let stdout = OpenOptions::new().write(true).open("NUL").ok()?;
        let stderr = OpenOptions::new().write(true).open("NUL").ok()?;
        Some((stdout, stderr))
    });

    unsafe {
        if let Some((stdout, stderr)) = handles.as_ref() {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, HANDLE(stdout.as_raw_handle()));
            let _ = SetStdHandle(STD_ERROR_HANDLE, HANDLE(stderr.as_raw_handle()));
        }
        let _ = FreeConsole();
    }
}

#[cfg(not(windows))]
pub fn detach_for_tray(_logs: &LogBuffer) {}
