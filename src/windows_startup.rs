pub const STARTUP_ARGUMENT: &str = "--windows-startup";
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "ruster";

pub fn apply(enabled: bool) -> anyhow::Result<()> {
    platform::apply(enabled)
}

pub fn is_registered() -> bool {
    platform::is_registered()
}

fn build_command(exe_path: &str) -> String {
    format!("{} {STARTUP_ARGUMENT}", quote_command_arg(exe_path))
}

fn quote_command_arg(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

fn contains_startup_argument(value: &str) -> bool {
    value.split_whitespace().any(|part| {
        part.trim_matches('"')
            .eq_ignore_ascii_case(STARTUP_ARGUMENT)
    }) || value
        .to_ascii_lowercase()
        .contains(&STARTUP_ARGUMENT.to_ascii_lowercase())
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::path::PathBuf;

    use anyhow::Context;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
        RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegSetValueExW,
    };
    use windows::core::PCWSTR;

    use super::*;

    pub fn apply(enabled: bool) -> anyhow::Result<()> {
        let key = RunKey::create()?;
        if enabled {
            let command = build_command(&startup_exe_path()?);
            key.set_string(VALUE_NAME, &command)
        } else {
            key.delete_value(VALUE_NAME)
        }
    }

    pub fn is_registered() -> bool {
        read_run_value()
            .map(|value| contains_startup_argument(&value))
            .unwrap_or(false)
    }

    fn startup_exe_path() -> anyhow::Result<String> {
        std::env::current_exe()
            .or_else(|_| {
                std::env::args_os()
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::NotFound, "argv[0] missing")
                    })
            })
            .context("현재 실행 파일 경로를 확인할 수 없습니다.")
            .map(|path| path.to_string_lossy().into_owned())
    }

    fn read_run_value() -> anyhow::Result<String> {
        let value_name = wide_null(VALUE_NAME);
        let subkey = wide_null(RUN_KEY_PATH);
        let mut data = vec![0u16; 2048];
        let mut byte_len = (data.len() * std::mem::size_of::<u16>()) as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                PCWSTR(value_name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(data.as_mut_ptr() as *mut c_void),
                Some(&mut byte_len),
            )
        };
        win32_result(
            status,
            "Windows 시작 프로그램 레지스트리 값을 읽을 수 없습니다.",
        )?;
        let units = (byte_len as usize / std::mem::size_of::<u16>()).min(data.len());
        let end = data[..units]
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(units);
        Ok(String::from_utf16_lossy(&data[..end]))
    }

    struct RunKey(HKEY);

    impl RunKey {
        fn create() -> anyhow::Result<Self> {
            let subkey = wide_null(RUN_KEY_PATH);
            let mut key = HKEY::default();
            let status = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    PCWSTR(subkey.as_ptr()),
                    None,
                    PCWSTR::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_WRITE | KEY_QUERY_VALUE,
                    None,
                    &mut key,
                    None,
                )
            };
            win32_result(status, "Windows 시작 프로그램 레지스트리를 열 수 없습니다.")?;
            Ok(Self(key))
        }

        fn set_string(&self, name: &str, value: &str) -> anyhow::Result<()> {
            let name = wide_null(name);
            let value = wide_null(value);
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    value.as_ptr() as *const u8,
                    value.len() * std::mem::size_of::<u16>(),
                )
            };
            let status =
                unsafe { RegSetValueExW(self.0, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)) };
            win32_result(status, "Windows 시작 프로그램 값을 저장할 수 없습니다.")
        }

        fn delete_value(&self, name: &str) -> anyhow::Result<()> {
            let name = wide_null(name);
            let status = unsafe { RegDeleteValueW(self.0, PCWSTR(name.as_ptr())) };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            win32_result(status, "Windows 시작 프로그램 값을 삭제할 수 없습니다.")
        }
    }

    impl Drop for RunKey {
        fn drop(&mut self) {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    fn wide_null(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn win32_result(status: WIN32_ERROR, context: &str) -> anyhow::Result<()> {
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            anyhow::bail!("{context} (Win32 error {})", status.0)
        }
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn apply(_enabled: bool) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn is_registered() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_command_uses_ruster_argument_contract() {
        let command = build_command(r#"C:\Program Files\ruster\ruster.exe"#);

        assert_eq!(
            command,
            r#""C:\Program Files\ruster\ruster.exe" --windows-startup"#
        );
    }

    #[test]
    fn startup_registration_detection_is_case_insensitive() {
        assert!(contains_startup_argument(
            r#""C:\ruster\ruster.exe" --WINDOWS-STARTUP"#
        ));
        assert!(!contains_startup_argument(
            r#""C:\ruster\ruster.exe" --headless"#
        ));
    }

    #[test]
    fn startup_registry_value_name_is_ruster_owned() {
        assert_eq!(VALUE_NAME, "ruster");
    }
}
