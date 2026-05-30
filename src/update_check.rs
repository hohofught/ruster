use std::cmp::Ordering;
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

fn choose_primary_asset(assets: &[GithubReleaseAsset]) -> Option<&GithubReleaseAsset> {
    assets
        .iter()
        .find(|asset| asset.name.ends_with(".exe"))
        .or_else(|| assets.iter().find(|asset| asset.name.ends_with(".zip")))
        .or_else(|| assets.first())
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
}
