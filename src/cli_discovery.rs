use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

const OFFICIAL_PACKAGE_NAME: &str = "@google/gemini-cli";

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct GeminiCliInstallation {
    pub file_name: PathBuf,
    pub prefix_args: Vec<String>,
    pub source_path: PathBuf,
    pub package_dir: PathBuf,
    pub package_version: String,
    pub node_path: PathBuf,
    pub uses_node_direct: bool,
}

#[allow(dead_code)]
impl GeminiCliInstallation {
    pub fn display_source(&self) -> String {
        let source = self.source_path.display().to_string();
        if self.package_version.trim().is_empty() {
            source
        } else {
            format!("{source} (pkg {})", self.package_version)
        }
    }
}

static CACHED_INSTALLATION: OnceLock<Mutex<Option<GeminiCliInstallation>>> = OnceLock::new();

pub fn find() -> Result<GeminiCliInstallation, String> {
    try_find().ok_or_else(|| {
        "공식 Gemini CLI(@google/gemini-cli)를 찾을 수 없습니다. npm install -g @google/gemini-cli 설치 후 다시 시도해주세요."
            .to_owned()
    })
}

pub fn try_find() -> Option<GeminiCliInstallation> {
    let cache = CACHED_INSTALLATION.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock()
        && let Some(installation) = guard.clone()
    {
        return Some(installation);
    }

    let installation = discover();
    if let Some(found) = installation.clone()
        && let Ok(mut guard) = cache.lock()
    {
        *guard = Some(found);
    }
    installation
}

#[allow(dead_code)]
pub fn reset_cache() {
    if let Some(cache) = CACHED_INSTALLATION.get()
        && let Ok(mut guard) = cache.lock()
    {
        *guard = None;
    }
}

fn discover() -> Option<GeminiCliInstallation> {
    for command_path in get_command_candidates() {
        if let Some(candidate) = try_build_from_command_path(&command_path) {
            return Some(candidate);
        }
    }

    for package_dir in get_explicit_package_dir_candidates()
        .into_iter()
        .chain(get_global_package_dir_candidates())
    {
        if let Some(candidate) = try_build_from_package_dir(&package_dir, package_dir.clone(), None)
        {
            return Some(candidate);
        }
    }

    None
}

fn try_build_from_command_path(command_path: &Path) -> Option<GeminiCliInstallation> {
    if !command_path.is_file() {
        return None;
    }

    let command_dir = command_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let package_dir = try_resolve_package_dir_from_command_path(command_path)
        .or_else(|| try_resolve_package_dir_from_npm_bin(&command_dir))?;

    try_build_from_package_dir(&package_dir, command_path.to_path_buf(), Some(command_dir))
}

fn try_build_from_package_dir(
    package_dir: &Path,
    source_path: PathBuf,
    npm_bin_dir: Option<PathBuf>,
) -> Option<GeminiCliInstallation> {
    if !package_dir.is_dir() || !is_official_package_dir(package_dir) {
        return None;
    }

    let entry = try_resolve_package_bin(package_dir)?;
    if !entry.is_file() {
        return None;
    }

    let node_path = resolve_node_executable(npm_bin_dir.as_deref())?;
    if !node_path.is_file() {
        return None;
    }

    Some(GeminiCliInstallation {
        file_name: node_path.clone(),
        prefix_args: vec![entry.display().to_string()],
        source_path,
        package_dir: package_dir.to_path_buf(),
        package_version: try_read_package_string(package_dir, "version"),
        node_path,
        uses_node_direct: true,
    })
}

fn get_command_candidates() -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();

    for env_name in ["GEMINI_CLI_PATH", "GEMINI_CLI_COMMAND", "GEMINI_CLI_BIN"] {
        for candidate in expand_command_candidate(std::env::var(env_name).ok().as_deref()) {
            push_unique(&mut candidates, &mut seen, candidate);
        }
    }

    let names: &[&str] = if cfg!(windows) {
        &["gemini.cmd", "gemini.exe", "gemini.ps1", "gemini"]
    } else {
        &["gemini"]
    };

    for name in names {
        for candidate in resolve_command_on_path(name) {
            if cfg!(windows) && candidate.extension().is_none() {
                continue;
            }
            push_unique(&mut candidates, &mut seen, candidate);
        }
    }

    for candidate in get_known_gemini_wrapper_candidates() {
        push_unique(&mut candidates, &mut seen, candidate);
    }

    candidates
}

fn expand_command_candidate(raw: Option<&str>) -> Vec<PathBuf> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let value = raw.trim().trim_matches('"');
    if value.is_empty() {
        return Vec::new();
    }

    let direct = PathBuf::from(value);
    if direct.is_file() {
        return vec![direct];
    }

    resolve_command_on_path(value)
}

fn get_explicit_package_dir_candidates() -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for env_name in ["GEMINI_CLI_PACKAGE_DIR", "GEMINI_CLI_ROOT"] {
        let Ok(raw) = std::env::var(env_name) else {
            continue;
        };
        let path = PathBuf::from(raw.trim().trim_matches('"'));
        if path.is_dir() {
            push_unique(&mut out, &mut seen, path);
        }
    }
    out
}

fn get_known_gemini_wrapper_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if cfg!(windows) {
        for bin_dir in get_known_windows_package_bin_dirs() {
            for name in ["gemini.cmd", "gemini.ps1", "gemini.exe"] {
                let candidate = bin_dir.join(name);
                if candidate.is_file() {
                    out.push(candidate);
                }
            }
        }
        return out;
    }

    let home = dirs::home_dir().unwrap_or_default();
    for path in [
        PathBuf::from("/usr/local/bin/gemini"),
        PathBuf::from("/opt/homebrew/bin/gemini"),
        home.join(".npm-global").join("bin").join("gemini"),
        home.join(".local").join("bin").join("gemini"),
    ] {
        if path.is_file() {
            out.push(path);
        }
    }
    out
}

fn get_global_package_dir_candidates() -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for root in get_global_node_module_roots() {
        let package_dir = root.join("@google").join("gemini-cli");
        if package_dir.is_dir() {
            push_unique(&mut out, &mut seen, package_dir);
        }
    }

    out
}

fn get_global_node_module_roots() -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for command in ["npm", "pnpm"] {
        if let Some(root) = run_command_first_line(command, &["root", "-g"], Duration::from_secs(3))
        {
            let path = PathBuf::from(root.trim());
            if path.is_dir() {
                push_unique(&mut out, &mut seen, path);
            }
        }
    }

    if cfg!(windows) {
        for bin_dir in get_known_windows_package_bin_dirs() {
            let root = bin_dir.join("node_modules");
            if root.is_dir() {
                push_unique(&mut out, &mut seen, root);
            }
        }
    } else {
        let home = dirs::home_dir().unwrap_or_default();
        for root in [
            PathBuf::from("/usr/local/lib/node_modules"),
            PathBuf::from("/opt/homebrew/lib/node_modules"),
            home.join(".npm-global").join("lib").join("node_modules"),
        ] {
            if root.is_dir() {
                push_unique(&mut out, &mut seen, root);
            }
        }
    }

    out
}

fn get_known_windows_package_bin_dirs() -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for raw in [
        std::env::var("APPDATA").ok(),
        dirs::data_dir().map(|p| p.display().to_string()),
    ]
    .into_iter()
    .flatten()
    {
        let path = PathBuf::from(raw.trim().trim_matches('"')).join("npm");
        push_unique(&mut out, &mut seen, path);
    }

    for raw in [
        std::env::var("LOCALAPPDATA").ok(),
        dirs::data_local_dir().map(|p| p.display().to_string()),
    ]
    .into_iter()
    .flatten()
    {
        let path = PathBuf::from(raw.trim().trim_matches('"')).join("pnpm");
        push_unique(&mut out, &mut seen, path);
    }

    out
}

fn try_resolve_package_dir_from_npm_bin(npm_bin_dir: &Path) -> Option<PathBuf> {
    let candidate = npm_bin_dir
        .join("node_modules")
        .join("@google")
        .join("gemini-cli");
    candidate.is_dir().then_some(candidate)
}

fn try_resolve_package_dir_from_command_path(command_path: &Path) -> Option<PathBuf> {
    let mut cursor = command_path.parent();
    for _ in 0..8 {
        let Some(dir) = cursor else {
            break;
        };
        if dir.join("package.json").is_file()
            && try_read_package_string(dir, "name").eq_ignore_ascii_case(OFFICIAL_PACKAGE_NAME)
        {
            return Some(dir.to_path_buf());
        }
        cursor = dir.parent();
    }
    None
}

fn try_resolve_package_bin(package_dir: &Path) -> Option<PathBuf> {
    let package_json = package_dir.join("package.json");
    if let Ok(text) = std::fs::read_to_string(&package_json)
        && let Ok(value) = serde_json::from_str::<Value>(&text)
    {
        let relative = value.get("bin").and_then(|bin| {
            bin.as_str().map(ToOwned::to_owned).or_else(|| {
                bin.get("gemini")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
        });
        if let Some(relative) = relative {
            let candidate = package_dir.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
            if is_path_inside_directory(package_dir, &candidate) && candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    for fallback in ["bundle/gemini.js", "dist/index.js"] {
        let candidate = package_dir.join(fallback.replace('/', std::path::MAIN_SEPARATOR_STR));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn is_official_package_dir(package_dir: &Path) -> bool {
    try_read_package_string(package_dir, "name").eq_ignore_ascii_case(OFFICIAL_PACKAGE_NAME)
}

fn resolve_node_executable(npm_bin_dir: Option<&Path>) -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("GEMINI_CLI_NODE_PATH") {
        let candidate = PathBuf::from(raw.trim().trim_matches('"'));
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    if let Some(bin_dir) = npm_bin_dir {
        let candidate = bin_dir.join(if cfg!(windows) { "node.exe" } else { "node" });
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    for candidate in get_known_node_executable_candidates() {
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    resolve_command_on_path(if cfg!(windows) { "node.exe" } else { "node" })
        .into_iter()
        .next()
}

fn get_known_node_executable_candidates() -> Vec<PathBuf> {
    if cfg!(windows) {
        let mut roots = Vec::new();
        if let Some(path) = dirs::executable_dir() {
            roots.push(path);
        }
        if let Ok(path) = std::env::var("ProgramFiles") {
            roots.push(PathBuf::from(path));
        }
        if let Ok(path) = std::env::var("ProgramW6432") {
            roots.push(PathBuf::from(path));
        }
        if let Ok(path) = std::env::var("ProgramFiles(x86)") {
            roots.push(PathBuf::from(path));
        }
        if let Some(path) = dirs::data_local_dir() {
            roots.push(path.join("Programs"));
        }

        let mut out = Vec::new();
        for root in roots {
            out.push(root.join("nodejs").join("node.exe"));
            out.push(root.join("Node.js").join("node.exe"));
        }
        return out;
    }

    vec![
        PathBuf::from("/usr/local/bin/node"),
        PathBuf::from("/opt/homebrew/bin/node"),
        PathBuf::from("/usr/bin/node"),
    ]
}

fn resolve_command_on_path(command_name: &str) -> Vec<PathBuf> {
    let Some(path_var) = std::env::var_os("PATH") else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(command_name);
        if candidate.is_file() {
            out.push(candidate);
        }
    }
    out
}

fn run_command_first_line(file_name: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut process = Command::new(file_name)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let start = std::time::Instant::now();
    loop {
        if start.elapsed() >= timeout {
            let _ = process.kill();
            let _ = process.wait();
            return None;
        }
        match process.try_wait() {
            Ok(Some(status)) => {
                let output = process.wait_with_output().ok()?;
                if !status.success() {
                    return None;
                }
                return String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(ToOwned::to_owned);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
}

fn try_read_package_string(package_dir: &Path, property_name: &str) -> String {
    std::fs::read_to_string(package_dir.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| {
            value
                .get(property_name)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_default()
}

fn is_path_inside_directory(directory: &Path, path: &Path) -> bool {
    let Ok(directory) = directory.canonicalize() else {
        return false;
    };
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    path.starts_with(directory)
}

fn push_unique(out: &mut Vec<PathBuf>, seen: &mut HashSet<String>, path: PathBuf) {
    let key = path.display().to_string().to_ascii_lowercase();
    if seen.insert(key) {
        out.push(path);
    }
}
