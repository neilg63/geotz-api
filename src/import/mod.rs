//! Rebuilds the SQLite database from the GeoNames export dumps.
//!
//! Two stages, usable separately:
//! 1. [`fetch`]: download what changed, unzip, delete the archives.
//! 2. [`db`]: build a fresh database file and swap it into place.

pub mod db;
pub mod fetch;
pub mod geonames;
pub mod names;
pub mod space;
pub mod timezonedb;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default locations of the published files. Both can be overridden, e.g.
/// to use a mirror.
pub const GEONAMES_DUMP_URL: &str = "https://download.geonames.org/export/dump/";
pub const TIMEZONEDB_FILES_URL: &str = "https://timezonedb.com/files/";

/// Makes a base URL safe to append a file name to.
pub fn base_url(url: &str) -> String {
    format!("{}/", url.trim().trim_end_matches('/'))
}

/// Feature codes present in the original MariaDB `toponyms` table.
const DEFAULT_FCODES: &str = include_str!("fcodes.txt");

/// A file published in the GeoNames dump directory.
#[derive(Debug, Clone)]
pub struct Source {
    /// Name on the server, e.g. `allCountries.zip` or `admin2Codes.txt`.
    pub remote: String,
    /// Name of the text file kept locally.
    pub local: String,
    /// Name of the file inside the archive, for zips.
    pub member: String,
}

impl Source {
    fn new(remote: &str) -> Self {
        let local = match remote.strip_suffix(".zip") {
            Some(stem) => format!("{stem}.txt"),
            None => remote.to_string(),
        };
        Source { remote: remote.to_string(), member: local.clone(), local }
    }

    /// Alternate names for one country. The archive member is `XX.txt`, the
    /// same name as the country's place file, so it is kept under another.
    fn alternate_names(cc: &str) -> Self {
        Source {
            remote: format!("alternatenames/{cc}.zip"),
            local: format!("alternatenames-{cc}.txt"),
            member: format!("{cc}.txt"),
        }
    }

    pub fn is_zip(&self) -> bool {
        self.remote.ends_with(".zip")
    }
}

/// Supporting files. These are published uncompressed.
pub const ADMIN1_FILE: &str = "admin1CodesASCII.txt";
pub const ADMIN2_FILE: &str = "admin2Codes.txt";
pub const FEATURE_CODES_FILE: &str = "featureCodes_en.txt";

/// Official languages per country; read for name selection, not stored.
pub const COUNTRY_INFO_FILE: &str = "countryInfo.txt";
pub const LANGUAGE_CODES_FILE: &str = "iso-languagecodes.txt";

const SUPPORT_FILES: [&str; 5] = [
    COUNTRY_INFO_FILE,
    LANGUAGE_CODES_FILE,
    ADMIN1_FILE,
    ADMIN2_FILE,
    FEATURE_CODES_FILE,
];

#[derive(Debug, Clone)]
pub struct Config {
    /// Unpacked GeoNames files.
    pub data_dir: PathBuf,
    /// Unpacked TimeZoneDB file.
    pub tz_dir: PathBuf,
    pub db_path: PathBuf,
    /// Base URL of the GeoNames dump directory, ending in `/`.
    pub geonames_url: String,
    /// Base URL of the TimeZoneDB files directory, ending in `/`.
    pub timezonedb_url: String,
    /// Empty: use `allCountries.zip`. Otherwise one `XX.zip` per code.
    pub countries: Vec<String>,
    pub fcodes: HashSet<String>,
    /// Languages whose alternate names become search names.
    pub languages: names::Languages,
    /// Keep rows whose feature code starts with `P` even without a population.
    pub keep_unpopulated: bool,
    pub force: bool,
    pub offline: bool,
    /// Skip the free-disk-space checks before downloading and loading.
    pub skip_space_check: bool,
    /// Abort when the place count falls by more than this percentage
    /// compared with the previous import made with the same settings.
    pub max_drop_percent: f64,
}

impl Config {
    pub fn support_sources(&self) -> Vec<Source> {
        SUPPORT_FILES.iter().map(|f| Source::new(f)).collect()
    }

    pub fn place_sources(&self) -> Vec<Source> {
        if self.countries.is_empty() {
            vec![Source::new("allCountries.zip")]
        } else {
            self.countries
                .iter()
                .map(|cc| Source::new(&format!("{}.zip", cc.to_uppercase())))
                .collect()
        }
    }

    /// Language-tagged alternate names, the source of search names.
    pub fn name_sources(&self) -> Vec<Source> {
        if self.countries.is_empty() {
            vec![Source::new("alternateNamesV2.zip")]
        } else {
            self.countries.iter().map(|cc| Source::alternate_names(&cc.to_uppercase())).collect()
        }
    }

    /// Every GeoNames file the place import reads.
    pub fn all_sources(&self) -> Vec<Source> {
        let mut sources = self.support_sources();
        sources.extend(self.place_sources());
        sources.extend(self.name_sources());
        sources
    }

    pub fn local_path(&self, file: &str) -> PathBuf {
        self.data_dir.join(file)
    }
}

/// Reads a whitespace-separated list of feature codes; `#` starts a comment.
pub fn parse_fcodes(text: &str) -> HashSet<String> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .flat_map(|line| line.split_whitespace())
        .map(|code| code.to_uppercase())
        .collect()
}

pub fn default_fcodes() -> HashSet<String> {
    parse_fcodes(DEFAULT_FCODES)
}

pub fn load_fcodes(path: &Path) -> Result<HashSet<String>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read feature code list {}", path.display()))?;
    let codes = parse_fcodes(&text);
    anyhow::ensure!(!codes.is_empty(), "{} lists no feature codes", path.display());
    Ok(codes)
}

/// Downloads and unpacks everything the build stage needs.
/// Returns how many files were newly downloaded.
///
/// `need_files` forces files discarded after an earlier import to be
/// downloaded again even if nothing has changed, for a rebuild.
pub async fn fetch_all(config: &Config, need_files: bool) -> Result<usize> {
    let fetcher = fetch::Fetcher::new(config)?;
    let downloaded = fetcher.ensure_all(&config.all_sources(), need_files).await?;
    fetcher.save_manifest()?;
    Ok(downloaded)
}

/// Deletes the unpacked GeoNames files to free disk space. They are
/// downloaded again the next time an import needs them. Returns bytes freed.
pub fn discard_sources(config: &Config) -> Result<u64> {
    fetch::Fetcher::new(config)?.discard(&config.all_sources())
}

/// Compares every source file with the server without downloading anything.
/// Returns how many are missing or out of date.
pub async fn check_all(config: &Config) -> Result<usize> {
    let fetcher = fetch::Fetcher::new(config)?;
    let mut stale = 0;
    for source in &config.all_sources() {
        match fetcher.check(source).await? {
            fetch::Freshness::Current => println!("  current  {}", source.local),
            fetch::Freshness::Missing => {
                stale += 1;
                println!("  missing  {}", source.local);
            }
            fetch::Freshness::Changed(date) => {
                stale += 1;
                let date = date.unwrap_or_else(|| "date unknown".into());
                println!("  changed  {} (server copy: {date})", source.local);
            }
        }
    }
    Ok(stale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_list_has_the_75_original_codes() {
        let codes = default_fcodes();
        assert_eq!(codes.len(), 75);
        assert!(codes.contains("PPLA2") && codes.contains("ADM1") && codes.contains("AREA"));
    }

    #[test]
    fn fcode_lists_allow_comments_and_any_whitespace() {
        let codes = parse_fcodes("ppl PPLA # capitals\n\n  adm1\t# first order\n");
        assert_eq!(codes, HashSet::from(["PPL".into(), "PPLA".into(), "ADM1".into()]));
    }

    #[test]
    fn base_urls_end_in_one_slash() {
        assert_eq!(base_url("https://mirror.example/dump"), "https://mirror.example/dump/");
        assert_eq!(base_url(" https://mirror.example/dump// "), "https://mirror.example/dump/");
        assert_eq!(base_url(GEONAMES_DUMP_URL), GEONAMES_DUMP_URL);
    }

    #[test]
    fn zip_sources_map_to_text_files() {
        let zip = Source::new("GB.zip");
        assert!(zip.is_zip());
        assert_eq!(zip.local, "GB.txt");
        let names = Source::alternate_names("GB");
        assert_eq!((names.remote.as_str(), names.member.as_str()), ("alternatenames/GB.zip", "GB.txt"));
        assert_eq!(names.local, "alternatenames-GB.txt");
        let plain = Source::new(ADMIN2_FILE);
        assert!(!plain.is_zip());
        assert_eq!(plain.local, ADMIN2_FILE);
    }
}
