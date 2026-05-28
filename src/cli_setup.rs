use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cli_discovery;

#[derive(Clone, Debug, Default)]
pub struct CliSetupEnvironmentStatus {
    pub node_path: String,
    pub node_version: String,
    pub npm_path: String,
    pub npm_version: String,
    pub gemini_path: String,
    pub gemini_version: String,
    pub winget_path: String,
    pub winget_version: String,
}

impl CliSetupEnvironmentStatus {
    pub fn has_node(&self) -> bool {
        !self.node_path.trim().is_empty()
    }

    pub fn has_npm(&self) -> bool {
        !self.npm_path.trim().is_empty()
    }

    pub fn has_gemini(&self) -> bool {
        !self.gemini_path.trim().is_empty()
    }

    pub fn has_winget(&self) -> bool {
        !self.winget_path.trim().is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "Node.js: {}\nnpm: {}\nGemini CLI: {}\nwinget: {}",
            format_status(self.has_node(), &self.node_version, &self.node_path),
            format_status(self.has_npm(), &self.npm_version, &self.npm_path),
            format_status(self.has_gemini(), &self.gemini_version, &self.gemini_path),
            format_status(self.has_winget(), &self.winget_version, &self.winget_path),
        )
    }
}

pub fn get_environment_status() -> CliSetupEnvironmentStatus {
    let mut node_path = find_setup_command_path(if cfg!(windows) { "node.exe" } else { "node" });
    if node_path.is_empty() {
        node_path = find_known_node_executable_path();
    }

    let mut npm_path = find_setup_command_path(if cfg!(windows) { "npm.cmd" } else { "npm" });
    if npm_path.is_empty() {
        npm_path = find_setup_command_path("npm");
    }

    cli_discovery::reset_cache();
    let (gemini_path, gemini_version) = if let Some(installation) = cli_discovery::try_find() {
        let version = run_command_first_line(
            &installation.file_name.display().to_string(),
            &installation
                .prefix_args
                .iter()
                .map(String::as_str)
                .chain(["--version"])
                .collect::<Vec<_>>(),
            Duration::from_secs(5),
        );
        (installation.display_source(), version)
    } else {
        (String::new(), String::new())
    };

    let winget_path = if cfg!(windows) {
        find_setup_command_path("winget.exe")
    } else {
        String::new()
    };

    CliSetupEnvironmentStatus {
        node_version: version_for(&node_path),
        node_path,
        npm_version: version_for(&npm_path),
        npm_path,
        gemini_path,
        gemini_version,
        winget_version: version_for(&winget_path),
        winget_path,
    }
}

pub fn launch_login_terminal() -> Result<(), String> {
    let installation = cli_discovery::find()?;
    let invocation = build_powershell_cli_invocation(
        &installation.file_name.display().to_string(),
        &installation.prefix_args,
    );
    let command = format!(
        r#"$Host.UI.RawUI.WindowTitle='Gemini CLI Login'
Write-Host '[Gemini CLI] 로그인/초기화 자동 실행 중...' -ForegroundColor Cyan
Write-Host '앱은 로그인 완료를 자동 감지해 시작합니다.' -ForegroundColor DarkGray
{invocation}
Write-Host ''
Write-Host '[안내] 인증 완료 후 이 창은 닫아도 됩니다.' -ForegroundColor Yellow
"#
    );
    launch_powershell_window(&command)
}

pub fn launch_install_terminal() -> Result<(), String> {
    launch_powershell_window(INSTALL_SCRIPT)
}

fn find_setup_command_path(command_name: &str) -> String {
    for path in resolve_command_on_path(command_name) {
        if path.is_file() {
            return path.display().to_string();
        }
    }
    String::new()
}

fn find_known_node_executable_path() -> String {
    let candidates = if cfg!(windows) {
        let mut out = Vec::new();
        for root in [
            std::env::var("ProgramFiles").ok(),
            std::env::var("ProgramW6432").ok(),
            std::env::var("ProgramFiles(x86)").ok(),
        ]
        .into_iter()
        .flatten()
        {
            let root = PathBuf::from(root);
            out.push(root.join("nodejs").join("node.exe"));
            out.push(root.join("Node.js").join("node.exe"));
        }
        if let Some(local) = dirs::data_local_dir() {
            out.push(local.join("Programs").join("nodejs").join("node.exe"));
        }
        out
    } else {
        vec![
            PathBuf::from("/usr/local/bin/node"),
            PathBuf::from("/opt/homebrew/bin/node"),
            PathBuf::from("/usr/bin/node"),
        ]
    };

    candidates
        .into_iter()
        .find(|path| path.is_file())
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

fn version_for(path: &str) -> String {
    if path.trim().is_empty() {
        String::new()
    } else {
        run_command_first_line(path, &["--version"], Duration::from_secs(3))
    }
}

fn resolve_command_on_path(command_name: &str) -> Vec<PathBuf> {
    let Some(path_var) = std::env::var_os("PATH") else {
        return Vec::new();
    };

    std::env::split_paths(&path_var)
        .map(|dir| dir.join(command_name))
        .filter(|path| path.is_file())
        .collect()
}

fn run_command_first_line(file_name: &str, args: &[&str], timeout: Duration) -> String {
    let Ok(mut process) = Command::new(file_name)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return String::new();
    };

    let start = Instant::now();
    loop {
        if start.elapsed() >= timeout {
            let _ = process.kill();
            let _ = process.wait();
            return String::new();
        }
        match process.try_wait() {
            Ok(Some(status)) => {
                let Ok(output) = process.wait_with_output() else {
                    return String::new();
                };
                let text = if status.success() || !output.stdout.is_empty() {
                    String::from_utf8_lossy(&output.stdout).to_string()
                } else {
                    String::from_utf8_lossy(&output.stderr).to_string()
                };
                return text
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or_default()
                    .to_owned();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return String::new(),
        }
    }
}

fn build_powershell_cli_invocation(file_name: &str, prefix_args: &[String]) -> String {
    let args = prefix_args
        .iter()
        .map(|arg| quote_powershell_string(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!("& {} {}", quote_powershell_string(file_name), args)
}

fn launch_powershell_window(command: &str) -> Result<(), String> {
    let encoded = encode_powershell_command(command);
    if cfg!(windows) {
        Command::new("cmd.exe")
            .arg("/C")
            .arg("start")
            .arg("")
            .arg("powershell.exe")
            .arg("-NoExit")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-EncodedCommand")
            .arg(encoded)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("PowerShell 창 실행 실패: {error}"))
    } else {
        Err("CLI 초기설정 터미널 자동 실행은 현재 Windows만 지원합니다.".to_owned())
    }
}

fn encode_powershell_command(command: &str) -> String {
    let mut bytes = Vec::with_capacity(command.len() * 2);
    for unit in command.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    base64_standard(&bytes)
}

fn base64_standard(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let chunk = ((bytes[index] as u32) << 16)
            | ((bytes[index + 1] as u32) << 8)
            | bytes[index + 2] as u32;
        out.push(TABLE[((chunk >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((chunk >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((chunk >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(chunk & 0x3f) as usize] as char);
        index += 3;
    }

    match bytes.len() - index {
        1 => {
            let chunk = (bytes[index] as u32) << 16;
            out.push(TABLE[((chunk >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((chunk >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let chunk = ((bytes[index] as u32) << 16) | ((bytes[index + 1] as u32) << 8);
            out.push(TABLE[((chunk >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((chunk >> 12) & 0x3f) as usize] as char);
            out.push(TABLE[((chunk >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }

    out
}

fn quote_powershell_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn format_status(found: bool, version: &str, path: &str) -> String {
    if !found {
        return "없음".to_owned();
    }
    let version = if version.trim().is_empty() {
        "버전 확인 실패"
    } else {
        version.trim()
    };
    format!("{version} ({path})")
}

const INSTALL_SCRIPT: &str = r#"
$Host.UI.RawUI.WindowTitle='Gemini CLI Install / Login'
$ErrorActionPreference='Continue'

function Refresh-Path {
  $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
  $user = [Environment]::GetEnvironmentVariable('Path', 'User')
  $env:Path = "$machine;$user"
}

function Has-Command([string]$name) {
  return [bool](Get-Command $name -ErrorAction SilentlyContinue)
}

function Get-GeminiCliInvocation {
  if (-not (Has-Command 'npm') -or -not (Has-Command 'node')) {
    return $null
  }

  $npmRoot = (& npm root -g 2>$null | Select-Object -First 1)
  if ([string]::IsNullOrWhiteSpace($npmRoot)) {
    return $null
  }

  $packageDir = Join-Path (Join-Path $npmRoot '@google') 'gemini-cli'
  $packageJson = Join-Path $packageDir 'package.json'
  if (-not (Test-Path -LiteralPath $packageJson)) {
    return $null
  }

  $relativePath = ''
  try {
    $pkg = Get-Content -LiteralPath $packageJson -Raw | ConvertFrom-Json
    if ($pkg.name -ne '@google/gemini-cli') {
      return $null
    }

    if ($pkg.bin -is [string]) {
      $relativePath = $pkg.bin
    } elseif ($pkg.bin -and $pkg.bin.gemini) {
      $relativePath = [string]$pkg.bin.gemini
    }
  } catch {
    $relativePath = ''
  }

  if ([string]::IsNullOrWhiteSpace($relativePath)) {
    foreach ($fallback in @('bundle/gemini.js', 'dist/index.js')) {
      if (Test-Path -LiteralPath (Join-Path $packageDir $fallback)) {
        $relativePath = $fallback
        break
      }
    }
  }

  if ([string]::IsNullOrWhiteSpace($relativePath)) {
    return $null
  }

  $entry = Join-Path $packageDir ($relativePath -replace '/', '\')
  if (-not (Test-Path -LiteralPath $entry)) {
    return $null
  }

  $node = Get-Command 'node' -ErrorAction SilentlyContinue
  if (-not $node) {
    return $null
  }

  return [pscustomobject]@{ Node = $node.Source; Entry = $entry; PackageDir = $packageDir }
}

function Invoke-GeminiCli {
  param([string[]]$CliArgs = @())
  $cli = Get-GeminiCliInvocation
  if (-not $cli) {
    throw '공식 @google/gemini-cli 실행 파일을 찾을 수 없습니다.'
  }
  & $cli.Node $cli.Entry @CliArgs
}

Write-Host '[Gemini CLI] 설치/로그인 초기설정' -ForegroundColor Cyan
Write-Host '배포 환경에서는 Node.js LTS + npm + @google/gemini-cli가 필요합니다.' -ForegroundColor DarkGray
Refresh-Path

if (-not (Has-Command 'node') -or -not (Has-Command 'npm')) {
  Write-Host ''
  Write-Host '[1/3] Node.js/npm을 찾을 수 없습니다.' -ForegroundColor Yellow
  if (Has-Command 'winget') {
    Write-Host 'winget으로 Node.js LTS 설치를 시도합니다.' -ForegroundColor Cyan
    winget install -e --id OpenJS.NodeJS.LTS --accept-package-agreements --accept-source-agreements
    Refresh-Path
  } else {
    Write-Host 'winget도 찾을 수 없습니다. https://nodejs.org 에서 Node.js LTS를 설치한 뒤 이 버튼을 다시 눌러주세요.' -ForegroundColor Red
  }
}

Refresh-Path
if (-not (Has-Command 'npm')) {
  Write-Host ''
  Write-Host '[중단] npm을 아직 찾을 수 없습니다. Node.js LTS 설치가 끝난 뒤 앱에서 CLI 초기설정을 다시 실행하세요.' -ForegroundColor Red
  Write-Host ''
  Write-Host '[안내] 앱은 준비 상태를 자동 감지합니다.' -ForegroundColor Yellow
  return
}

Write-Host ''
Write-Host '[2/3] Node/npm 확인' -ForegroundColor Cyan
node --version
npm --version

if (-not (Get-GeminiCliInvocation)) {
  Write-Host ''
  Write-Host '[3/3] Gemini CLI 설치: npm install -g @google/gemini-cli' -ForegroundColor Cyan
  npm install -g @google/gemini-cli
  Refresh-Path
}

Refresh-Path
if (-not (Get-GeminiCliInvocation)) {
  Write-Host '설치는 끝났지만 공식 @google/gemini-cli 패키지 실행 파일을 찾지 못했습니다. 앱 또는 PC를 재시작한 뒤 다시 시도하세요.' -ForegroundColor Red
  return
}

Write-Host ''
Write-Host '[Gemini CLI] 설치 완료. 버전 확인 후 로그인/온보딩을 시작합니다.' -ForegroundColor Green
Invoke-GeminiCli @('--version')
Invoke-GeminiCli
Write-Host ''
Write-Host '[안내] 인증 완료 후 이 창은 닫아도 됩니다. 앱은 준비 상태를 자동 감지합니다.' -ForegroundColor Yellow
"#;
