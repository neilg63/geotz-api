//! Place lookups: nearest place to a point, and name search.

use std::collections::HashMap;

use serde::Serialize;
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use super::ApiError;
use super::params::{Coords, LangPref};

const EARTH_RADIUS_KM: f64 = 6371.0;
const KM_PER_DEGREE: f64 = 111.2;

/// Most name matches read for one search before ranking. Reached only by
/// very short prefixes; the largest places are read first.
const MAX_CANDIDATES: i64 = 4000;

/// Distance over which a proximity preference fades.
const PROXIMITY_SCALE_KM: f64 = 500.0;

const PLACE_COLUMNS: &str = "t.pl_id, t.name, t.ascii_name, t.cc, t.fclass, t.fcode, t.lat, t.lng, \
                             t.population, t.zone_name, t.admin_name, t.region";

/// A row of `toponyms`.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub id: i64,
    pub name: String,
    pub ascii_name: Option<String>,
    pub cc: Option<String>,
    pub fclass: Option<String>,
    pub fcode: Option<String>,
    pub lat: f64,
    pub lng: f64,
    pub population: i64,
    pub zone_name: Option<String>,
    pub admin_name: Option<String>,
    pub region: Option<String>,
    /// Set by [`search`] when the region is just another name for the place
    /// itself, as `Ciudad de México` is for Mexico City.
    pub region_is_own_name: bool,
    /// Set by [`search`]: the region's conventional short name, such as
    /// `Jakarta` for `Daerah Khusus Ibukota Jakarta`.
    pub short_region: Option<String>,
}

impl Place {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Place {
            id: row.try_get("pl_id")?,
            name: row.try_get("name")?,
            ascii_name: row.try_get("ascii_name")?,
            cc: row.try_get("cc")?,
            fclass: row.try_get("fclass")?,
            fcode: row.try_get("fcode")?,
            lat: row.try_get("lat")?,
            lng: row.try_get("lng")?,
            population: row.try_get("population")?,
            zone_name: row.try_get("zone_name")?,
            admin_name: row.try_get("admin_name")?,
            region: row.try_get("region")?,
            region_is_own_name: false,
            short_region: None,
        })
    }

    /// `Name, Region (CC)`. The region is shortened to its own name and
    /// left out when missing or named after the place itself.
    pub fn label(&self) -> String {
        let mut text = self.name.clone();
        let region = self
            .short_region
            .as_deref()
            .or(self.region.as_deref())
            .filter(|_| !self.region_is_own_name)
            .and_then(|region| super::tidy::region_for_label(&self.name, region));
        if let Some(region) = region {
            text.push_str(", ");
            text.push_str(region);
        }
        if let Some(cc) = &self.cc {
            text.push_str(&format!(" ({cc})"));
        }
        text
    }
}

/// Great-circle distance in km.
pub fn distance_km(a: Coords, b: Coords) -> f64 {
    let (lat1, lat2) = (a.lat.to_radians(), b.lat.to_radians());
    let dlat = lat2 - lat1;
    let dlng = (b.lng - a.lng).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlng / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * h.sqrt().min(1.0).asin()
}

/// Latitude range and one or two longitude ranges enclosing a circle of
/// `radius_km` around `centre`. Two longitude ranges are needed when the
/// circle crosses the 180th meridian; near a pole every longitude qualifies.
#[derive(Debug, PartialEq)]
pub struct SearchBox {
    pub lat: (f64, f64),
    pub lng: (f64, f64),
    pub lng_wrapped: Option<(f64, f64)>,
}

pub fn search_box(centre: Coords, radius_km: f64) -> SearchBox {
    let dlat = radius_km / KM_PER_DEGREE;
    let lat = (
        (centre.lat - dlat).max(-90.0),
        (centre.lat + dlat).min(90.0),
    );
    // A degree of longitude shrinks with latitude; widen the box to match.
    let widest_lat = lat.0.abs().max(lat.1.abs());
    let shrink = widest_lat.to_radians().cos();
    if shrink < 0.01 || dlat / shrink >= 180.0 {
        return SearchBox {
            lat,
            lng: (-180.0, 180.0),
            lng_wrapped: None,
        };
    }
    let dlng = dlat / shrink;
    let (west, east) = (centre.lng - dlng, centre.lng + dlng);
    if west < -180.0 {
        SearchBox {
            lat,
            lng: (-180.0, east),
            lng_wrapped: Some((west + 360.0, 180.0)),
        }
    } else if east > 180.0 {
        SearchBox {
            lat,
            lng: (west, 180.0),
            lng_wrapped: Some((-180.0, east - 360.0)),
        }
    } else {
        SearchBox {
            lat,
            lng: (west, east),
            lng_wrapped: None,
        }
    }
}

/// Which features count as "a place" for a nearest-place lookup.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scope {
    /// Cities, towns and villages only.
    Populated,
    /// Any imported feature that has a time zone.
    AnyWithZone,
}

/// The place closest to `centre` within `radius_km`, with its distance.
pub async fn nearest_within(
    pool: &SqlitePool,
    centre: Coords,
    radius_km: f64,
    scope: Scope,
) -> Result<Option<(Place, f64)>, ApiError> {
    let area = search_box(centre, radius_km);
    // An impossible range stands in when no second longitude range is needed.
    let wrapped = area.lng_wrapped.unwrap_or((1.0, -1.0));
    let filter = match scope {
        Scope::Populated => "t.fclass = 'P'",
        Scope::AnyWithZone => "t.zone_name IS NOT NULL",
    };
    let sql = format!(
        "SELECT {PLACE_COLUMNS} FROM toponyms_rtree r JOIN toponyms t ON t.pl_id = r.id
         WHERE r.min_lat >= ? AND r.max_lat <= ?
           AND ((r.min_lng >= ? AND r.max_lng <= ?) OR (r.min_lng >= ? AND r.max_lng <= ?))
           AND {filter}"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(area.lat.0)
        .bind(area.lat.1)
        .bind(area.lng.0)
        .bind(area.lng.1)
        .bind(wrapped.0)
        .bind(wrapped.1)
        .fetch_all(pool)
        .await?;
    let mut best: Option<(Place, f64)> = None;
    for row in &rows {
        let place = Place::from_row(row)?;
        let distance = distance_km(
            centre,
            Coords {
                lat: place.lat,
                lng: place.lng,
            },
        );
        if distance <= radius_km && best.as_ref().is_none_or(|(_, d)| distance < *d) {
            best = Some((place, distance));
        }
    }
    Ok(best)
}

/// The place that best represents a position: the nearest town or village,
/// looking further out in steps, then any feature with a time zone. `None`
/// means open sea or uninhabited land.
pub async fn locate(pool: &SqlitePool, centre: Coords) -> Result<Option<(Place, f64)>, ApiError> {
    for radius in [25.0, 100.0, 300.0] {
        if let Some(found) = nearest_within(pool, centre, radius, Scope::Populated).await? {
            return Ok(Some(found));
        }
    }
    nearest_within(pool, centre, 300.0, Scope::AnyWithZone).await
}

pub async fn country_name(pool: &SqlitePool, cc: &str) -> Result<Option<String>, ApiError> {
    Ok(
        sqlx::query_scalar("SELECT country_name FROM countries WHERE country_code = ?")
            .bind(cc)
            .fetch_optional(pool)
            .await?,
    )
}

// --------------------------------------------------------------- name search

/// Lower-cases and removes accents: `München` -> `munchen`.
pub fn fold(text: &str) -> String {
    text.nfd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Words of a name or query, folded.
pub(super) fn words(text: &str) -> Vec<String> {
    fold(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Builds the full-text query: every word must start a word of the name.
/// Returns `None` if the text has fewer than two letters or digits.
pub fn match_expression(query: &str) -> Option<String> {
    let tokens = words(query);
    if tokens.iter().map(|t| t.chars().count()).sum::<usize>() < 2 {
        return None;
    }
    Some(
        tokens
            .iter()
            .map(|t| format!("\"{t}\"*"))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// How well the query fits a name, from 0 to 1, judged on the first query
/// word: the share of the matched word that has been typed (`monac` against
/// `Monaco di Baviera` is 5/6), reduced when the match is not the name's
/// first word (`lond` against `East London`).
fn closeness(query_words: &[String], name: &str) -> f64 {
    const LATER_WORD: f64 = 0.6;
    let Some(first) = query_words.first() else {
        return 0.0;
    };
    let typed = first.chars().count() as f64;
    words(name)
        .iter()
        .enumerate()
        .filter(|(_, word)| word.starts_with(first.as_str()))
        .map(|(position, word)| {
            let share = typed / word.chars().count() as f64;
            if position == 0 {
                share
            } else {
                share * LATER_WORD
            }
        })
        .fold(0.2, f64::max)
}

/// What a search is asked to favour or restrict to.
#[derive(Debug, Default, Clone)]
pub struct SearchOptions {
    /// Hard filter: only these countries. Empty means all.
    pub countries: Vec<String>,
    /// Hard filter: region name must start with this (accent-insensitive).
    pub region: Option<String>,
    /// Soft: favour names in these languages.
    pub languages: Vec<LangPref>,
    /// Soft: favour places near this position, with a strength of 0 to 1.
    pub proximity: Option<(Coords, f64)>,
    /// Include administrative divisions as well as populated places.
    pub include_admin: bool,
    pub max: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Match {
    #[serde(skip)]
    pub place: Place,
    /// The name that matched, which may be an alternate name.
    pub matched: String,
    pub score: f64,
}

/// Prepares the results' regions for labels, from data the importer
/// already stores.
///
/// * `short_region` is the region's conventional short name from the
///   GeoNames region codes: `Jakarta` for `Daerah Khusus Ibukota Jakarta`,
///   `Maharashtra` for `State of Mahārāshtra`.
/// * `region_is_own_name` is set when the region is named after the place
///   itself, so that a label does not say the same thing twice. Either the
///   region is one of the place's own names (`Mexico City` in `Ciudad de
///   México`), or one of the region's names is the place's (`Moscow` in
///   `Moskva`, also known as Moscow). A region that is a separate entity
///   shares no name with its city and is kept: `Munich` in `Bavaria`.
async fn mark_regions_named_for_the_place(
    pool: &SqlitePool,
    matches: &mut [Match],
) -> Result<(), ApiError> {
    let with_region: Vec<i64> = matches
        .iter()
        .filter(|found| found.place.region.as_deref().is_some_and(|r| !r.is_empty()))
        .map(|found| found.place.id)
        .collect();
    if with_region.is_empty() {
        return Ok(());
    }
    // Each place's names, its region's names, and its region's short name.
    let sources = [
        "SELECT pl_id, name FROM place_names WHERE pl_id IN (",
        "SELECT t.pl_id, n.name FROM toponyms t
         JOIN admin1_codes a ON a.cc = t.cc AND a.adm1 = t.adm1
         JOIN place_names n ON n.pl_id = a.geoname_id
         WHERE t.pl_id IN (",
        "SELECT t.pl_id, a.name FROM toponyms t
         JOIN admin1_codes a ON a.cc = t.cc AND a.adm1 = t.adm1
         WHERE t.pl_id IN (",
    ];
    let mut names: [HashMap<i64, Vec<String>>; 3] = Default::default();
    for (source, found) in sources.into_iter().zip(names.iter_mut()) {
        let mut sql: QueryBuilder<Sqlite> = QueryBuilder::new(source);
        let mut ids = sql.separated(", ");
        for id in &with_region {
            ids.push_bind(*id);
        }
        sql.push(")");
        for row in sql.build().fetch_all(pool).await? {
            found
                .entry(row.try_get("pl_id")?)
                .or_default()
                .push(row.try_get("name")?);
        }
    }
    let [own_names, region_names, mut short_names] = names;
    let none = Vec::new();
    for found in matches.iter_mut() {
        let place = &mut found.place;
        let Some(region) = place.region.as_deref() else {
            continue;
        };
        place.short_region = short_names
            .remove(&place.id)
            .and_then(|mut names| names.pop())
            .filter(|n| !n.is_empty());
        let own = own_names.get(&place.id).unwrap_or(&none);
        let of_region = region_names.get(&place.id).unwrap_or(&none);
        let aliases = of_region.iter().chain(place.short_region.iter());
        place.region_is_own_name = own
            .iter()
            .any(|name| super::tidy::named_after(region, name))
            || aliases.into_iter().any(|alias| {
                super::tidy::named_after(alias, &place.name)
                    || own.iter().any(|name| super::tidy::named_after(alias, name))
            });
    }
    Ok(())
}

/// Scores one matching name of a place.
///
/// * `weight`: 1-100, how expected this name is for the place.
/// * `langs`: languages the name is tagged with.
/// * A name in a preferred language is lifted towards primary-name weight,
///   fully so from strength 50, and gains up to a further 50%.
/// * Population counts on a log scale so a metropolis does not swamp
///   everything else sharing a name.
/// * A proximity preference multiplies by up to 1 + strength at the given
///   position, fading with distance.
fn score(
    weight: i64,
    langs: Option<&str>,
    population: i64,
    closeness: f64,
    distance_km: Option<f64>,
    options: &SearchOptions,
) -> f64 {
    let lang_strength = langs
        .map(|tagged| {
            options
                .languages
                .iter()
                .filter(|pref| tagged.split(' ').any(|l| l == pref.lang))
                .map(|pref| pref.strength)
                .fold(0.0, f64::max)
        })
        .unwrap_or(0.0);
    let weight = weight as f64;
    let lifted = weight + (100.0 - weight) * (lang_strength * 2.0).min(1.0);
    let mut score = lifted / 100.0 * ((population.max(0) + 1000) as f64).log10() * closeness;
    score *= 1.0 + lang_strength * 0.5;
    if let (Some(distance), Some((_, strength))) = (distance_km, options.proximity) {
        score *= 1.0 + strength * (-distance / PROXIMITY_SCALE_KM).exp();
    }
    score
}

/// Finds places whose names start with the query words, best first.
pub async fn search(
    pool: &SqlitePool,
    query: &str,
    options: &SearchOptions,
) -> Result<Vec<Match>, ApiError> {
    let Some(expression) = match_expression(query) else {
        return Ok(Vec::new());
    };
    let query_words = words(query);

    let mut sql: QueryBuilder<Sqlite> = QueryBuilder::new(format!(
        "SELECT {PLACE_COLUMNS}, n.name AS matched, n.langs, n.weight
         FROM place_names_fts f
         JOIN place_names n ON n.id = f.rowid
         JOIN toponyms t ON t.pl_id = n.pl_id
         WHERE place_names_fts MATCH "
    ));
    sql.push_bind(expression);
    sql.push(if options.include_admin {
        " AND t.fclass IN ('P', 'A')"
    } else {
        " AND t.fclass = 'P'"
    });
    if !options.countries.is_empty() {
        sql.push(" AND t.cc IN (");
        let mut list = sql.separated(", ");
        for cc in &options.countries {
            list.push_bind(cc.clone());
        }
        sql.push(")");
    }
    // Name ids are assigned by the importer in order of population, and the
    // index yields matches in id order, so this reads the largest places
    // first and stops at the limit without sorting.
    sql.push(" ORDER BY f.rowid LIMIT ");
    sql.push_bind(MAX_CANDIDATES);
    let rows = sql.build().fetch_all(pool).await?;

    let region_filter = options.region.as_deref().map(fold);
    let mut best: HashMap<i64, Match> = HashMap::new();
    for row in &rows {
        let place = Place::from_row(row)?;
        if let Some(wanted) = &region_filter {
            let in_region = [&place.region, &place.admin_name]
                .into_iter()
                .flatten()
                .any(|name| fold(name).starts_with(wanted.as_str()));
            if !in_region {
                continue;
            }
        }
        let matched: String = row.try_get("matched")?;
        let langs: Option<String> = row.try_get("langs")?;
        let weight: i64 = row.try_get("weight")?;
        let distance = options.proximity.map(|(centre, _)| {
            distance_km(
                centre,
                Coords {
                    lat: place.lat,
                    lng: place.lng,
                },
            )
        });
        let score = score(
            weight,
            langs.as_deref(),
            place.population,
            closeness(&query_words, &matched),
            distance,
            options,
        );
        match best.get_mut(&place.id) {
            Some(existing) if existing.score >= score => {}
            Some(existing) => {
                *existing = Match {
                    place,
                    matched,
                    score,
                }
            }
            None => {
                best.insert(
                    place.id,
                    Match {
                        place,
                        matched,
                        score,
                    },
                );
            }
        }
    }
    let mut matches: Vec<Match> = best.into_values().collect();
    matches.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.place.population.cmp(&a.place.population))
    });
    let mut matches = super::tidy::without_overlaps(matches);
    matches.truncate(options.max.max(1));
    mark_regions_named_for_the_place(pool, &mut matches).await?;
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(lat: f64, lng: f64) -> Coords {
        Coords { lat, lng }
    }

    #[test]
    fn distances() {
        assert!(distance_km(at(55.95, -3.19), at(55.95, -3.19)) < 1e-9);
        let edinburgh_london = distance_km(at(55.9521, -3.1965), at(51.5074, -0.1278));
        assert!((edinburgh_london - 534.0).abs() < 3.0, "{edinburgh_london}");
        let across_date_line = distance_km(at(0.0, 179.5), at(0.0, -179.5));
        assert!((across_date_line - 111.2).abs() < 0.5, "{across_date_line}");
    }

    #[test]
    fn boxes_widen_with_latitude_and_wrap() {
        let equator = search_box(at(0.0, 10.0), 111.2);
        assert_eq!(equator.lat, (-1.0, 1.0));
        assert!((equator.lng.1 - equator.lng.0 - 2.0).abs() < 0.01);
        assert_eq!(equator.lng_wrapped, None);

        let scotland = search_box(at(56.0, -3.0), 111.2);
        let width = scotland.lng.1 - scotland.lng.0;
        assert!(width > 3.5 && width < 3.8, "{width}");

        let fiji = search_box(at(-17.0, 179.5), 111.2);
        assert_eq!(fiji.lng.1, 180.0);
        let (west, east) = fiji.lng_wrapped.unwrap();
        assert_eq!(west, -180.0);
        assert!(east > -180.0 && east < -179.0);

        let aleutians = search_box(at(52.0, -179.5), 111.2);
        assert_eq!(aleutians.lng.0, -180.0);
        assert_eq!(aleutians.lng_wrapped.unwrap().1, 180.0);

        assert_eq!(search_box(at(89.9, 0.0), 100.0).lng, (-180.0, 180.0));
    }

    #[test]
    fn folding_and_match_expressions() {
        assert_eq!(fold("München"), "munchen");
        assert_eq!(fold("Hà Nội"), "ha noi");
        assert_eq!(match_expression("Monac").as_deref(), Some("\"monac\"*"));
        assert_eq!(
            match_expression(" San  Fr ").as_deref(),
            Some("\"san\"* \"fr\"*")
        );
        assert_eq!(
            match_expression("Saint-Étienne").as_deref(),
            Some("\"saint\"* \"etienne\"*")
        );
        assert_eq!(
            match_expression("O'Fallon \"x\" OR *").as_deref(),
            Some("\"o\"* \"fallon\"* \"x\"* \"or\"*")
        );
        assert_eq!(match_expression("m"), None);
        assert_eq!(match_expression("  -- "), None);
    }

    #[test]
    fn closeness_is_judged_on_the_matched_word() {
        let q = words("monac");
        assert!((closeness(&q, "Monaco") - 5.0 / 6.0).abs() < 1e-9);
        assert!((closeness(&q, "Monaco di Baviera") - 5.0 / 6.0).abs() < 1e-9);
        assert!((closeness(&words("lond"), "East London") - 0.6 * 4.0 / 6.0).abs() < 1e-9);
        assert!(closeness(&words("sa"), "Sa Dec") > closeness(&words("sa"), "Lungsod sa Dabaw"));
        assert!((closeness(&words("san"), "San Antonio") - 1.0).abs() < 1e-9);
        assert_eq!(closeness(&words("munchen"), "München"), 1.0);
    }

    fn options(lang: Option<(&str, f64)>, proximity: Option<f64>) -> SearchOptions {
        SearchOptions {
            languages: lang
                .map(|(l, s)| LangPref {
                    lang: l.into(),
                    strength: s,
                })
                .into_iter()
                .collect(),
            proximity: proximity.map(|s| (at(0.0, 0.0), s)),
            max: 10,
            ..Default::default()
        }
    }

    #[test]
    fn monaco_wins_by_default_and_munich_for_italian_speakers() {
        let close = 5.0 / 6.0;
        let monaco = |o: &SearchOptions| score(100, Some("fr de en it"), 32_965, close, None, o);
        let munich = |o: &SearchOptions| score(50, Some("it"), 1_260_391, close, None, o);
        let neutral = options(None, None);
        assert!(monaco(&neutral) > munich(&neutral));
        let italian = options(Some(("it", 0.5)), None);
        assert!(munich(&italian) > monaco(&italian));
        let faint = options(Some(("it", 0.1)), None);
        assert!(
            monaco(&faint) > munich(&faint),
            "a weak preference only narrows the gap"
        );
        assert!(munich(&faint) > munich(&neutral));
        let german = options(Some(("de", 0.5)), None);
        assert!(
            monaco(&german) > munich(&german),
            "other languages do not lift the Italian name"
        );
    }

    #[test]
    fn proximity_boost_fades_with_distance() {
        let o = options(None, Some(0.5));
        let base = score(100, None, 50_000, 1.0, None, &o);
        let here = score(100, None, 50_000, 1.0, Some(0.0), &o);
        let far = score(100, None, 50_000, 1.0, Some(5000.0), &o);
        assert!((here / base - 1.5).abs() < 1e-9);
        assert!(far / base < 1.001);
        let strong = options(None, Some(1.0));
        assert!((score(100, None, 50_000, 1.0, Some(0.0), &strong) / base - 2.0).abs() < 1e-9);
    }

    #[test]
    fn labels() {
        let mut place = Place {
            id: 1,
            name: "Edinburgh".into(),
            ascii_name: None,
            cc: Some("GB".into()),
            fclass: Some("P".into()),
            fcode: Some("PPLA".into()),
            lat: 0.0,
            lng: 0.0,
            population: 0,
            zone_name: None,
            admin_name: None,
            region: Some("Scotland".into()),
            region_is_own_name: false,
            short_region: None,
        };
        assert_eq!(place.label(), "Edinburgh, Scotland (GB)");
        place.region = Some("Edinburgh".into());
        assert_eq!(place.label(), "Edinburgh (GB)");
        place.region = None;
        place.cc = None;
        assert_eq!(place.label(), "Edinburgh");
    }
}
