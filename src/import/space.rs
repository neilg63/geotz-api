//! Estimates the disk space an import needs and checks it is available
//! before anything is downloaded.

use std::path::Path;

use anyhow::{Result, bail};

use super::fetch::human_size;

/// Unpacked size of a GeoNames zip relative to the archive. The large files
/// measure 3.8 to 4.25; rounding up keeps the estimate on the safe side.
const UNZIP_RATIO: f64 = 4.5;

/// Size of the database relative to the unpacked source files it is built
/// from (about 0.4 GB from 2.6 GB with the default settings).
const DATABASE_RATIO: f64 = 0.16;

/// Extra allowed on top of every estimate.
const MARGIN: f64 = 1.05;

/// One file that is about to be downloaded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Download {
    /// Size on the server; `None` if the server did not say.
    pub remote_size: Option<u64>,
    pub is_zip: bool,
    /// Size of the local copy it will replace, 0 if there is none.
    pub existing: u64,
}

impl Download {
    /// Expected size once unpacked.
    pub fn unpacked(&self) -> u64 {
        match (self.remote_size, self.is_zip) {
            (Some(size), true) => (size as f64 * UNZIP_RATIO) as u64,
            (Some(size), false) => size,
            (None, _) => self.existing,
        }
    }
}

/// The most extra disk space in use at any moment while the files are
/// fetched one after another.
///
/// A zip is on disk together with its unpacked contents until unpacking
/// finishes; a file being replaced stays until its replacement is complete.
pub fn download_peak(downloads: &[Download]) -> u64 {
    let mut used: i64 = 0;
    let mut peak: i64 = 0;
    for download in downloads {
        let unpacked = download.unpacked() as i64;
        let archive = if download.is_zip { download.remote_size.unwrap_or(0) as i64 } else { 0 };
        peak = peak.max(used + archive + unpacked);
        used += unpacked - download.existing as i64;
    }
    peak.max(0) as u64
}

/// Space the load step needs: room for the database to grow to its expected
/// size, plus as much again for the write-ahead log, which holds the whole
/// new data set until the import commits.
pub fn load_allowance(source_bytes: u64, current_database: u64) -> u64 {
    let expected = ((source_bytes as f64 * DATABASE_RATIO) as u64).max(current_database);
    expected + expected.saturating_sub(current_database)
}

/// An estimate with the safety margin added, as compared and reported.
pub fn with_margin(bytes: u64) -> u64 {
    (bytes as f64 * MARGIN) as u64
}

/// The nearest existing directory at or above `path`, for measuring a
/// location that has not been created yet.
fn existing_ancestor(path: &Path) -> &Path {
    path.ancestors().find(|p| p.is_dir()).unwrap_or(Path::new("."))
}

/// Fails with an explanation when `dir`'s filesystem has less than `needed`
/// bytes free. `parts` describes what the space is for.
pub fn require(dir: &Path, needed: u64, parts: &str) -> Result<()> {
    if needed == 0 {
        return Ok(());
    }
    let needed = with_margin(needed);
    // If the filesystem cannot be measured, carry on rather than block.
    let Ok(available) = fs4::available_space(existing_ancestor(dir)) else { return Ok(()) };
    if available < needed {
        bail!(
            "not enough free disk space in {}: this needs about {} ({parts}) but only {} is available. \
             Free some space, or pass --no-space-check to try anyway",
            dir.display(),
            human_size(needed),
            human_size(available)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;

    fn zip(size_mb: u64, existing_mb: u64) -> Download {
        Download { remote_size: Some(size_mb * MB), is_zip: true, existing: existing_mb * MB }
    }

    #[test]
    fn first_download_peaks_with_the_last_archive_still_present() {
        // 400 MB and 200 MB archives: 1800 + 900 unpacked, plus the second zip.
        let peak = download_peak(&[zip(400, 0), zip(200, 0)]);
        assert_eq!(peak, (1800 + 900 + 200) * MB);
    }

    #[test]
    fn replacing_files_needs_room_for_one_new_copy_at_a_time() {
        // Old copies the same size as the new ones: after each swap nothing
        // extra is used, so the peak is one archive plus one unpacked copy.
        let peak = download_peak(&[zip(400, 1800), zip(200, 900)]);
        assert_eq!(peak, (400 + 1800) * MB);
    }

    #[test]
    fn plain_files_and_unknown_sizes() {
        let plain = Download { remote_size: Some(3 * MB), is_zip: false, existing: 0 };
        assert_eq!(download_peak(&[plain]), 3 * MB);
        let unknown = Download { remote_size: None, is_zip: true, existing: 50 * MB };
        assert_eq!(unknown.unpacked(), 50 * MB);
        assert_eq!(download_peak(&[]), 0);
    }

    #[test]
    fn loading_needs_the_database_twice_the_first_time() {
        // 2600 MB of sources -> about 416 MB database, plus the same in WAL.
        let first = load_allowance(2600 * MB, 0);
        assert!((first as f64 / MB as f64 - 832.0).abs() < 2.0, "{first}");
        // An existing 400 MB database only needs the WAL.
        let later = load_allowance(2600 * MB, 400 * MB);
        assert!((later as f64 / MB as f64 - 432.0).abs() < 2.0, "{later}");
        // A database larger than the estimate (other settings) is trusted.
        assert_eq!(load_allowance(100 * MB, 900 * MB), 900 * MB);
    }

    #[test]
    fn space_check_passes_and_fails() {
        let here = std::env::temp_dir();
        assert!(require(&here, 0, "nothing").is_ok());
        assert!(require(&here, 1, "one byte").is_ok());
        let error = require(&here.join("not/created/yet"), u64::MAX / 4, "far too much").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("not enough free disk space") && text.contains("--no-space-check"), "{text}");
    }
}
