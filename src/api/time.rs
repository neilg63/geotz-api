//! Date-time handling and time-zone offset lookups.
//!
//! All arithmetic is on Unix seconds. A "local" value is a wall-clock time
//! held in the same form, i.e. the Unix timestamp that would show that clock
//! reading in UTC.

use chrono::{DateTime, Datelike, NaiveDate};
use julian_day_converter::{julian_day_to_unixtime, unixtime_to_julian_day};
use serde::Serialize;
use sqlx::{Row, SqlitePool};

use super::ApiError;

/// The moment a request refers to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum When {
    /// A UTC instant (`dt`, `jd`, `un`, or now).
    Utc(i64),
    /// A wall-clock time at the place in question (`dtl`).
    Local(i64),
}

/// Parses `YYYY-MM-DD`, optionally followed by `T` or a space and
/// `HH`, `HH:MM` or `HH:MM:SS`. Missing parts are zero. A trailing `Z` is
/// accepted. Returns the reading as Unix seconds.
pub fn parse_datetime(text: &str) -> Option<i64> {
    let text = text.trim().trim_end_matches(['Z', 'z']);
    let (date, time) = match text.split_once(['T', 't', ' ']) {
        Some((date, time)) => (date, time.trim()),
        None => (text, ""),
    };
    let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let mut parts = [0u32; 3];
    if !time.is_empty() {
        let mut fields = time.split(':');
        for part in parts.iter_mut() {
            match fields.next() {
                // Fractions of a second are dropped.
                Some(field) => *part = field.split('.').next()?.parse().ok()?,
                None => break,
            }
        }
        if fields.next().is_some() {
            return None;
        }
    }
    let datetime = date.and_hms_opt(parts[0], parts[1], parts[2])?;
    Some(datetime.and_utc().timestamp())
}

/// Formats Unix seconds as `YYYY-MM-DDTHH:MM:SS`.
pub fn format_datetime(ts: i64) -> String {
    match DateTime::from_timestamp(ts, 0) {
        Some(dt) => dt.format("%Y-%m-%dT%H:%M:%S").to_string(),
        None => String::new(),
    }
}

/// Julian day for a Unix timestamp, via the `julian_day_converter` crate.
pub fn unix_to_julian_day(ts: i64) -> f64 {
    unixtime_to_julian_day(ts)
}

/// Unix timestamp for a Julian day, via the `julian_day_converter` crate.
pub fn julian_day_to_unix(jd: f64) -> i64 {
    julian_day_to_unixtime(jd)
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WeekDay {
    /// Monday = 1 ... Sunday = 7.
    pub iso: u32,
    /// Sunday = 1 ... Saturday = 7.
    pub sun: u32,
    pub abbr: String,
}

fn week_day(local_ts: i64) -> Option<WeekDay> {
    let dt = DateTime::from_timestamp(local_ts, 0)?;
    let iso = dt.weekday().number_from_monday();
    Some(WeekDay {
        iso,
        sun: iso % 7 + 1,
        abbr: dt.format("%a").to_string(),
    })
}

/// An offset in force from `start` until the next period of the same zone.
#[derive(Debug, Clone, PartialEq)]
pub struct ZonePeriod {
    pub start: i64,
    pub offset: i64,
    pub abbreviation: String,
    pub dst: bool,
    pub country_code: String,
}

/// Converts a wall-clock time to UTC using a zone's periods, which must be
/// sorted by start and include the period before the first candidate.
///
/// Each period is tried in turn: `local - offset` is the answer if it falls
/// inside that period. Around a transition there may be two answers or none:
///
/// * Clocks go back, so a reading occurs twice: `first_occurrence` picks the
///   earlier instant (still on the old offset, usually summer time),
///   otherwise the later one.
/// * Clocks go forward, so a reading never occurs: it is read on the offset
///   in force before the jump, which lands just after it (01:30 in a
///   01:00 -> 02:00 gap becomes 02:30 on the new offset).
///
/// Returns `None` when `local` is earlier than the first period given.
pub fn local_to_utc(periods: &[ZonePeriod], local: i64, first_occurrence: bool) -> Option<i64> {
    let mut matches = Vec::with_capacity(2);
    for (i, period) in periods.iter().enumerate() {
        let utc = local - period.offset;
        let ends_before = periods.get(i + 1).is_none_or(|next| utc < next.start);
        if utc >= period.start && ends_before {
            matches.push(utc);
        }
    }
    match matches.as_slice() {
        [only] => Some(*only),
        [first, .., last] => Some(if first_occurrence { *first } else { *last }),
        [] => periods.windows(2).find_map(|pair| {
            let (before, after) = (&pair[0], &pair[1]);
            let on_old_offset = local - before.offset;
            let in_gap = on_old_offset >= after.start && local - after.offset < after.start;
            in_gap.then_some(on_old_offset)
        }),
    }
}

/// Offset from UTC by longitude alone, to the second (local mean solar time).
pub fn solar_offset(lng: f64) -> i64 {
    let wrapped = (lng + 540.0).rem_euclid(360.0) - 180.0;
    (wrapped * 240.0).round() as i64
}

/// Offset of the nautical time zone for a longitude: whole hours.
pub fn nautical_offset(lng: f64) -> i64 {
    let wrapped = (lng + 540.0).rem_euclid(360.0) - 180.0;
    (wrapped / 15.0).round() as i64 * 3600
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct Period {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_utc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_utc_offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_utc: Option<String>,
}

/// The time-zone answer returned by `/timezone`, `/geotz` and `/geotime`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TimeZone {
    pub zone_name: String,
    pub country_code: String,
    pub abbreviation: String,
    /// Seconds east of UTC at the reference time.
    pub gmt_offset: i64,
    pub dst: bool,
    pub local_dt: String,
    pub utc: String,
    pub period: Period,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub week_day: Option<WeekDay>,
    pub ref_unix: i64,
    pub ref_jd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solar_utc_offset: Option<i64>,
}

impl TimeZone {
    fn new(
        zone_name: &str,
        country_code: &str,
        abbreviation: &str,
        offset: i64,
        dst: bool,
        utc: i64,
    ) -> Self {
        TimeZone {
            zone_name: zone_name.to_owned(),
            country_code: country_code.to_owned(),
            abbreviation: abbreviation.to_owned(),
            gmt_offset: offset,
            dst,
            local_dt: format_datetime(utc + offset),
            utc: format_datetime(utc),
            period: Period::default(),
            week_day: week_day(utc + offset),
            ref_unix: utc,
            ref_jd: unix_to_julian_day(utc),
            solar_utc_offset: None,
        }
    }

    /// A zone derived from longitude alone, for open sea or for dates before
    /// a zone's records begin. Before 1900 the offset is local solar time
    /// (`SOL`); from 1900 it is the nautical whole-hour zone (`LOC`).
    pub fn natural(zone_name: &str, country_code: &str, lng: f64, when: When) -> Self {
        let reading = match when {
            When::Utc(ts) | When::Local(ts) => ts,
        };
        let before_1900 = reading < -2_208_988_800;
        let offset = if before_1900 {
            solar_offset(lng)
        } else {
            nautical_offset(lng)
        };
        let utc = match when {
            When::Utc(ts) => ts,
            When::Local(ts) => ts - offset,
        };
        let abbreviation = if before_1900 { "SOL" } else { "LOC" };
        let mut zone = TimeZone::new(zone_name, country_code, abbreviation, offset, false, utc);
        zone.solar_utc_offset = Some(solar_offset(lng));
        zone
    }

    /// The sea-area zone for a point with no place nearby, e.g.
    /// `North_Atlantic/03W`.
    pub fn at_sea(lat: f64, lng: f64, when: When) -> Self {
        let hours = nautical_offset(lng) / 3600;
        let side = if hours < 0 { 'W' } else { 'E' };
        let name = format!("{}/{:02}{side}", sea_area(lat, lng), hours.abs());
        TimeZone::natural(&name, "", lng, when)
    }
}

/// A coarse name for the body of water at a position.
fn sea_area(lat: f64, lng: f64) -> &'static str {
    if lat >= 66.5 {
        "Arctic"
    } else if lat <= -60.0 {
        "Southern"
    } else if (20.0..147.0).contains(&lng) && lat < 30.0 {
        "Indian"
    } else if (-70.0..20.0).contains(&lng) {
        if lat >= 0.0 {
            "North_Atlantic"
        } else {
            "South_Atlantic"
        }
    } else if lat >= 0.0 {
        "North_Pacific"
    } else {
        "South_Pacific"
    }
}

fn period_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ZonePeriod, sqlx::Error> {
    Ok(ZonePeriod {
        start: row.try_get("time_start")?,
        offset: row.try_get("gmt_offset")?,
        abbreviation: row.try_get("abbreviation")?,
        dst: row.try_get::<i64, _>("dst")? != 0,
        country_code: row.try_get("country_code")?,
    })
}

/// The periods of a zone that could contain `local`: everything starting
/// within two days either side, plus the one in force before that window.
async fn periods_near(
    pool: &SqlitePool,
    zone: &str,
    local: i64,
) -> Result<Vec<ZonePeriod>, ApiError> {
    const WINDOW: i64 = 2 * 86_400;
    let rows = sqlx::query(
        "SELECT time_start, gmt_offset, abbreviation, dst, country_code FROM (
           SELECT * FROM (SELECT * FROM time_zones WHERE zone_name = ?1 AND time_start < ?2
                          ORDER BY time_start DESC LIMIT 1)
           UNION ALL
           SELECT * FROM time_zones WHERE zone_name = ?1 AND time_start BETWEEN ?2 AND ?3
         ) ORDER BY time_start",
    )
    .bind(zone)
    .bind(local - WINDOW)
    .bind(local + WINDOW)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(period_from_row).collect::<Result<_, _>>()?)
}

/// The period in force at `utc` and the one after it.
async fn period_at(
    pool: &SqlitePool,
    zone: &str,
    utc: i64,
) -> Result<(Option<ZonePeriod>, Option<ZonePeriod>), ApiError> {
    const COLUMNS: &str =
        "SELECT time_start, gmt_offset, abbreviation, dst, country_code FROM time_zones";
    let current = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{COLUMNS} WHERE zone_name = ? AND time_start <= ? ORDER BY time_start DESC LIMIT 1"
    )))
    .bind(zone)
    .bind(utc)
    .fetch_optional(pool)
    .await?;
    let next = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{COLUMNS} WHERE zone_name = ? AND time_start > ? ORDER BY time_start LIMIT 1"
    )))
    .bind(zone)
    .bind(utc)
    .fetch_optional(pool)
    .await?;
    Ok((
        current.as_ref().map(period_from_row).transpose()?,
        next.as_ref().map(period_from_row).transpose()?,
    ))
}

/// Looks up the offset for `zone` at `when`.
///
/// `lng`, when known, adds the solar offset and allows a longitude-based
/// answer for dates before the zone's records begin. `first_occurrence`
/// settles a local time that occurs twice (see [`local_to_utc`]).
///
/// Returns `None` if the zone is not in the database.
pub async fn zone_time(
    pool: &SqlitePool,
    zone: &str,
    when: When,
    lng: Option<f64>,
    first_occurrence: bool,
) -> Result<Option<TimeZone>, ApiError> {
    let utc = match when {
        When::Utc(ts) => Some(ts),
        When::Local(local) => {
            let periods = periods_near(pool, zone, local).await?;
            local_to_utc(&periods, local, first_occurrence)
        }
    };
    let (current, next) = match utc {
        Some(utc) => period_at(pool, zone, utc).await?,
        None => (None, None),
    };

    let Some(current) = current else {
        // Before the zone's first record, or an unknown zone.
        let (_, first) = period_at(pool, zone, i64::MIN).await?;
        let Some(first) = first else { return Ok(None) };
        return Ok(Some(match lng {
            Some(lng) => TimeZone::natural(zone, &first.country_code, lng, when),
            None => {
                // The first record is local mean time, which applies to
                // every earlier date.
                let utc = match when {
                    When::Utc(ts) => ts,
                    When::Local(ts) => ts - first.offset,
                };
                TimeZone::new(
                    zone,
                    &first.country_code,
                    &first.abbreviation,
                    first.offset,
                    first.dst,
                    utc,
                )
            }
        }));
    };

    let utc = utc.unwrap_or_default();
    let mut zone_time = TimeZone::new(
        zone,
        &current.country_code,
        &current.abbreviation,
        current.offset,
        current.dst,
        utc,
    );
    zone_time.period = Period {
        start: Some(current.start),
        start_utc: Some(format_datetime(current.start)),
        next_utc_offset: next.as_ref().map(|n| n.offset),
        end: next.as_ref().map(|n| n.start),
        end_utc: next.as_ref().map(|n| format_datetime(n.start)),
    };
    zone_time.solar_utc_offset = lng.map(solar_offset);
    Ok(Some(zone_time))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn period(start: &str, offset: i64) -> ZonePeriod {
        ZonePeriod {
            start: parse_datetime(start).unwrap(),
            offset,
            abbreviation: String::new(),
            dst: false,
            country_code: String::new(),
        }
    }

    fn utc_of(periods: &[ZonePeriod], local: &str, first: bool) -> String {
        format_datetime(local_to_utc(periods, parse_datetime(local).unwrap(), first).unwrap())
    }

    /// America/Chicago around the 2010 changes.
    fn chicago() -> Vec<ZonePeriod> {
        vec![
            period("2009-11-01T07:00:00", -21600),
            period("2010-03-14T08:00:00", -18000),
            period("2010-11-07T07:00:00", -21600),
        ]
    }

    #[test]
    fn dates_parse_with_optional_parts() {
        assert_eq!(parse_datetime("1970-01-01"), Some(0));
        assert_eq!(parse_datetime("1970-01-02T00:00:01"), Some(86_401));
        assert_eq!(parse_datetime("2010-03-14 08:00"), Some(1_268_553_600));
        assert_eq!(parse_datetime("2010-03-14T08"), Some(1_268_553_600));
        assert_eq!(
            parse_datetime(" 2010-03-14T08:00:00.75Z "),
            Some(1_268_553_600)
        );
        assert_eq!(parse_datetime("1890-01-01T12:00:00"), Some(-2_524_478_400));
        for bad in [
            "",
            "2010",
            "2010-13-01",
            "2010-02-30",
            "2010-03-14T25:00",
            "2010-03-14T08:00:00:00",
            "yesterday",
        ] {
            assert_eq!(parse_datetime(bad), None, "{bad}");
        }
    }

    #[test]
    fn datetimes_format_and_convert() {
        assert_eq!(format_datetime(1_268_553_600), "2010-03-14T08:00:00");
        assert_eq!(unix_to_julian_day(1_656_676_800), 2_459_762.0);
        assert_eq!(julian_day_to_unix(2_459_762.0), 1_656_676_800);
        let day = week_day(parse_datetime("2022-10-30T01:30:00").unwrap()).unwrap();
        assert_eq!((day.iso, day.sun, day.abbr.as_str()), (7, 1, "Sun"));
        let friday = week_day(parse_datetime("2022-07-01").unwrap()).unwrap();
        assert_eq!((friday.iso, friday.sun), (5, 6));
    }

    #[test]
    fn local_time_before_and_after_dst_start() {
        // DST starts at 2010-03-14T08:00:00 UTC (02:00 local standard time).
        let zone = chicago();
        assert_eq!(
            utc_of(&zone, "2010-03-14T00:00:00", true),
            "2010-03-14T06:00:00"
        );
        assert_eq!(
            utc_of(&zone, "2010-03-14T01:59:59", true),
            "2010-03-14T07:59:59"
        );
        assert_eq!(
            utc_of(&zone, "2010-03-14T03:00:00", true),
            "2010-03-14T08:00:00"
        );
        assert_eq!(
            utc_of(&zone, "2010-07-01T12:00:00", true),
            "2010-07-01T17:00:00"
        );
        assert_eq!(
            utc_of(&zone, "2010-01-01T12:00:00", true),
            "2010-01-01T18:00:00"
        );
    }

    #[test]
    fn skipped_local_times_move_past_the_gap() {
        // 02:00-02:59 local does not exist on 2010-03-14.
        let zone = chicago();
        assert_eq!(
            utc_of(&zone, "2010-03-14T02:30:00", true),
            "2010-03-14T08:30:00"
        );
        assert_eq!(
            utc_of(&zone, "2010-03-14T02:30:00", false),
            "2010-03-14T08:30:00"
        );
        assert_eq!(
            utc_of(&zone, "2010-03-14T02:00:00", true),
            "2010-03-14T08:00:00"
        );
    }

    #[test]
    fn repeated_local_times_follow_the_dst_choice() {
        // 01:00-01:59 local occurs twice on 2010-11-07.
        let zone = chicago();
        assert_eq!(
            utc_of(&zone, "2010-11-07T01:30:00", true),
            "2010-11-07T06:30:00"
        );
        assert_eq!(
            utc_of(&zone, "2010-11-07T01:30:00", false),
            "2010-11-07T07:30:00"
        );
        assert_eq!(
            utc_of(&zone, "2010-11-07T00:59:59", false),
            "2010-11-07T05:59:59"
        );
        assert_eq!(
            utc_of(&zone, "2010-11-07T02:00:00", true),
            "2010-11-07T08:00:00"
        );
    }

    #[test]
    fn positive_offsets_and_missing_history() {
        // Europe/London, autumn 2022: BST (+3600) to GMT at 01:00 UTC.
        let london = vec![
            period("2022-03-27T01:00:00", 3600),
            period("2022-10-30T01:00:00", 0),
        ];
        assert_eq!(
            utc_of(&london, "2022-10-30T00:30:00", true),
            "2022-10-29T23:30:00"
        );
        assert_eq!(
            utc_of(&london, "2022-10-30T01:30:00", true),
            "2022-10-30T00:30:00"
        );
        assert_eq!(
            utc_of(&london, "2022-10-30T01:30:00", false),
            "2022-10-30T01:30:00"
        );
        assert_eq!(
            utc_of(&london, "2022-10-30T02:30:00", true),
            "2022-10-30T02:30:00"
        );
        let before_records = parse_datetime("2000-01-01").unwrap();
        assert_eq!(local_to_utc(&london, before_records, true), None);
        assert_eq!(local_to_utc(&[], before_records, true), None);
    }

    #[test]
    fn longitude_offsets() {
        assert_eq!(solar_offset(0.0), 0);
        assert_eq!(solar_offset(-3.1875), -765);
        assert_eq!(solar_offset(77.2), 18_528);
        assert_eq!(solar_offset(190.0), solar_offset(-170.0));
        assert_eq!(nautical_offset(-40.0), -3 * 3600);
        assert_eq!(nautical_offset(7.4), 0);
        assert_eq!(nautical_offset(7.6), 3600);
        assert_eq!(nautical_offset(179.0), 12 * 3600);
        assert_eq!(nautical_offset(-179.0), -12 * 3600);
    }

    #[test]
    fn natural_zones_use_solar_time_before_1900() {
        let old = TimeZone::natural(
            "Europe/London",
            "GB",
            -3.1875,
            When::Utc(parse_datetime("1800-01-01T12:00:00").unwrap()),
        );
        assert_eq!((old.abbreviation.as_str(), old.gmt_offset), ("SOL", -765));
        assert_eq!(old.local_dt, "1800-01-01T11:47:15");
        let sea = TimeZone::at_sea(
            30.0,
            -40.0,
            When::Local(parse_datetime("2022-07-01T12:00:00").unwrap()),
        );
        assert_eq!(sea.zone_name, "North_Atlantic/03W");
        assert_eq!(
            (sea.abbreviation.as_str(), sea.gmt_offset),
            ("LOC", -10_800)
        );
        assert_eq!(
            (sea.utc.as_str(), sea.local_dt.as_str()),
            ("2022-07-01T15:00:00", "2022-07-01T12:00:00")
        );
        assert_eq!(
            TimeZone::at_sea(-10.0, 80.0, When::Utc(0)).zone_name,
            "Indian/05E"
        );
        assert_eq!(
            TimeZone::at_sea(10.0, -150.0, When::Utc(0)).zone_name,
            "North_Pacific/10W"
        );
    }
}
