//! The status report served at `/`: which data sets are loaded, when they
//! were imported, how many records they hold and whether newer source files
//! are known to exist.

use std::collections::HashMap;

use serde::Serialize;
use sqlx::{Row, SqlitePool};

use super::ApiError;
use super::time::format_datetime;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    message: &'static str,
    version: &'static str,
    /// True when both data sets are loaded and every endpoint can answer.
    ready: bool,
    sources: Sources,
    /// The latest import attempts, newest first.
    recent_imports: Vec<LogEntry>,
}

#[derive(Serialize)]
struct Sources {
    geonames: Source,
    timezonedb: Source,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    /// What the data set provides.
    provides: &'static str,
    imported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    imported_at: Option<String>,
    /// Row counts by table, as of the last import.
    #[serde(skip_serializing_if = "Option::is_none")]
    records: Option<HashMap<&'static str, i64>>,
    /// Date the publisher last modified the files that were imported.
    #[serde(skip_serializing_if = "Option::is_none")]
    release: Option<String>,
    /// For places: `allCountries`, or the country codes imported.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    /// When the source was last compared with the publisher's files.
    #[serde(skip_serializing_if = "Option::is_none")]
    checked_at: Option<String>,
    /// Whether that comparison found newer files than those imported.
    #[serde(skip_serializing_if = "Option::is_none")]
    update_available: Option<bool>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LogEntry {
    data_set: String,
    started_at: String,
    finished_at: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    records: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

/// UTC timestamp for display, e.g. `2026-11-17T09:30:00Z`.
fn timestamp(unix: i64) -> String {
    format!("{}Z", format_datetime(unix))
}

/// A database written before a table existed simply has nothing to report.
fn or_empty<T: Default>(result: Result<T, sqlx::Error>) -> Result<T, sqlx::Error> {
    match result {
        Err(error) if error.to_string().contains("no such table") => Ok(T::default()),
        other => other,
    }
}

struct Meta(HashMap<String, String>);

impl Meta {
    fn text(&self, key: &str) -> Option<String> {
        self.0.get(key).filter(|v| !v.is_empty()).cloned()
    }

    fn number(&self, key: &str) -> Option<i64> {
        self.0.get(key)?.parse().ok()
    }

    fn time(&self, key: &str) -> Option<String> {
        self.number(key).map(timestamp)
    }

    fn flag(&self, key: &str) -> Option<bool> {
        self.0.get(key)?.parse().ok()
    }

    fn records(&self, tables: &[(&'static str, &str)]) -> Option<HashMap<&'static str, i64>> {
        let counts: HashMap<&'static str, i64> =
            tables.iter().filter_map(|(label, key)| Some((*label, self.number(key)?))).collect();
        (!counts.is_empty()).then_some(counts)
    }
}

async fn has_rows(pool: &SqlitePool, table_query: &'static str) -> Result<bool, sqlx::Error> {
    or_empty(sqlx::query(table_query).fetch_optional(pool).await).map(|row| row.is_some())
}

pub async fn status(pool: &SqlitePool) -> Result<Status, ApiError> {
    let rows = or_empty(sqlx::query("SELECT key, value FROM import_meta").fetch_all(pool).await)?;
    let mut values = HashMap::new();
    for row in &rows {
        values.insert(row.try_get::<String, _>("key")?, row.try_get::<Option<String>, _>("value")?.unwrap_or_default());
    }
    let meta = Meta(values);

    let places_loaded = has_rows(pool, "SELECT 1 FROM toponyms LIMIT 1").await?;
    let zones_loaded = has_rows(pool, "SELECT 1 FROM time_zones LIMIT 1").await?;

    let geonames = Source {
        provides: "places, search names and their time-zone names",
        imported: places_loaded,
        imported_at: meta.time("geonames_imported_at").filter(|_| places_loaded),
        records: meta
            .records(&[("places", "toponyms_rows"), ("searchNames", "place_names_rows")])
            .filter(|_| places_loaded),
        release: meta.text("geonames_release").filter(|_| places_loaded),
        scope: meta.text("geonames_scope").filter(|_| places_loaded),
        checked_at: meta.time("geonames_checked_at"),
        update_available: meta.flag("geonames_update_available"),
    };
    let timezonedb = Source {
        provides: "time-zone offset history and country names",
        imported: zones_loaded,
        imported_at: meta.time("timezonedb_imported_at").filter(|_| zones_loaded),
        records: meta
            .records(&[
                ("offsetPeriods", "time_zones_rows"),
                ("zones", "time_zones_zones"),
                ("countries", "countries_rows"),
            ])
            .filter(|_| zones_loaded),
        release: meta.text("timezonedb_release").filter(|_| zones_loaded),
        scope: None,
        checked_at: meta.time("timezonedb_checked_at"),
        update_available: meta.flag("timezonedb_update_available"),
    };

    let log_rows = or_empty(
        sqlx::query(
            "SELECT data_set, started_at, finished_at, status, records, detail
             FROM import_log ORDER BY id DESC LIMIT 10",
        )
        .fetch_all(pool)
        .await,
    )?;
    let mut recent_imports = Vec::with_capacity(log_rows.len());
    for row in &log_rows {
        recent_imports.push(LogEntry {
            data_set: row.try_get("data_set")?,
            started_at: timestamp(row.try_get("started_at")?),
            finished_at: timestamp(row.try_get("finished_at")?),
            status: row.try_get("status")?,
            records: row.try_get("records")?,
            detail: row.try_get("detail")?,
        });
    }

    Ok(Status {
        message: "Welcome to GeoTimeZone",
        version: env!("CARGO_PKG_VERSION"),
        ready: places_loaded && zones_loaded,
        sources: Sources { geonames, timezonedb },
        recent_imports,
    })
}
