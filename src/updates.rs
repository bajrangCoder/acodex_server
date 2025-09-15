use reqwest::Client;
use semver::Version;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;

const GITHUB_API_URL: &str =
    "https://api.github.com/repos/bajrangCoder/acodex_server/releases/latest";
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const CACHE_FILE: &str = ".axs_update_cache";

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

struct UpdateCache {
    last_check: SystemTime,
    latest_version: String,
}

pub struct UpdateChecker {
    client: Client,
    current_version: Version,
}

impl UpdateChecker {
    pub fn new(current_version: &str) -> Self {
        Self {
            client: Client::new(),
            current_version: Version::parse(current_version).unwrap(),
        }
    }

    async fn get_cache() -> Option<UpdateCache> {
        let cache_path = Self::get_cache_path()?;
        let content = fs::read_to_string(cache_path).await.ok()?;
        let parts: Vec<&str> = content.split(',').collect();
        if parts.len() != 2 {
            return None;
        }

        let timestamp = parts[0].parse::<u64>().ok()?;
        Some(UpdateCache {
            last_check: SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp),
            latest_version: parts[1].to_string(),
        })
    }

    async fn save_cache(version: &str) -> tokio::io::Result<()> {
        if let Some(cache_path) = Self::get_cache_path() {
            if let Some(parent) = cache_path.parent() {
                fs::create_dir_all(parent).await?;
            }

            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let content = format!("{now},{version}");
            fs::write(cache_path, content).await?;
        }
        Ok(())
    }

    fn get_cache_path() -> Option<PathBuf> {
        match std::env::var_os("HOME") {
            Some(home) => {
                let mut path = PathBuf::from(home);
                path.push(".cache");
                path.push("axs");
                path.push(CACHE_FILE);
                Some(path)
            }
            None => match std::env::var_os("TMPDIR").or_else(|| std::env::var_os("TMP")) {
                Some(tmp) => {
                    let mut path = PathBuf::from(tmp);
                    path.push("axs");
                    path.push(CACHE_FILE);
                    Some(path)
                }
                None => None,
            },
        }
    }

    pub async fn check_update(&self) -> Result<Option<String>, Box<dyn std::error::Error>> {
        // Check cache first
        if let Some(cache) = Self::get_cache().await {
            let elapsed = SystemTime::now()
                .duration_since(cache.last_check)
                .unwrap_or(UPDATE_CHECK_INTERVAL);
            if elapsed < UPDATE_CHECK_INTERVAL {
                let cached_version = Version::parse(&cache.latest_version)?;
                if cached_version > self.current_version {
                    return Ok(Some(cache.latest_version));
                }
                return Ok(None);
            }
        }

        // Fetch latest release from GitHub
        let release: GithubRelease = self
            .client
            .get(GITHUB_API_URL)
            .header("User-Agent", "axs-update-checker")
            .send()
            .await?
            .json()
            .await?;

        let latest_version = Version::parse(release.tag_name.trim_start_matches('v'))?;
        Self::save_cache(&latest_version.to_string()).await?;

        if latest_version > self.current_version {
            Ok(Some(release.tag_name))
        } else {
            Ok(None)
        }
    }

    pub async fn update(&self) -> Result<(), Box<dyn std::error::Error>> {
        let release: GithubRelease = self
            .client
            .get(GITHUB_API_URL)
            .header("User-Agent", "axs-update-checker")
            .send()
            .await?
            .json()
            .await?;

        // Detect current architecture
        let arch = match std::env::consts::ARCH {
            "arm" => "android-armv7",
            "aarch64" => "android-arm64",
            _ => return Err("Unsupported architecture".into()),
        };

        let asset = release
            .assets
            .iter()
            .find(|a| a.name == format!("axs-{arch}"))
            .ok_or("No matching binary found")?;

        // Download binary
        let response = self
            .client
            .get(&asset.browser_download_url)
            .send()
            .await?
            .bytes()
            .await?;

        let current_exe = std::env::current_exe()?;
        let temp_path = current_exe.with_extension("new");

        let mut file = File::create(&temp_path).await?;
        file.write_all(&response).await?;
        file.sync_all().await?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = fs::metadata(&temp_path).await?;
            let mut perms = metadata.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&temp_path, perms).await?;
        }

        // Replace old binary with new one
        fs::rename(temp_path, current_exe).await?;

        Ok(())
    }
}
