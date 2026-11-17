//! Imports TimeZoneDB's `time_zone` and `country` tables, stored here as
//! `time_zones` and `countries`.
//!
//! The source is a MySQL dump inside a small zip. The server sends no ETag or
//! Last-Modified header, so a release is identified by the CRC-32 of the SQL
//! file recorded in the zip. The archive is only ever held in memory.

use std::fs;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sqlx::{Connection, QueryBuilder, Sqlite};

use super::Config;
use super::db;

/// Name of the archive under the TimeZoneDB files URL.
const ZIP_FILE: &str = "TimeZoneDB.sql.zip";

/// Full address of the archive for these settings.
pub fn zip_url(config: &Config) -> String {
    format!("{}{ZIP_FILE}", config.timezonedb_url)
}
const SQL_FILE: &str = "time_zone.sql";
const MANIFEST_FILE: &str = "manifest.json";
const USER_AGENT: &str = concat!("geotz-import/", env!("CARGO_PKG_VERSION"));
const BATCH_ROWS: usize = 2000;

/// One offset period: `gmt_offset` applies in `zone_name` from `time_start`
/// (Unix seconds) until the next row for the same zone.
#[derive(Debug, Clone, PartialEq)]
pub struct ZonePeriod {
    pub zone_name: String,
    pub country_code: String,
    pub abbreviation: String,
    pub time_start: i64,
    pub gmt_offset: i32,
    pub dst: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Country {
    pub country_code: String,
    pub country_name: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct Dump {
    pub countries: Vec<Country>,
    pub periods: Vec<ZonePeriod>,
}

/// Identity of the release last fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// CRC-32 of `time_zone.sql`, from the zip directory.
    pub crc32: u32,
    /// Date the SQL file was generated, `YYYY-MM-DD`, from the zip directory.
    pub date: Option<String>,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub periods: u64,
    pub zones: u64,
    pub countries: u64,
    pub release_date: Option<String>,
}

pub enum Outcome {
    Imported(Stats),
    UpToDate,
}

// ---------------------------------------------------------------- SQL parsing

/// Reads the rows of every `INSERT INTO ... VALUES` statement for the
/// `country` and `time_zone` tables. Other statements are ignored.
pub fn parse_dump(sql: &str) -> Result<Dump> {
    let bytes = sql.as_bytes();
    let mut dump = Dump::default();
    for (start, marker) in sql.match_indices("INSERT INTO ") {
        let mut pos = start + marker.len();
        let name_end = pos + bytes[pos..].iter().position(|b| b.is_ascii_whitespace() || *b == b'(')
            .context("unterminated INSERT statement")?;
        let table = sql[pos..name_end].trim_matches('`');
        if table != "country" && table != "time_zone" {
            continue;
        }
        pos = name_end + sql[name_end..].find("VALUES").context("INSERT without VALUES")? + 6;
        loop {
            while pos < bytes.len() && (bytes[pos].is_ascii_whitespace() || bytes[pos] == b',') {
                pos += 1;
            }
            match bytes.get(pos) {
                Some(b'(') => {
                    let (fields, next) = parse_tuple(bytes, pos + 1)?;
                    pos = next;
                    if table == "country" {
                        dump.countries.push(country_row(fields)?);
                    } else {
                        dump.periods.push(period_row(fields)?);
                    }
                }
                Some(b';') | None => break,
                Some(other) => bail!("unexpected character {:?} in {table} rows", *other as char),
            }
        }
    }
    Ok(dump)
}

/// Parses the fields of one `(...)` row starting just after the opening
/// bracket. Returns the fields and the position after the closing bracket.
fn parse_tuple(bytes: &[u8], mut pos: usize) -> Result<(Vec<String>, usize)> {
    let mut fields = Vec::new();
    loop {
        while bytes.get(pos).is_some_and(|b| b.is_ascii_whitespace()) {
            pos += 1;
        }
        let mut field = Vec::new();
        if bytes.get(pos) == Some(&b'\'') {
            pos += 1;
            loop {
                match bytes.get(pos).context("unterminated string in SQL row")? {
                    b'\\' => {
                        let escaped = *bytes.get(pos + 1).context("dangling escape in SQL row")?;
                        field.push(match escaped {
                            b'n' => b'\n',
                            b'r' => b'\r',
                            b't' => b'\t',
                            b'0' => 0,
                            other => other,
                        });
                        pos += 2;
                    }
                    b'\'' if bytes.get(pos + 1) == Some(&b'\'') => {
                        field.push(b'\'');
                        pos += 2;
                    }
                    b'\'' => {
                        pos += 1;
                        break;
                    }
                    other => {
                        field.push(*other);
                        pos += 1;
                    }
                }
            }
        } else {
            while let Some(b) = bytes.get(pos).filter(|b| **b != b',' && **b != b')') {
                field.push(*b);
                pos += 1;
            }
        }
        fields.push(String::from_utf8(field).context("SQL row is not valid UTF-8")?.trim().to_owned());
        match bytes.get(pos) {
            Some(b',') => pos += 1,
            Some(b')') => return Ok((fields, pos + 1)),
            _ => bail!("unterminated SQL row"),
        }
    }
}

fn country_row(fields: Vec<String>) -> Result<Country> {
    let [code, name] = <[String; 2]>::try_from(fields)
        .map_err(|f| anyhow::anyhow!("country row has {} fields, expected 2", f.len()))?;
    Ok(Country { country_code: code, country_name: name })
}

fn period_row(fields: Vec<String>) -> Result<ZonePeriod> {
    let [zone_name, country_code, abbreviation, start, offset, dst] =
        <[String; 6]>::try_from(fields)
            .map_err(|f| anyhow::anyhow!("time_zone row has {} fields, expected 6", f.len()))?;
    Ok(ZonePeriod {
        time_start: start.parse().with_context(|| format!("bad time_start {start:?} for {zone_name}"))?,
        gmt_offset: offset.parse().with_context(|| format!("bad gmt_offset {offset:?} for {zone_name}"))?,
        dst: dst == "1",
        zone_name,
        country_code,
        abbreviation,
    })
}

// ------------------------------------------------------------------ fetching

fn sql_path(config: &Config) -> PathBuf {
    config.tz_dir.join(SQL_FILE)
}

fn manifest_path(config: &Config) -> PathBuf {
    config.tz_dir.join(MANIFEST_FILE)
}

/// The release last fetched. The SQL file itself may have been discarded
/// after import; only `--offline` needs it.
fn local_release(config: &Config) -> Option<Release> {
    let text = fs::read_to_string(manifest_path(config)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Downloads the zip and returns the SQL text with its release identity.
async fn download(config: &Config) -> Result<(String, Release)> {
    let url = zip_url(config);
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(300))
        .build()?;
    let zip_bytes = client
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("cannot download {url}"))?
        .bytes()
        .await?;
    let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes))
        .context("TimeZoneDB download is not a zip archive")?;
    let mut entry = archive
        .by_name(SQL_FILE)
        .with_context(|| format!("TimeZoneDB archive has no {SQL_FILE}"))?;
    let date = entry
        .last_modified()
        .map(|d| format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day()));
    let release = Release { crc32: entry.crc32(), date };
    let mut sql = String::new();
    // Reading to the end also verifies the CRC.
    entry.read_to_string(&mut sql).context("cannot unpack time_zone.sql")?;
    Ok((sql, release))
}

/// Fetches the current release if it differs from the local copy.
/// Returns the SQL text, its release and whether it was newly downloaded.
async fn ensure(config: &Config) -> Result<(String, Release, bool)> {
    let local = local_release(config);
    if config.offline {
        let release = local.with_context(|| {
            format!("{} is missing and --offline was given", sql_path(config).display())
        })?;
        let sql = fs::read_to_string(sql_path(config)).with_context(|| {
            format!("{} is missing and --offline was given", sql_path(config).display())
        })?;
        println!("  kept  {SQL_FILE} (offline)");
        return Ok((sql, release, false));
    }
    let (sql, release) = match download(config).await {
        Ok(found) => found,
        Err(e) => {
            let Some(release) = local else { return Err(e) };
            let sql = fs::read_to_string(sql_path(config))
                .with_context(|| format!("cannot download the TimeZoneDB file and have no local copy: {e:#}"))?;
            println!("  kept  {SQL_FILE} (cannot check for updates: {e:#})");
            return Ok((sql, release, false));
        }
    };
    if local.as_ref() == Some(&release) && !config.force {
        println!("  kept  {SQL_FILE} (unchanged)");
        return Ok((sql, release, false));
    }
    fs::create_dir_all(&config.tz_dir)?;
    let staging = config.tz_dir.join(format!("{SQL_FILE}.part"));
    fs::write(&staging, &sql)?;
    fs::rename(&staging, sql_path(config))?;
    fs::write(manifest_path(config), serde_json::to_string_pretty(&release)?)?;
    let dated = release.date.clone().unwrap_or_else(|| "undated".into());
    println!("  fetch {SQL_FILE} (release {dated})");
    Ok((sql, release, true))
}

/// Deletes the unpacked SQL file to free disk space. Returns bytes freed.
pub fn discard_source(config: &Config) -> Result<u64> {
    let path = sql_path(config);
    let Ok(info) = fs::symlink_metadata(&path) else { return Ok(0) };
    fs::remove_file(&path).with_context(|| format!("cannot delete {}", path.display()))?;
    Ok(info.len())
}

/// Compares the server's release with the local copy. Returns `true` when an
/// update is available. Downloads the zip (under 1 MB) but writes nothing.
pub async fn check(config: &Config) -> Result<bool> {
    let (_, remote) = download(config).await?;
    let dated = remote.date.clone().unwrap_or_else(|| "undated".into());
    match local_release(config) {
        Some(local) if local == remote => {
            println!("  current  {SQL_FILE} (release {dated})");
            Ok(false)
        }
        Some(_) => {
            println!("  changed  {SQL_FILE} (server release: {dated})");
            Ok(true)
        }
        None => {
            println!("  missing  {SQL_FILE}");
            Ok(true)
        }
    }
}

// ------------------------------------------------------------------- loading

async fn meta(conn: &mut sqlx::SqliteConnection, key: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT value FROM import_meta WHERE key = ?")
        .bind(key)
        .fetch_optional(conn)
        .await?)
}

/// Fetches the current release and reloads both tables if it is new.
pub async fn import(config: &Config, rebuild: bool) -> Result<Outcome> {
    let (sql, release, _) = ensure(config).await?;

    let mut conn = db::open(&config.db_path).await?;
    let loaded_crc = meta(&mut conn, "timezonedb_crc32").await?;
    let previous_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM time_zones").fetch_one(&mut conn).await?;
    if !rebuild && previous_rows > 0 && loaded_crc == Some(release.crc32.to_string()) {
        conn.close().await?;
        return Ok(Outcome::UpToDate);
    }

    let dump = parse_dump(&sql)?;
    if dump.periods.is_empty() || dump.countries.is_empty() {
        bail!("import aborted, database unchanged: {SQL_FILE} has no time_zone or country rows");
    }
    let new_rows = dump.periods.len() as u64;
    if db::shrinks_too_much(previous_rows as u64, new_rows, config.max_drop_percent) {
        bail!(
            "import aborted, database unchanged: {new_rows} time zone periods would replace \
             {previous_rows} (limit {}% drop). Check the source file, or pass --max-drop 100 to accept",
            config.max_drop_percent
        );
    }

    let started_at = db::unix_now();
    let mut stats = Stats { release_date: release.date.clone(), ..Stats::default() };
    let mut tx = conn.begin().await?;
    sqlx::query("DELETE FROM time_zones").execute(&mut *tx).await?;
    sqlx::query("DELETE FROM countries").execute(&mut *tx).await?;

    stats.countries = dump.countries.len() as u64;
    let mut query: QueryBuilder<Sqlite> =
        QueryBuilder::new("INSERT OR REPLACE INTO countries (country_code, country_name) ");
    query.push_values(dump.countries, |mut row, c| {
        row.push_bind(c.country_code).push_bind(c.country_name);
    });
    query.build().execute(&mut *tx).await?;

    stats.periods = new_rows;
    let mut periods = dump.periods.into_iter().peekable();
    while periods.peek().is_some() {
        let batch: Vec<ZonePeriod> = periods.by_ref().take(BATCH_ROWS).collect();
        let mut query: QueryBuilder<Sqlite> = QueryBuilder::new(
            "INSERT OR REPLACE INTO time_zones \
             (zone_name, country_code, abbreviation, time_start, gmt_offset, dst) ",
        );
        query.push_values(batch, |mut row, p| {
            row.push_bind(p.zone_name)
                .push_bind(p.country_code)
                .push_bind(p.abbreviation)
                .push_bind(p.time_start)
                .push_bind(p.gmt_offset)
                .push_bind(p.dst);
        });
        query.build().execute(&mut *tx).await?;
    }

    let zones: i64 = sqlx::query_scalar("SELECT count(DISTINCT zone_name) FROM time_zones")
        .fetch_one(&mut *tx)
        .await?;
    stats.zones = zones as u64;

    let entries = [
        ("timezonedb_imported_at", started_at.to_string()),
        ("timezonedb_crc32", release.crc32.to_string()),
        ("timezonedb_release", release.date.unwrap_or_default()),
        ("time_zones_rows", stats.periods.to_string()),
        ("time_zones_zones", stats.zones.to_string()),
        ("countries_rows", stats.countries.to_string()),
        ("timezonedb_checked_at", started_at.to_string()),
        ("timezonedb_update_available", "false".to_string()),
    ];
    for (key, value) in entries {
        sqlx::query("INSERT OR REPLACE INTO import_meta (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    sqlx::raw_sql("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);").execute(&mut conn).await?;
    conn.close().await?;
    Ok(Outcome::Imported(stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"/*M!999999\- enable the sandbox mode */
DROP TABLE IF EXISTS `country`;
CREATE TABLE `country` (
  `country_code` char(2) DEFAULT NULL,
  `country_name` varchar(45) DEFAULT NULL
) ENGINE=MyISAM;
INSERT INTO `country` VALUES
('CI','Côte d\'Ivoire'),
('KP','Korea, Democratic People\'s Republic of'),
('GB','United Kingdom');
COMMIT;
INSERT INTO `other` VALUES (1,'ignored');
INSERT INTO `time_zone` VALUES
('Europe/London','GB','BST',1648342800,3600,'1'),
('Europe/London','GB','GMT',1667091600,0,'0');
INSERT INTO `time_zone` VALUES ('Etc/UTC','','GMT',-2208988800,0,'0'),('Asia/Kolkata','IN','IST',-764145000,19800,'0');
"#;

    #[test]
    fn dump_rows_parse_across_statements_and_layouts() {
        let dump = parse_dump(SAMPLE).unwrap();
        assert_eq!(dump.countries.len(), 3);
        assert_eq!(dump.countries[0].country_name, "Côte d'Ivoire");
        assert_eq!(dump.countries[1].country_name, "Korea, Democratic People's Republic of");
        assert_eq!(dump.periods.len(), 4);
        assert_eq!(
            dump.periods[0],
            ZonePeriod {
                zone_name: "Europe/London".into(),
                country_code: "GB".into(),
                abbreviation: "BST".into(),
                time_start: 1648342800,
                gmt_offset: 3600,
                dst: true,
            }
        );
        assert!(!dump.periods[1].dst);
        assert_eq!(dump.periods[2].country_code, "");
        assert_eq!(dump.periods[2].time_start, -2208988800);
        assert_eq!(dump.periods[3].gmt_offset, 19800);
    }

    #[test]
    fn doubled_quotes_and_column_lists_are_accepted() {
        let sql = "INSERT INTO `country` (`country_code`, `country_name`) VALUES ('CI','Cote d''Ivoire');";
        let dump = parse_dump(sql).unwrap();
        assert_eq!(dump.countries[0].country_name, "Cote d'Ivoire");
    }

    #[test]
    fn malformed_rows_are_errors_not_silent_skips() {
        assert!(parse_dump("INSERT INTO `country` VALUES ('GB');").is_err());
        assert!(parse_dump("INSERT INTO `time_zone` VALUES ('A/B','GB','X','soon',0,'0');").is_err());
        assert!(parse_dump("INSERT INTO `country` VALUES ('GB','United").is_err());
    }

    #[test]
    fn a_dump_without_inserts_is_empty() {
        assert_eq!(parse_dump("CREATE TABLE `country` (x int);").unwrap(), Dump::default());
    }
}
