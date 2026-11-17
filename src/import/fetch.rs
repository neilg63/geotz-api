//! Downloads GeoNames dump files, unpacks archives and deletes them.
//!
//! A file is fetched again only when the server copy has changed. What was
//! last fetched is recorded in `manifest.json` beside the data, because the
//! archives themselves are not kept.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::header::{ETAG, HeaderMap, LAST_MODIFIED};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use super::{Config, GEONAMES_DUMP_URL, Source};

const MANIFEST_FILE: &str = "manifest.json";
const USER_AGENT: &str = concat!("geotz-import/", env!("CARGO_PKG_VERSION"));
const ATTEMPTS: u32 = 3;

/// Identity of a remote file as reported by the server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteVersion {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub size: Option<u64>,
}

impl RemoteVersion {
    fn from_headers(headers: &HeaderMap, size: Option<u64>) -> Self {
        let text = |name| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        RemoteVersion { etag: text(ETAG), last_modified: text(LAST_MODIFIED), size }
    }

    fn modified_time(&self) -> Option<SystemTime> {
        httpdate::parse_http_date(self.last_modified.as_deref()?).ok()
    }

    fn is_known(&self) -> bool {
        self.etag.is_some() || self.last_modified.is_some()
    }
}

/// Result of comparing a local file with the server copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    Current,
    Missing,
    /// The server copy differs; holds its Last-Modified date if reported.
    Changed(Option<String>),
}

/// The publisher's last-modified date (`YYYY-MM-DD`) of a file as it was
/// when last fetched, from the manifest. `None` if it was never recorded.
pub fn source_date(config: &Config, source: &Source) -> Option<String> {
    let text = fs::read_to_string(config.local_path(MANIFEST_FILE)).ok()?;
    let manifest: Manifest = serde_json::from_str(&text).ok()?;
    let modified = manifest.get(&source.remote)?.modified_time()?;
    let date: chrono::DateTime<chrono::Utc> = modified.into();
    Some(date.format("%Y-%m-%d").to_string())
}

type Manifest = BTreeMap<String, RemoteVersion>;

pub struct Fetcher<'a> {
    config: &'a Config,
    client: reqwest::Client,
    manifest: std::sync::Mutex<Manifest>,
}

impl<'a> Fetcher<'a> {
    pub fn new(config: &'a Config) -> Result<Self> {
        fs::create_dir_all(&config.data_dir)
            .with_context(|| format!("cannot create {}", config.data_dir.display()))?;
        let manifest = match fs::read_to_string(config.local_path(MANIFEST_FILE)) {
            Ok(text) => serde_json::from_str(&text).context("manifest.json is not valid")?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Manifest::new(),
            Err(e) => return Err(e).context("cannot read manifest.json"),
        };
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Fetcher { config, client, manifest: std::sync::Mutex::new(manifest) })
    }

    pub fn save_manifest(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(&*self.manifest.lock().unwrap())?;
        fs::write(self.config.local_path(MANIFEST_FILE), text).context("cannot write manifest.json")
    }

    /// Reports whether the server copy of `source` differs from the local
    /// one, without downloading it.
    pub async fn check(&self, source: &Source) -> Result<Freshness> {
        let target = self.config.local_path(&source.local);
        if !target.is_file() {
            return Ok(Freshness::Missing);
        }
        let url = format!("{GEONAMES_DUMP_URL}{}", source.remote);
        let remote = self.head(&url).await.with_context(|| format!("cannot reach {url}"))?;
        if self.is_current(source, &target, &remote) {
            Ok(Freshness::Current)
        } else {
            Ok(Freshness::Changed(remote.last_modified))
        }
    }

    /// Makes sure the local text file for `source` exists and is current.
    /// Returns `true` when a new copy was downloaded.
    pub async fn ensure(&self, source: &Source) -> Result<bool> {
        let target = self.config.local_path(&source.local);
        let present = target.is_file();

        if self.config.offline {
            if !present {
                bail!("{} is missing and --offline was given", target.display());
            }
            println!("  kept  {} (offline)", source.local);
            return Ok(false);
        }

        let url = format!("{GEONAMES_DUMP_URL}{}", source.remote);
        let remote = match self.head(&url).await {
            Ok(version) => version,
            Err(e) if present => {
                println!("  kept  {} (cannot check for updates: {e})", source.local);
                return Ok(false);
            }
            Err(e) => return Err(e).with_context(|| format!("cannot reach {url}")),
        };

        if present && !self.config.force && self.is_current(source, &target, &remote) {
            println!("  kept  {} (unchanged)", source.local);
            self.record(source, remote);
            return Ok(false);
        }

        let download = self.config.local_path(&format!("{}.part", source.remote.replace('/', "_")));
        let fetched = self.download(&url, &download).await?;
        if source.is_zip() {
            let (zip, member, out) = (download.clone(), source.member.clone(), target.clone());
            tokio::task::spawn_blocking(move || unzip_member(&zip, &member, &out))
                .await?
                .with_context(|| format!("cannot unpack {}", source.remote))?;
            fs::remove_file(&download)?;
        } else {
            fs::rename(&download, &target)?;
        }
        println!("  fetch {} ({})", source.local, human_size(fetched));
        self.record(source, remote);
        // Saved after each file so an interrupted run does not repeat finished downloads.
        self.save_manifest()?;
        Ok(true)
    }

    fn record(&self, source: &Source, version: RemoteVersion) {
        self.manifest.lock().unwrap().insert(source.remote.clone(), version);
    }

    fn is_current(&self, source: &Source, target: &Path, remote: &RemoteVersion) -> bool {
        if !remote.is_known() {
            return false;
        }
        if let Some(recorded) = self.manifest.lock().unwrap().get(&source.remote) {
            return recorded == remote;
        }
        // No record (e.g. files unpacked by hand): trust a local copy that is
        // newer than the server's.
        let local_time = fs::metadata(target).and_then(|m| m.modified()).ok();
        match (local_time, remote.modified_time()) {
            (Some(local), Some(server)) => local >= server,
            _ => false,
        }
    }

    async fn head(&self, url: &str) -> Result<RemoteVersion> {
        let response = self.client.head(url).send().await?.error_for_status()?;
        let size = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        Ok(RemoteVersion::from_headers(response.headers(), size))
    }

    async fn download(&self, url: &str, dest: &Path) -> Result<u64> {
        let mut last_error = None;
        for attempt in 1..=ATTEMPTS {
            match self.download_once(url, dest).await {
                Ok(bytes) => return Ok(bytes),
                Err(e) => {
                    if attempt < ATTEMPTS {
                        println!("  retry {url} after error: {e}");
                        tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
                    }
                    last_error = Some(e);
                }
            }
        }
        let _ = fs::remove_file(dest);
        Err(last_error.unwrap()).with_context(|| format!("download failed: {url}"))
    }

    async fn download_once(&self, url: &str, dest: &Path) -> Result<u64> {
        let response = self.client.get(url).send().await?.error_for_status()?;
        let expected = response.content_length();
        let mut file = tokio::fs::File::create(dest).await?;
        let mut stream = response.bytes_stream();
        let mut written = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        file.flush().await?;
        if let Some(expected) = expected
            && expected != written
        {
            bail!("incomplete download: {written} of {expected} bytes");
        }
        Ok(written)
    }
}

/// Extracts one named member of a zip archive to `out`, via a temporary file
/// so a failed extraction never leaves a truncated text file behind.
fn unzip_member(zip_path: &Path, member: &str, out: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(fs::File::open(zip_path)?)?;
    let mut entry = archive
        .by_name(member)
        .with_context(|| format!("archive has no member named {member}"))?;
    let staging = out.with_extension("txt.part");
    let mut file = fs::File::create(&staging)?;
    io::copy(&mut entry, &mut file)?;
    file.sync_all()?;
    fs::rename(&staging, out)?;
    Ok(())
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_readable() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(400 * 1024 * 1024), "400.0 MiB");
    }

    #[test]
    fn last_modified_parses_to_a_time() {
        let version = RemoteVersion {
            last_modified: Some("Mon, 16 Nov 2026 02:41:07 GMT".into()),
            ..Default::default()
        };
        assert!(version.modified_time().is_some());
        assert!(version.is_known());
        assert!(!RemoteVersion::default().is_known());
    }
}
