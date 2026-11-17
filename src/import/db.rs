//! Loads the unpacked GeoNames files into SQLite.
//!
//! The whole load runs in one transaction on the live database: readers keep
//! seeing the previous data until it commits (WAL mode), and a failed import
//! changes nothing. Rows are keyed by GeoNames id, so a re-import updates
//! existing places, adds new ones and drops those GeoNames has deleted.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{ConnectOptions, Connection, QueryBuilder, Sqlite, SqliteConnection};
use tokio::io::{AsyncBufReadExt, BufReader};

use super::geonames::{
    AdminNames, Place, PlaceLine, parse_admin1, parse_admin2, parse_feature_code,
};
use super::names::{AltName, WEIGHT_ASCII, WEIGHT_MAIN, base_language};
use super::{
    ADMIN1_FILE, ADMIN2_FILE, COUNTRY_INFO_FILE, Config, FEATURE_CODES_FILE, LANGUAGE_CODES_FILE,
};

const SCHEMA: &str = include_str!("schema.sql");

/// Rows per INSERT statement. 16 columns each, well under SQLite's
/// 32,766 bound-parameter limit.
const BATCH_ROWS: usize = 1000;

/// Tables owned by this import, emptied and refilled on each run.
const CLEAR_TABLES: [&str; 6] = [
    "DELETE FROM place_names",
    "DELETE FROM toponyms_rtree",
    "DELETE FROM toponyms",
    "DELETE FROM admin1_codes",
    "DELETE FROM admin2_codes",
    "DELETE FROM feature_codes",
];

#[derive(Debug, Default)]
pub struct Stats {
    pub lines: u64,
    pub imported: u64,
    pub other_fcode: u64,
    pub unpopulated: u64,
    pub malformed: u64,
    pub no_region: u64,
    pub no_admin_name: u64,
    pub no_zone: u64,
    /// Rows whose region or admin name was replaced by a feature name.
    pub renamed: u64,
    /// Alternate-name rows read, and how many were kept as search names.
    pub alt_names_read: u64,
    pub alt_names_kept: u64,
    /// Total rows in place_names, including each place's own name.
    pub search_names: u64,
    /// Language codes in the configuration that GeoNames does not list.
    pub unknown_languages: Vec<String>,
    pub admin1: u64,
    pub admin2: u64,
}

fn non_empty(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}

pub async fn open(db_path: &Path) -> Result<SqliteConnection> {
    if let Some(dir) = db_path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let mut conn = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .connect()
        .await
        .with_context(|| format!("cannot open {}", db_path.display()))?;
    migrate(&mut conn).await.context("cannot create schema")?;
    Ok(conn)
}

/// Bumped whenever a table in `schema.sql` changes shape or name.
const SCHEMA_VERSION: i64 = 4;

/// Creates missing tables. A database written by an earlier schema version
/// first loses the tables that changed; the next import refills them.
async fn migrate(conn: &mut SqliteConnection) -> Result<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&mut *conn).await?;
    if version < 2 {
        // v1 had singular TimeZoneDB tables and GeoNames-based `countries`
        // and `time_zones` tables with different columns.
        sqlx::raw_sql(
            "DROP TABLE IF EXISTS time_zone;
             DROP TABLE IF EXISTS country;
             DROP TABLE IF EXISTS time_zones;
             DROP TABLE IF EXISTS countries;
             DELETE FROM import_meta WHERE key LIKE 'timezonedb_%' OR key = 'time_zone_rows';",
        )
        .execute(&mut *conn)
        .await
        .or_else(|e| {
            // A brand-new file has no import_meta table yet.
            if e.to_string().contains("no such table") { Ok(Default::default()) } else { Err(e) }
        })?;
    }
    if version < 3 {
        // v2 kept every GeoNames alternate name in toponyms.alt_names;
        // search names now live in place_names.
        let has_column: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pragma_table_info('toponyms') WHERE name = 'alt_names'",
        )
        .fetch_one(&mut *conn)
        .await?;
        if has_column > 0 {
            sqlx::raw_sql("ALTER TABLE toponyms DROP COLUMN alt_names").execute(&mut *conn).await?;
        }
    }
    if version < 4 {
        // v3 recorded one language per search name; v4 records all of them.
        sqlx::raw_sql(
            "DROP TABLE IF EXISTS place_names_fts;
             DROP TABLE IF EXISTS place_names;
             DELETE FROM import_meta WHERE key = 'geonames_names';",
        )
        .execute(&mut *conn)
        .await
        .or_else(|e| {
            if e.to_string().contains("no such table") { Ok(Default::default()) } else { Err(e) }
        })?;
    }
    sqlx::raw_sql(SCHEMA).execute(&mut *conn).await?;
    if version != SCHEMA_VERSION {
        sqlx::raw_sql("PRAGMA user_version = 4").execute(&mut *conn).await?;
    }
    Ok(())
}

pub async fn build(config: &Config) -> Result<Stats> {
    let started = Instant::now();
    // Recorded as the import time: any source file modified after this
    // moment may not have been read.
    let started_at = unix_now();
    let mut conn = open(&config.db_path).await?;
    let mut tx = conn.begin().await?;
    let mut stats = Stats::default();

    // Only comparable when the previous import used the same settings.
    let previous_rows = if settings_match(config, &mut tx).await? {
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM toponyms").fetch_one(&mut *tx).await?;
        rows as u64
    } else {
        0
    };

    for statement in CLEAR_TABLES {
        sqlx::query(statement).execute(&mut *tx).await?;
    }

    let names = load_support_tables(config, &mut tx, &mut stats).await?;

    let mut place_countries: PlaceCountries = HashMap::new();
    for source in config.place_sources() {
        let path = config.local_path(&source.local);
        load_places(config, &path, &names, &mut tx, &mut stats, &mut place_countries).await?;
    }

    if shrinks_too_much(previous_rows, stats.imported, config.max_drop_percent) {
        // Dropping the transaction rolls everything back.
        bail!(
            "import aborted, database unchanged: {} places would replace {} (a drop of {:.1}%, limit {}%). \
             Check the source files, or pass --max-drop 100 to accept",
            stats.imported,
            previous_rows,
            drop_percent(previous_rows, stats.imported),
            config.max_drop_percent
        );
    }

    stats.renamed = apply_feature_names(&mut tx).await?;

    load_search_names(config, &place_countries, &mut tx, &mut stats).await?;
    drop(place_countries);

    sqlx::query(
        "INSERT INTO toponyms_rtree (id, min_lat, max_lat, min_lng, max_lng)
         SELECT pl_id, lat, lat, lng, lng FROM toponyms",
    )
    .execute(&mut *tx)
    .await
    .context("cannot build spatial index")?;

    let mut meta = settings_meta(config).to_vec();
    meta.push(("geonames_imported_at", started_at.to_string()));
    meta.push(("toponyms_rows", stats.imported.to_string()));
    meta.push(("place_names_rows", stats.search_names.to_string()));
    let source_date = config
        .place_sources()
        .iter()
        .filter_map(|source| super::fetch::source_date(config, source))
        .max()
        .unwrap_or_default();
    meta.push(("geonames_release", source_date));
    // A fresh import is by definition current.
    meta.push(("geonames_checked_at", started_at.to_string()));
    meta.push(("geonames_update_available", "false".to_string()));
    for (key, value) in meta {
        sqlx::query("INSERT OR REPLACE INTO import_meta (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    sqlx::raw_sql("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);").execute(&mut conn).await?;
    conn.close().await?;
    println!("  database written in {:.1}s", started.elapsed().as_secs_f64());
    Ok(stats)
}

/// Prefers the name of the division's own feature row over the name in the
/// admin code files, e.g. "Khuzestan Province" rather than "Khuzestan". The
/// code-file name stays where the feature row was not imported.
async fn apply_feature_names(tx: &mut sqlx::Transaction<'_, Sqlite>) -> Result<u64> {
    let regions = sqlx::query(
        "UPDATE toponyms SET region = f.name
         FROM admin1_codes a, toponyms f
         WHERE a.cc = toponyms.cc AND a.adm1 = toponyms.adm1
           AND f.pl_id = a.geoname_id AND f.name <> toponyms.region",
    )
    .execute(&mut **tx)
    .await?;
    let districts = sqlx::query(
        "UPDATE toponyms SET admin_name = f.name
         FROM admin2_codes a, toponyms f
         WHERE a.cc = toponyms.cc AND a.adm1 = toponyms.adm1 AND a.adm2 = toponyms.adm2
           AND f.pl_id = a.geoname_id AND f.name <> toponyms.admin_name",
    )
    .execute(&mut **tx)
    .await?;
    Ok(regions.rows_affected() + districts.rows_affected())
}

/// The import settings that decide which rows end up in the database.
fn settings_meta(config: &Config) -> [(&'static str, String); 4] {
    let scope = if config.countries.is_empty() {
        "allCountries".to_string()
    } else {
        config.countries.join(",").to_uppercase()
    };
    let mut fcodes: Vec<&str> = config.fcodes.iter().map(String::as_str).collect();
    fcodes.sort_unstable();
    [
        ("geonames_scope", scope),
        ("geonames_fcodes", fcodes.join(" ")),
        ("geonames_keep_unpopulated", config.keep_unpopulated.to_string()),
        ("geonames_names", config.languages.signature()),
    ]
}

async fn settings_match(config: &Config, conn: &mut SqliteConnection) -> Result<bool> {
    let mut same = true;
    for (key, expected) in settings_meta(config) {
        let stored: Option<String> =
            sqlx::query_scalar("SELECT value FROM import_meta WHERE key = ?")
                .bind(key)
                .fetch_optional(&mut *conn)
                .await?;
        same &= stored.as_deref() == Some(expected.as_str());
    }
    Ok(same)
}

fn drop_percent(previous: u64, new: u64) -> f64 {
    if previous == 0 || new >= previous {
        0.0
    } else {
        (previous - new) as f64 * 100.0 / previous as f64
    }
}

/// Guards against a truncated or broken source file wiping good data.
pub fn shrinks_too_much(previous: u64, new: u64, max_drop_percent: f64) -> bool {
    drop_percent(previous, new) > max_drop_percent
}

/// True when the database already holds an import made with these settings
/// from the source files as they are now, so a rebuild would change nothing.
pub async fn is_current(config: &Config) -> Result<bool> {
    if !config.db_path.is_file() {
        return Ok(false);
    }
    let mut conn = open(&config.db_path).await?;
    let same = settings_match(config, &mut conn).await?;
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM toponyms").fetch_one(&mut conn).await?;
    let imported_at: Option<String> =
        sqlx::query_scalar("SELECT value FROM import_meta WHERE key = 'geonames_imported_at'")
            .fetch_optional(&mut conn)
            .await?;
    conn.close().await?;
    let imported_at = imported_at.and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);

    // A source file written after the last import (e.g. by `fetch`) has not
    // been loaded yet.
    let mut newest_source = 0;
    for source in &config.all_sources() {
        let modified = fs::metadata(config.local_path(&source.local))?.modified()?;
        let secs = modified.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        newest_source = newest_source.max(secs);
    }
    Ok(same && rows > 0 && newest_source <= imported_at)
}

/// Appends a row to `import_log`. Called after the import itself has
/// committed or rolled back, so failures are recorded too.
pub async fn log_import(
    db_path: &Path,
    data_set: &str,
    started_at: u64,
    status: &str,
    records: Option<u64>,
    detail: &str,
) -> Result<()> {
    let mut conn = open(db_path).await?;
    sqlx::query(
        "INSERT INTO import_log (data_set, started_at, finished_at, status, records, detail)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(data_set)
    .bind(started_at as i64)
    .bind(unix_now() as i64)
    .bind(status)
    .bind(records.map(|r| r as i64))
    .bind(non_empty(detail))
    .execute(&mut conn)
    .await?;
    conn.close().await?;
    Ok(())
}

/// Stores the outcome of a check for newer source files, for the status page.
pub async fn record_check(db_path: &Path, data_set: &str, update_available: bool) -> Result<()> {
    // Nothing to annotate before the first import.
    if !db_path.is_file() {
        return Ok(());
    }
    let mut conn = open(db_path).await?;
    let entries = [
        (format!("{data_set}_checked_at"), unix_now().to_string()),
        (format!("{data_set}_update_available"), update_available.to_string()),
    ];
    for (key, value) in entries {
        sqlx::query("INSERT OR REPLACE INTO import_meta (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&mut conn)
            .await?;
    }
    conn.close().await?;
    Ok(())
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_support(config: &Config, file: &str) -> Result<String> {
    let path = config.local_path(file);
    fs::read_to_string(&path)
        .with_context(|| format!("cannot read {} (run the fetch step first)", path.display()))
}

async fn load_support_tables(
    config: &Config,
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    stats: &mut Stats,
) -> Result<AdminNames> {
    let mut names = AdminNames::default();

    let admin1 = read_support(config, ADMIN1_FILE)?;
    for code in admin1.lines().filter_map(parse_admin1) {
        sqlx::query(
            "INSERT OR REPLACE INTO admin1_codes (cc, adm1, name, ascii_name, geoname_id)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&code.cc)
        .bind(&code.adm1)
        .bind(&code.name)
        .bind(non_empty(&code.ascii_name))
        .bind(code.geoname_id)
        .execute(&mut **tx)
        .await?;
        names.add(&code);
        stats.admin1 += 1;
    }

    let admin2 = read_support(config, ADMIN2_FILE)?;
    for code in admin2.lines().filter_map(parse_admin2) {
        sqlx::query(
            "INSERT OR REPLACE INTO admin2_codes (cc, adm1, adm2, name, ascii_name, geoname_id)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&code.cc)
        .bind(&code.adm1)
        .bind(&code.adm2)
        .bind(&code.name)
        .bind(non_empty(&code.ascii_name))
        .bind(code.geoname_id)
        .execute(&mut **tx)
        .await?;
        names.add(&code);
        stats.admin2 += 1;
    }

    let features = read_support(config, FEATURE_CODES_FILE)?;
    for feature in features.lines().filter_map(parse_feature_code) {
        sqlx::query(
            "INSERT OR IGNORE INTO feature_codes (fcode, fclass, name, description)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&feature.fcode)
        .bind(&feature.fclass)
        .bind(non_empty(&feature.name))
        .bind(non_empty(&feature.description))
        .execute(&mut **tx)
        .await?;
    }

    Ok(names)
}

/// Country code of each imported place, by GeoNames id.
type PlaceCountries = HashMap<i64, [u8; 2]>;

/// Fills `place_names` with each place's own name and the alternate names
/// its country's language rules accept, then rebuilds the search index.
async fn load_search_names(
    config: &Config,
    place_countries: &PlaceCountries,
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    stats: &mut Stats,
) -> Result<()> {
    let mut languages = config.languages.clone();
    languages.add_country_info(&read_support(config, COUNTRY_INFO_FILE)?);
    stats.unknown_languages = languages.check_codes(&read_support(config, LANGUAGE_CODES_FILE)?);

    sqlx::query("INSERT INTO place_names (pl_id, name, langs, weight) SELECT pl_id, name, NULL, ? FROM toponyms")
        .bind(WEIGHT_MAIN)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT OR IGNORE INTO place_names (pl_id, name, langs, weight)
         SELECT pl_id, ascii_name, NULL, ? FROM toponyms
         WHERE ascii_name IS NOT NULL AND ascii_name <> name",
    )
    .bind(WEIGHT_ASCII)
    .execute(&mut **tx)
    .await?;

    let mut batch: Vec<(i64, String, String, i64)> = Vec::with_capacity(BATCH_ROWS);
    for source in config.name_sources() {
        let path = config.local_path(&source.local);
        let file = tokio::fs::File::open(&path)
            .await
            .with_context(|| format!("cannot read {} (run the fetch step first)", path.display()))?;
        let mut lines = BufReader::with_capacity(1 << 20, file).lines();
        while let Some(text) = lines.next_line().await? {
            stats.alt_names_read += 1;
            let Some(alt) = AltName::parse(&text) else { continue };
            let Some(cc) = place_countries.get(&alt.geoname_id) else { continue };
            let cc = std::str::from_utf8(cc).unwrap_or("");
            let Some(weight) = languages.weigh(cc, &alt) else { continue };
            batch.push((alt.geoname_id, alt.name.to_owned(), base_language(alt.lang), weight));
            if batch.len() == BATCH_ROWS {
                insert_names(std::mem::take(&mut batch), tx, stats).await?;
            }
        }
    }
    if !batch.is_empty() {
        insert_names(batch, tx, stats).await?;
    }

    // Renumber the names so that ids run from the largest populated place
    // down. The search index returns matches in id order, so a search can
    // take the first few thousand and stop instead of reading and sorting
    // every match of a short prefix.
    sqlx::raw_sql(
        "CREATE TEMP TABLE names_sorted AS
           SELECT n.pl_id, n.name, n.langs, n.weight
           FROM place_names n JOIN toponyms t ON t.pl_id = n.pl_id
           ORDER BY (t.fclass = 'P') DESC, t.population DESC, n.pl_id, n.weight DESC;
         DELETE FROM place_names;
         INSERT INTO place_names (pl_id, name, langs, weight)
           SELECT pl_id, name, langs, weight FROM names_sorted ORDER BY rowid;
         DROP TABLE names_sorted;",
    )
    .execute(&mut **tx)
    .await
    .context("cannot order search names")?;

    sqlx::query("INSERT INTO place_names_fts (place_names_fts) VALUES ('rebuild')")
        .execute(&mut **tx)
        .await
        .context("cannot build search index")?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM place_names").fetch_one(&mut **tx).await?;
    stats.search_names = total as u64;
    Ok(())
}

async fn insert_names(
    batch: Vec<(i64, String, String, i64)>,
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    stats: &mut Stats,
) -> Result<()> {
    stats.alt_names_kept += batch.len() as u64;
    let mut query: QueryBuilder<Sqlite> =
        QueryBuilder::new("INSERT INTO place_names (pl_id, name, langs, weight) ");
    query.push_values(batch, |mut row, (id, name, lang, weight)| {
        row.push_bind(id).push_bind(name).push_bind(lang).push_bind(weight);
    });
    // The same spelling in several languages is one row: it keeps its
    // strongest weight and collects every language, so a search can favour
    // the names used in the searcher's own language.
    query.push(
        " ON CONFLICT (pl_id, name) DO UPDATE SET \
         langs = CASE \
           WHEN langs IS NULL THEN excluded.langs \
           WHEN instr(' ' || langs || ' ', ' ' || excluded.langs || ' ') > 0 THEN langs \
           ELSE langs || ' ' || excluded.langs END, \
         weight = max(weight, excluded.weight)",
    );
    query.build().execute(&mut **tx).await?;
    Ok(())
}

/// The selection rule of the original table: a listed feature code, and for
/// codes starting with `P` a known population.
fn is_wanted(config: &Config, line: &PlaceLine<'_>, stats: &mut Stats) -> bool {
    let fcode = line.fcode();
    if !config.fcodes.contains(fcode) {
        stats.other_fcode += 1;
        return false;
    }
    if !config.keep_unpopulated && fcode.starts_with('P') && line.population() <= 0 {
        stats.unpopulated += 1;
        return false;
    }
    true
}

async fn load_places(
    config: &Config,
    path: &Path,
    names: &AdminNames,
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    stats: &mut Stats,
    place_countries: &mut PlaceCountries,
) -> Result<()> {
    let file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("cannot read {} (run the fetch step first)", path.display()))?;
    let mut lines = BufReader::with_capacity(1 << 20, file).lines();
    let mut batch: Vec<Place> = Vec::with_capacity(BATCH_ROWS);

    while let Some(text) = lines.next_line().await? {
        stats.lines += 1;
        if stats.lines.is_multiple_of(2_000_000) {
            println!("  read {}M lines, kept {}", stats.lines / 1_000_000, stats.imported);
        }
        let Some(line) = PlaceLine::parse(&text) else {
            stats.malformed += 1;
            continue;
        };
        if !is_wanted(config, &line, stats) {
            continue;
        }
        let Some(place) = line.to_place() else {
            stats.malformed += 1;
            continue;
        };
        if let [a, b] = place.cc.as_bytes() {
            place_countries.insert(place.id, [*a, *b]);
        }
        batch.push(place);
        if batch.len() == BATCH_ROWS {
            insert_places(std::mem::take(&mut batch), names, tx, stats).await?;
        }
    }
    if !batch.is_empty() {
        insert_places(batch, names, tx, stats).await?;
    }
    Ok(())
}

async fn insert_places(
    batch: Vec<Place>,
    names: &AdminNames,
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    stats: &mut Stats,
) -> Result<()> {
    let mut query: QueryBuilder<Sqlite> = QueryBuilder::new(
        "INSERT OR REPLACE INTO toponyms (pl_id, name, ascii_name, cc, fclass, fcode, \
         lat, lng, population, zone_name, date, adm1, adm2, admin_name, region) ",
    );
    stats.imported += batch.len() as u64;
    query.push_values(batch, |mut row, p| {
        let region = names.region(&p.cc, &p.adm1).map(str::to_owned);
        let admin_name = names.admin_name(&p.cc, &p.adm1, &p.adm2).map(str::to_owned);
        stats.no_region += region.is_none() as u64;
        stats.no_admin_name += admin_name.is_none() as u64;
        stats.no_zone += p.zone_name.is_empty() as u64;
        let opt = |s: String| if s.is_empty() { None } else { Some(s) };
        row.push_bind(p.id)
            .push_bind(p.name)
            .push_bind(opt(p.ascii_name))
            .push_bind(opt(p.cc))
            .push_bind(opt(p.fclass))
            .push_bind(opt(p.fcode))
            .push_bind(p.lat)
            .push_bind(p.lng)
            .push_bind(p.population)
            .push_bind(opt(p.zone_name))
            .push_bind(opt(p.date))
            .push_bind(opt(p.adm1))
            .push_bind(opt(p.adm2))
            .push_bind(admin_name)
            .push_bind(region);
    });
    query.build().execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_trips_only_on_large_drops() {
        assert!(!shrinks_too_much(0, 0, 10.0), "first import has nothing to compare");
        assert!(!shrinks_too_much(1_000_000, 1_050_000, 10.0), "growth is fine");
        assert!(!shrinks_too_much(1_000_000, 950_000, 10.0));
        assert!(!shrinks_too_much(1_000_000, 900_000, 10.0), "exactly at the limit passes");
        assert!(shrinks_too_much(1_000_000, 899_999, 10.0));
        assert!(shrinks_too_much(1_000_000, 0, 10.0));
        assert!(!shrinks_too_much(1_000_000, 0, 100.0), "100 disables the guard");
    }
}
