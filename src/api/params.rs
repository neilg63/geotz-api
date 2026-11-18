//! Query-string parameters shared by the endpoints.
//!
//! Every value arrives as text and is parsed here, so that a bad value gives
//! a JSON error naming the parameter instead of a framework rejection.

use serde::Deserialize;

use super::ApiError;
use super::time::{When, julian_day_to_unix, now, parse_datetime};

/// Strength applied when a soft preference gives none, e.g. `lang=it`.
pub const DEFAULT_STRENGTH: f64 = 0.5;

#[derive(Debug, Default, Deserialize)]
pub struct Params {
    /// UTC date-time.
    pub dt: Option<String>,
    /// Local wall-clock date-time at the place in question.
    pub dtl: Option<String>,
    /// Julian day (UTC).
    pub jd: Option<String>,
    /// Unix timestamp (UTC).
    pub un: Option<String>,
    /// IANA zone name.
    pub zn: Option<String>,
    /// `lat,lng`, optionally `:strength` where it is a search preference.
    pub loc: Option<String>,
    /// Place name. `q` is the preferred spelling; `place` is the original.
    pub q: Option<String>,
    pub place: Option<String>,
    /// Country code filter, or several separated by commas.
    pub cc: Option<String>,
    /// Region (first-level division) name filter.
    pub reg: Option<String>,
    /// Preferred languages for names: `it`, `it:80`, `it:60,en:30`.
    pub lang: Option<String>,
    /// Maximum results.
    pub max: Option<String>,
    /// 0 picks the second occurrence of a repeated local time.
    pub dst: Option<String>,
    /// Search radius in km for `/nearby`.
    pub radius: Option<String>,
    /// Original `/nearby` tolerance in degrees.
    pub fuzzy: Option<String>,
}

fn given(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// A geographic position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coords {
    pub lat: f64,
    pub lng: f64,
}

/// A language preference with a strength from 0 to 1.
#[derive(Debug, Clone, PartialEq)]
pub struct LangPref {
    pub lang: String,
    pub strength: f64,
}

/// Splits `value:strength`, reading the strength on a 0-100 scale.
fn split_strength<'a>(param: &str, text: &'a str) -> Result<(&'a str, f64), ApiError> {
    match text.rsplit_once(':') {
        None => Ok((text, DEFAULT_STRENGTH)),
        Some((value, strength)) => {
            let strength: f64 = strength.trim().parse().map_err(|_| {
                ApiError::bad_request(format!("{param}: strength must be a number from 0 to 100"))
            })?;
            Ok((value, strength.clamp(0.0, 100.0) / 100.0))
        }
    }
}

fn parse_coords(param: &str, text: &str) -> Result<Coords, ApiError> {
    let invalid = || {
        ApiError::bad_request(format!(
            "{param}: expected lat,lng in decimal degrees, e.g. 55.95,-3.19"
        ))
    };
    let mut parts = text.split(',').map(|p| p.trim().parse::<f64>());
    let lat = parts.next().and_then(Result::ok).ok_or_else(invalid)?;
    let lng = parts.next().and_then(Result::ok).ok_or_else(invalid)?;
    // A third part (altitude) is accepted and ignored.
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        return Err(ApiError::bad_request(format!(
            "{param}: latitude must be within -90 to 90 and longitude within -180 to 180"
        )));
    }
    Ok(Coords { lat, lng })
}

impl Params {
    /// The search text: `q`, or `place` as the original API called it.
    pub fn query(&self) -> Option<&str> {
        given(&self.q).or_else(|| given(&self.place))
    }

    pub fn zone(&self) -> Option<&str> {
        given(&self.zn)
    }

    pub fn region(&self) -> Option<&str> {
        given(&self.reg)
    }

    /// `loc` as a position, ignoring any strength.
    pub fn coords(&self) -> Result<Option<Coords>, ApiError> {
        Ok(self.proximity()?.map(|(coords, _)| coords))
    }

    /// `loc` as a search preference: position and strength (0 to 1).
    pub fn proximity(&self) -> Result<Option<(Coords, f64)>, ApiError> {
        let Some(text) = given(&self.loc) else {
            return Ok(None);
        };
        let (coords, strength) = split_strength("loc", text)?;
        Ok(Some((parse_coords("loc", coords)?, strength)))
    }

    /// Country codes to restrict to, upper-cased, with `UK` read as `GB`.
    pub fn countries(&self) -> Result<Vec<String>, ApiError> {
        let Some(text) = given(&self.cc) else {
            return Ok(Vec::new());
        };
        text.split(',')
            .map(|code| {
                let code = code.trim().to_uppercase();
                if code.len() != 2 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
                    return Err(ApiError::bad_request(format!(
                        "cc: {code:?} is not a two-letter country code"
                    )));
                }
                Ok(if code == "UK" { "GB".to_owned() } else { code })
            })
            .collect()
    }

    pub fn languages(&self) -> Result<Vec<LangPref>, ApiError> {
        let Some(text) = given(&self.lang) else {
            return Ok(Vec::new());
        };
        text.split(',')
            .map(|item| {
                let (lang, strength) = split_strength("lang", item.trim())?;
                let lang = lang.trim().split('-').next().unwrap_or("").to_lowercase();
                if !(2..=3).contains(&lang.len()) || !lang.chars().all(|c| c.is_ascii_alphabetic())
                {
                    return Err(ApiError::bad_request(format!(
                        "lang: {lang:?} is not a language code such as it or bar"
                    )));
                }
                Ok(LangPref { lang, strength })
            })
            .collect()
    }

    /// `max`, defaulting to `default` and capped at `limit`.
    pub fn max(&self, default: usize, limit: usize) -> Result<usize, ApiError> {
        match given(&self.max) {
            None => Ok(default),
            Some(text) => match text.parse::<usize>() {
                Ok(0) => Ok(default),
                Ok(n) => Ok(n.min(limit)),
                Err(_) => Err(ApiError::bad_request("max: expected a whole number")),
            },
        }
    }

    /// False only for `dst=0`: use the second occurrence of a local time
    /// that is repeated when clocks go back.
    pub fn first_occurrence(&self) -> Result<bool, ApiError> {
        match given(&self.dst) {
            None | Some("1") | Some("true") => Ok(true),
            Some("0") | Some("false") => Ok(false),
            Some(_) => Err(ApiError::bad_request("dst: expected 0 or 1")),
        }
    }

    /// Search radius for `/nearby` in km: `radius`, or the original `fuzzy`
    /// tolerance in degrees, or 100 km.
    pub fn radius_km(&self) -> Result<f64, ApiError> {
        let number = |param: &str, text: &str| {
            text.parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(|| {
                    ApiError::bad_request(format!("{param}: expected a positive number"))
                })
        };
        let km = if let Some(text) = given(&self.radius) {
            number("radius", text)?
        } else if let Some(text) = given(&self.fuzzy) {
            number("fuzzy", text)? * 111.2
        } else {
            100.0
        };
        Ok(km.min(500.0))
    }

    /// The moment the request refers to. The first of `dt`, `dtl`, `jd`
    /// and `un` that is present is used; with none, the current time.
    /// Only `dtl` is local time.
    pub fn when(&self) -> Result<When, ApiError> {
        let invalid = |param: &str| {
            ApiError::bad_request(format!(
                "{param}: expected a date-time such as 2010-03-14 or 2010-03-14T08:00:00"
            ))
        };
        if let Some(text) = given(&self.dt) {
            return parse_datetime(text)
                .map(When::Utc)
                .ok_or_else(|| invalid("dt"));
        }
        if let Some(text) = given(&self.dtl) {
            return parse_datetime(text)
                .map(When::Local)
                .ok_or_else(|| invalid("dtl"));
        }
        if let Some(text) = given(&self.jd) {
            return text
                .parse::<f64>()
                .ok()
                .filter(|jd| (0.0..10_000_000.0).contains(jd))
                .map(|jd| When::Utc(julian_day_to_unix(jd)))
                .ok_or_else(|| {
                    ApiError::bad_request("jd: expected a Julian day number, e.g. 2459762.0")
                });
        }
        if let Some(text) = given(&self.un) {
            return text
                .parse::<i64>()
                .ok()
                .filter(|ts| (-62_135_596_800..=253_402_300_799).contains(ts))
                .map(When::Utc)
                .ok_or_else(|| ApiError::bad_request("un: expected a Unix timestamp in seconds"));
        }
        Ok(When::Utc(now()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Params {
        let mut p = Params::default();
        for (key, value) in pairs {
            let value = Some(value.to_string());
            match *key {
                "dt" => p.dt = value,
                "dtl" => p.dtl = value,
                "jd" => p.jd = value,
                "un" => p.un = value,
                "loc" => p.loc = value,
                "q" => p.q = value,
                "place" => p.place = value,
                "cc" => p.cc = value,
                "lang" => p.lang = value,
                "max" => p.max = value,
                "dst" => p.dst = value,
                "radius" => p.radius = value,
                "fuzzy" => p.fuzzy = value,
                other => panic!("unknown test param {other}"),
            }
        }
        p
    }

    #[test]
    fn only_dtl_is_local_time() {
        assert_eq!(
            params(&[("dt", "2010-03-14T08:00:00")]).when().unwrap(),
            When::Utc(1_268_553_600)
        );
        assert_eq!(
            params(&[("dtl", "2010-03-14T08:00:00")]).when().unwrap(),
            When::Local(1_268_553_600)
        );
        assert_eq!(
            params(&[("un", "1268553600")]).when().unwrap(),
            When::Utc(1_268_553_600)
        );
        assert_eq!(
            params(&[("jd", "2459762.0")]).when().unwrap(),
            When::Utc(1_656_676_800)
        );
        assert!(matches!(params(&[]).when().unwrap(), When::Utc(ts) if ts > 1_700_000_000));
    }

    #[test]
    fn dt_takes_precedence_and_bad_dates_are_errors() {
        let both = params(&[("dt", "2010-03-14"), ("dtl", "2011-01-01"), ("un", "5")]);
        assert_eq!(both.when().unwrap(), When::Utc(1_268_524_800));
        assert!(params(&[("dt", "tomorrow")]).when().is_err());
        assert!(params(&[("dtl", "2010-02-30")]).when().is_err());
        assert!(params(&[("un", "12.5")]).when().is_err());
        assert!(params(&[("jd", "abc")]).when().is_err());
    }

    #[test]
    fn coordinates_allow_spaces_and_reject_nonsense() {
        let c = params(&[("loc", "55.95, -3.19")])
            .coords()
            .unwrap()
            .unwrap();
        assert_eq!((c.lat, c.lng), (55.95, -3.19));
        let with_alt = params(&[("loc", "43,-94,300")]).coords().unwrap().unwrap();
        assert_eq!((with_alt.lat, with_alt.lng), (43.0, -94.0));
        assert_eq!(params(&[]).coords().unwrap(), None);
        for bad in ["55.95", "north,west", "95,10", "10,190", ","] {
            assert!(params(&[("loc", bad)]).coords().is_err(), "{bad}");
        }
    }

    #[test]
    fn soft_preferences_carry_a_strength() {
        let (c, s) = params(&[("loc", "44,12:25")]).proximity().unwrap().unwrap();
        assert_eq!((c.lat, c.lng, s), (44.0, 12.0, 0.25));
        assert_eq!(
            params(&[("loc", "44,12")]).proximity().unwrap().unwrap().1,
            0.5
        );
        assert_eq!(
            params(&[("loc", "44,12:900")])
                .proximity()
                .unwrap()
                .unwrap()
                .1,
            1.0
        );
        assert!(params(&[("loc", "44,12:lots")]).proximity().is_err());

        let langs = params(&[("lang", "it:60, en:30,bar,de-CH")])
            .languages()
            .unwrap();
        let pairs: Vec<(&str, f64)> = langs
            .iter()
            .map(|l| (l.lang.as_str(), l.strength))
            .collect();
        assert_eq!(pairs, [("it", 0.6), ("en", 0.3), ("bar", 0.5), ("de", 0.5)]);
        assert!(params(&[("lang", "italian")]).languages().is_err());
        assert!(params(&[("lang", "it:x")]).languages().is_err());
    }

    #[test]
    fn country_codes_are_normalised() {
        assert_eq!(
            params(&[("cc", "it, ch")]).countries().unwrap(),
            ["IT", "CH"]
        );
        assert_eq!(params(&[("cc", "uk")]).countries().unwrap(), ["GB"]);
        assert!(params(&[("cc", "ITA")]).countries().is_err());
        assert!(params(&[]).countries().unwrap().is_empty());
    }

    #[test]
    fn small_options() {
        assert_eq!(params(&[]).max(20, 100).unwrap(), 20);
        assert_eq!(params(&[("max", "0")]).max(20, 100).unwrap(), 20);
        assert_eq!(params(&[("max", "500")]).max(20, 100).unwrap(), 100);
        assert!(params(&[("max", "many")]).max(20, 100).is_err());
        assert!(params(&[]).first_occurrence().unwrap());
        assert!(!params(&[("dst", "0")]).first_occurrence().unwrap());
        assert!(params(&[("dst", "2")]).first_occurrence().is_err());
        assert_eq!(params(&[]).radius_km().unwrap(), 100.0);
        assert_eq!(params(&[("radius", "30")]).radius_km().unwrap(), 30.0);
        assert_eq!(params(&[("fuzzy", "1")]).radius_km().unwrap(), 111.2);
        assert_eq!(params(&[("radius", "9000")]).radius_km().unwrap(), 500.0);
        assert_eq!(
            params(&[("q", " Monac "), ("place", "x")]).query(),
            Some("Monac")
        );
        assert_eq!(params(&[("place", "Edinburgh")]).query(), Some("Edinburgh"));
    }
}
