use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;

const GITHUB_OWNER: &str = "hohofught";
const GITHUB_REPO: &str = "ruster";
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/hohofught/ruster/releases/latest";

#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub latest_tag: String,
    pub release_url: String,
    pub published_at: Option<DateTime<Utc>>,
    pub update_available: bool,
    pub primary_asset_name: Option<String>,
    pub primary_asset_download_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Clone, Debug, Deserialize)]
struct GithubReleaseAsset {
    name: String,
    browser_download_url: String,
}

pub async fn check_latest_release(current_version: &str) -> anyhow::Result<UpdateInfo> {
    let current_version = normalize_release_version(current_version);
    if current_version.is_empty() {
        bail!("현재 앱 버전을 확인할 수 없습니다.");
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("GitHub 업데이트 확인용 HTTP client 생성 실패")?;

    let release = client
        .get(LATEST_RELEASE_URL)
        .header(
            USER_AGENT,
            format!("ruster/{current_version} (+https://github.com/{GITHUB_OWNER}/{GITHUB_REPO})"),
        )
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .context("GitHub 최신 릴리스 요청 실패")?
        .error_for_status()
        .context("GitHub 최신 릴리스 응답 오류")?
        .json::<GithubRelease>()
        .await
        .context("GitHub 최신 릴리스 응답 파싱 실패")?;

    let latest_tag = release.tag_name.trim().to_owned();
    let latest_version = normalize_release_version(&latest_tag);
    if latest_version.is_empty() {
        bail!("GitHub 최신 릴리스 태그가 비어 있습니다.");
    }

    let primary_asset = choose_primary_asset(&release.assets);
    Ok(UpdateInfo {
        update_available: compare_release_versions(&latest_version, &current_version)
            == Ordering::Greater,
        current_version,
        latest_version,
        latest_tag,
        release_url: release.html_url,
        published_at: release.published_at,
        primary_asset_name: primary_asset.map(|asset| asset.name.clone()),
        primary_asset_download_url: primary_asset.map(|asset| asset.browser_download_url.clone()),
    })
}

pub async fn download_and_launch_primary_asset(
    asset_name: &str,
    download_url: &str,
) -> anyhow::Result<PathBuf> {
    if !cfg!(windows) {
        bail!("릴리스 EXE 자동 실행은 현재 Windows만 지원합니다.");
    }

    let asset_name = asset_name.trim();
    if !is_primary_exe_asset_name(asset_name) {
        bail!("자동 다운로드/실행 가능한 Windows EXE 릴리스 자산이 아닙니다: {asset_name}");
    }
    if download_url.trim().is_empty() {
        bail!("릴리스 EXE 다운로드 URL이 비어 있습니다.");
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .context("릴리스 EXE 다운로드용 HTTP client 생성 실패")?;
    let bytes = client
        .get(download_url)
        .header(
            USER_AGENT,
            format!(
                "ruster-update/{asset_name} (+https://github.com/{GITHUB_OWNER}/{GITHUB_REPO})"
            ),
        )
        .send()
        .await
        .context("릴리스 EXE 다운로드 요청 실패")?
        .error_for_status()
        .context("릴리스 EXE 다운로드 응답 오류")?
        .bytes()
        .await
        .context("릴리스 EXE 다운로드 본문 읽기 실패")?;
    if bytes.is_empty() {
        bail!("릴리스 EXE 다운로드 결과가 비어 있습니다.");
    }

    let download_dir = std::env::temp_dir().join("ruster-updates");
    std::fs::create_dir_all(&download_dir)
        .with_context(|| format!("다운로드 폴더 생성 실패: {}", download_dir.display()))?;
    let download_path = unique_download_path(&download_dir, asset_name);
    std::fs::write(&download_path, bytes.as_ref())
        .with_context(|| format!("릴리스 EXE 저장 실패: {}", download_path.display()))?;

    Command::new(&download_path)
        .spawn()
        .with_context(|| format!("다운로드한 EXE 실행 실패: {}", download_path.display()))?;
    Ok(download_path)
}

fn choose_primary_asset(assets: &[GithubReleaseAsset]) -> Option<&GithubReleaseAsset> {
    assets
        .iter()
        .find(|asset| is_primary_exe_asset_name(&asset.name))
}

fn is_primary_exe_asset_name(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    lower.ends_with(".exe") && !lower.contains("portable")
}

fn unique_download_path(download_dir: &Path, asset_name: &str) -> PathBuf {
    let safe_name = safe_asset_file_name(asset_name);
    let candidate = download_dir.join(&safe_name);
    if !candidate.exists() {
        return candidate;
    }

    let stem = safe_name.strip_suffix(".exe").unwrap_or(&safe_name);
    for index in 1..1000 {
        let candidate = download_dir.join(format!("{stem}-{index}.exe"));
        if !candidate.exists() {
            return candidate;
        }
    }

    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    download_dir.join(format!("{stem}-{millis}.exe"))
}

fn safe_asset_file_name(asset_name: &str) -> String {
    let safe = asset_name
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.is_empty() || !safe.to_ascii_lowercase().ends_with(".exe") {
        "ruster-update.exe".to_owned()
    } else {
        safe
    }
}

fn normalize_release_version(value: &str) -> String {
    value
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['+', '-'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn compare_release_versions(left: &str, right: &str) -> Ordering {
    let left_parts = release_version_parts(left);
    let right_parts = release_version_parts(right);
    let len = left_parts.len().max(right_parts.len()).max(1);

    for index in 0..len {
        let left = left_parts.get(index).copied().unwrap_or(0);
        let right = right_parts.get(index).copied().unwrap_or(0);
        match left.cmp(&right) {
            Ordering::Equal => {}
            ordering => return ordering,
        }
    }

    Ordering::Equal
}

fn release_version_parts(value: &str) -> Vec<u64> {
    normalize_release_version(value)
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>()
                .parse::<u64>()
                .unwrap_or(0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_version_comparison_handles_v_prefix_and_zero_padding() {
        assert_eq!(
            compare_release_versions("v1.0.10", "1.0.2"),
            Ordering::Greater
        );
        assert_eq!(compare_release_versions("1.0", "v1.0.0"), Ordering::Equal);
        assert_eq!(compare_release_versions("2", "1.9.9"), Ordering::Greater);
        assert_eq!(compare_release_versions("1.0.0", "1.0.1"), Ordering::Less);
    }

    #[test]
    fn release_version_normalization_strips_tag_prefix_and_metadata() {
        assert_eq!(normalize_release_version(" v1.2.3 "), "1.2.3");
        assert_eq!(normalize_release_version("1.2.3+build.4"), "1.2.3");
        assert_eq!(normalize_release_version("v1.2.3-beta.1"), "1.2.3");
    }

    #[test]
    fn primary_asset_prefers_non_portable_exe() {
        let assets = vec![
            GithubReleaseAsset {
                name: "ruster-v1.0-windows-x86_64-portable.zip".to_owned(),
                browser_download_url: "portable".to_owned(),
            },
            GithubReleaseAsset {
                name: "SHA256SUMS.txt".to_owned(),
                browser_download_url: "sums".to_owned(),
            },
            GithubReleaseAsset {
                name: "ruster-v1.0-windows-x86_64.exe".to_owned(),
                browser_download_url: "exe".to_owned(),
            },
        ];

        assert_eq!(
            choose_primary_asset(&assets).map(|asset| asset.browser_download_url.as_str()),
            Some("exe")
        );
    }

    #[test]
    fn primary_asset_does_not_fallback_to_portable_zip() {
        let assets = vec![GithubReleaseAsset {
            name: "ruster-v1.0-windows-x86_64-portable.zip".to_owned(),
            browser_download_url: "portable".to_owned(),
        }];

        assert!(choose_primary_asset(&assets).is_none());
    }
}
