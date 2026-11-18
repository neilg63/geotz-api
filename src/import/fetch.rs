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

use super::{Config, Source, space};

const MANIFEST_FILE: &str = "manifest.json";
const USER_AGENT: &str = concat!("geotz-import/", env!("CARGO_PKG_VERSION"));
const ATTEMPTS: u32 = 3;

/// Identity of a remote file as reported by the server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteVersion {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub size: Option<u64>,
    /// The local copy was deleted on purpose after being imported
    /// (`--discard-sources`); the database holds this version.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub discarded: bool,
}

impl RemoteVersion {
    fn from_headers(headers: &HeaderMap, size: Option<u64>) -> Self {
        let text = |name| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        RemoteVersion {
            etag: text(ETAG),
            last_modified: text(LAST_MODIFIED),
            size,
            discarded: false,
        }
    }

    /// Same published file, whatever has happened to the local copy.
    fn same_file(&self, other: &RemoteVersion) -> bool {
        (&self.etag, &self.last_modified, self.size)
            == (&other.etag, &other.last_modified, other.size)
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
        Ok(Fetcher {
            config,
            client,
            manifest: std::sync::Mutex::new(manifest),
        })
    }

    pub fn save_manifest(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(&*self.manifest.lock().unwrap())?;
        fs::write(self.config.local_path(MANIFEST_FILE), text).context("cannot write manifest.json")
    }

    /// Reports whether the server copy of `source` differs from the local
    /// one, without downloading it.
    pub async fn check(&self, source: &Source) -> Result<Freshness> {
        let target = self.config.local_path(&source.local);
        let present = target.is_file();
        if !present && !self.was_discarded(source) {
            return Ok(Freshness::Missing);
        }
        let url = format!("{}{}", self.config.geonames_url, source.remote);
        let remote = self
            .head(&url)
            .await
            .with_context(|| format!("cannot reach {url}"))?;
        let current = if present {
            self.is_current(source, &target, &remote)
        } else {
            self.discarded_copy_is(source, &remote)
        };
        if current {
            Ok(Freshness::Current)
        } else {
            Ok(Freshness::Changed(remote.last_modified))
        }
    }

    fn was_discarded(&self, source: &Source) -> bool {
        self.manifest
            .lock()
            .unwrap()
            .get(&source.remote)
            .is_some_and(|v| v.discarded)
    }

    /// True when the local copy was discarded after import and the server
    /// still publishes that same version.
    fn discarded_copy_is(&self, source: &Source, remote: &RemoteVersion) -> bool {
        remote.is_known()
            && self
                .manifest
                .lock()
                .unwrap()
                .get(&source.remote)
                .is_some_and(|recorded| recorded.discarded && recorded.same_file(remote))
    }

    /// Makes sure the local files are current. Returns how many were
    /// downloaded.
    ///
    /// Files discarded after an earlier import are left absent while nothing
    /// has changed. As soon as any file has changed, or `need_files` says a
    /// rebuild is coming regardless, every missing file is downloaded again,
    /// because a rebuild reads them all.
    pub async fn ensure_all(&self, sources: &[Source], need_files: bool) -> Result<usize> {
        if self.config.offline {
            for source in sources {
                let target = self.config.local_path(&source.local);
                if !target.is_file() {
                    bail!("{} is missing and --offline was given", target.display());
                }
                println!("  kept  {} (offline)", source.local);
            }
            return Ok(0);
        }

        // What the server has now; `None` when it cannot be reached.
        let mut remotes = Vec::with_capacity(sources.len());
        for source in sources {
            let url = format!("{}{}", self.config.geonames_url, source.remote);
            remotes.push(self.head(&url).await);
        }

        #[derive(PartialEq)]
        enum State {
            Current,
            DiscardedCurrent,
            Unverified,
            Stale,
        }
        let states: Vec<State> = sources
            .iter()
            .zip(&remotes)
            .map(|(source, remote)| {
                let target = self.config.local_path(&source.local);
                let present = target.is_file();
                match remote {
                    Err(_) if present => State::Unverified,
                    Err(_) if self.was_discarded(source) => State::DiscardedCurrent,
                    Err(_) => State::Stale,
                    Ok(_) if self.config.force => State::Stale,
                    Ok(remote) if present && self.is_current(source, &target, remote) => {
                        State::Current
                    }
                    Ok(remote) if !present && self.discarded_copy_is(source, remote) => {
                        State::DiscardedCurrent
                    }
                    Ok(_) => State::Stale,
                }
            })
            .collect();
        let need_files = need_files || states.contains(&State::Stale);

        if !self.config.skip_space_check {
            self.require_space(sources, &remotes, |i| match states[i] {
                State::Current | State::Unverified => false,
                State::DiscardedCurrent => need_files,
                State::Stale => true,
            })?;
        }

        let mut downloaded = 0;
        for ((source, remote), state) in sources.iter().zip(remotes).zip(states) {
            match (state, remote) {
                (State::Current, Ok(remote)) => {
                    println!("  kept  {} (unchanged)", source.local);
                    self.record(source, remote);
                }
                (State::Unverified, Err(e)) => {
                    println!("  kept  {} (cannot check for updates: {e})", source.local);
                }
                (State::DiscardedCurrent, _) if !need_files => {
                    println!(
                        "  kept  {} (unchanged; local copy discarded after import)",
                        source.local
                    );
                }
                (_, Ok(remote)) => {
                    self.fetch(source, remote).await?;
                    downloaded += 1;
                }
                (_, Err(e)) => {
                    let url = format!("{}{}", self.config.geonames_url, source.remote);
                    return Err(e).with_context(|| format!("cannot reach {url}"));
                }
            }
        }
        Ok(downloaded)
    }

    /// Checks there is room for the downloads about to happen and for
    /// loading them into the database afterwards.
    fn require_space(
        &self,
        sources: &[Source],
        remotes: &[Result<RemoteVersion>],
        will_download: impl Fn(usize) -> bool,
    ) -> Result<()> {
        let local_size = |source: &Source| {
            fs::metadata(self.config.local_path(&source.local))
                .map(|m| m.len())
                .unwrap_or(0)
        };
        let mut downloads = Vec::new();
        let mut source_bytes = 0;
        for (i, source) in sources.iter().enumerate() {
            if !will_download(i) {
                source_bytes += local_size(source);
                continue;
            }
            let download = space::Download {
                remote_size: remotes[i].as_ref().ok().and_then(|r| r.size),
                is_zip: source.is_zip(),
                existing: local_size(source),
            };
            source_bytes += download.unpacked();
            downloads.push(download);
        }
        if downloads.is_empty() {
            return Ok(());
        }
        let fetching = space::download_peak(&downloads);
        let database = fs::metadata(&self.config.db_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let loading = space::load_allowance(source_bytes, database);
        let parts = format!(
            "{} while downloading and unpacking, {} while loading the database",
            human_size(fetching),
            human_size(loading)
        );
        println!(
            "  disk space needed: about {} ({parts})",
            human_size(space::with_margin(fetching + loading))
        );
        space::require(&self.config.data_dir, fetching + loading, &parts)
    }

    /// Downloads one file, unpacking it if it is an archive.
    async fn fetch(&self, source: &Source, remote: RemoteVersion) -> Result<()> {
        let target = self.config.local_path(&source.local);
        let url = format!("{}{}", self.config.geonames_url, source.remote);
        let download = self
            .config
            .local_path(&format!("{}.part", source.remote.replace('/', "_")));
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
        self.save_manifest()
    }

    /// Deletes the local copies of `sources` and notes in the manifest that
    /// this was deliberate. Returns the bytes freed.
    pub fn discard(&self, sources: &[Source]) -> Result<u64> {
        let mut freed = 0;
        for source in sources {
            let target = self.config.local_path(&source.local);
            // symlink_metadata: remove a link, not what it points at.
            let Ok(info) = fs::symlink_metadata(&target) else {
                continue;
            };
            fs::remove_file(&target)
                .with_context(|| format!("cannot delete {}", target.display()))?;
            freed += info.len();
            if let Some(recorded) = self.manifest.lock().unwrap().get_mut(&source.remote) {
                recorded.discarded = true;
            }
        }
        self.save_manifest()?;
        Ok(freed)
    }

    fn record(&self, source: &Source, version: RemoteVersion) {
        self.manifest
            .lock()
            .unwrap()
            .insert(source.remote.clone(), version);
    }

    fn is_current(&self, source: &Source, target: &Path, remote: &RemoteVersion) -> bool {
        if !remote.is_known() {
            return false;
        }
        if let Some(recorded) = self.manifest.lock().unwrap().get(&source.remote) {
            return recorded.same_file(remote);
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

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
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
