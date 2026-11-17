//! Parsers for the GeoNames tab-separated dump formats.
//!
//! The files are plain TSV with no quoting or escaping, so lines are split on
//! tabs directly. Column layouts are documented in the GeoNames `readme.txt`.

use std::collections::HashMap;

/// One row of `allCountries.txt` or a per-country file (19 columns).
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub id: i64,
    pub name: String,
    pub ascii_name: String,
    pub lat: f64,
    pub lng: f64,
    pub fclass: String,
    pub fcode: String,
    pub cc: String,
    pub adm1: String,
    pub adm2: String,
    pub population: i64,
    pub zone_name: String,
    pub date: String,
}

/// Borrowed view of the columns needed to decide whether a row is wanted,
/// so rejected rows (the large majority) cost no allocation.
pub struct PlaceLine<'a> {
    cols: [&'a str; 19],
}

impl<'a> PlaceLine<'a> {
    pub fn parse(line: &'a str) -> Option<Self> {
        let mut cols = [""; 19];
        let mut parts = line.split('\t');
        for col in cols.iter_mut() {
            *col = parts.next()?;
        }
        Some(PlaceLine { cols })
    }

    pub fn fcode(&self) -> &'a str {
        self.cols[7]
    }

    pub fn population(&self) -> i64 {
        self.cols[14].parse().unwrap_or(0)
    }

    pub fn to_place(&self) -> Option<Place> {
        let c = &self.cols;
        Some(Place {
            id: c[0].parse().ok()?,
            name: c[1].to_owned(),
            ascii_name: c[2].to_owned(),
            lat: c[4].parse().ok()?,
            lng: c[5].parse().ok()?,
            fclass: c[6].to_owned(),
            fcode: c[7].to_owned(),
            cc: c[8].to_owned(),
            adm1: c[10].to_owned(),
            adm2: c[11].to_owned(),
            population: self.population(),
            zone_name: c[17].to_owned(),
            date: c[18].to_owned(),
        })
    }
}

/// A named administrative division from `admin1CodesASCII.txt` or
/// `admin2Codes.txt`.
#[derive(Debug, Clone, PartialEq)]
pub struct AdminCode {
    pub cc: String,
    pub adm1: String,
    /// Empty for first-level divisions.
    pub adm2: String,
    pub name: String,
    pub ascii_name: String,
    pub geoname_id: Option<i64>,
}

/// Parses `CC.ADM1<TAB>name<TAB>asciiname<TAB>geonameid`.
pub fn parse_admin1(line: &str) -> Option<AdminCode> {
    let mut cols = line.split('\t');
    let (cc, adm1) = cols.next()?.split_once('.')?;
    admin_code(cc, adm1, "", cols)
}

/// Parses `CC.ADM1.ADM2<TAB>name<TAB>asciiname<TAB>geonameid`.
pub fn parse_admin2(line: &str) -> Option<AdminCode> {
    let mut cols = line.split('\t');
    let mut key = cols.next()?.splitn(3, '.');
    let (cc, adm1, adm2) = (key.next()?, key.next()?, key.next()?);
    admin_code(cc, adm1, adm2, cols)
}

fn admin_code<'a>(
    cc: &str,
    adm1: &str,
    adm2: &str,
    mut cols: impl Iterator<Item = &'a str>,
) -> Option<AdminCode> {
    let name = cols.next()?;
    let ascii_name = cols.next().unwrap_or("");
    let geoname_id = cols.next().and_then(|v| v.trim().parse().ok());
    Some(AdminCode {
        cc: cc.to_owned(),
        adm1: adm1.to_owned(),
        adm2: adm2.to_owned(),
        name: name.to_owned(),
        ascii_name: ascii_name.to_owned(),
        geoname_id,
    })
}

/// A row of `featureCodes_en.txt`: `CLASS.CODE<TAB>name<TAB>description`.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureCode {
    pub fclass: String,
    pub fcode: String,
    pub name: String,
    pub description: String,
}

pub fn parse_feature_code(line: &str) -> Option<FeatureCode> {
    let mut cols = line.split('\t');
    let (fclass, fcode) = cols.next()?.split_once('.')?;
    Some(FeatureCode {
        fclass: fclass.to_owned(),
        fcode: fcode.to_owned(),
        name: cols.next()?.to_owned(),
        description: cols.next().unwrap_or("").to_owned(),
    })
}

/// Resolves admin codes on a place to the names of its region and district.
#[derive(Default)]
pub struct AdminNames {
    admin1: HashMap<(String, String), String>,
    admin2: HashMap<(String, String, String), String>,
}

impl AdminNames {
    pub fn add(&mut self, code: &AdminCode) {
        if code.adm2.is_empty() {
            self.admin1.insert((code.cc.clone(), code.adm1.clone()), code.name.clone());
        } else {
            self.admin2.insert(
                (code.cc.clone(), code.adm1.clone(), code.adm2.clone()),
                code.name.clone(),
            );
        }
    }

    /// First-level division name, e.g. `Scotland`.
    pub fn region(&self, cc: &str, adm1: &str) -> Option<&str> {
        if adm1.is_empty() {
            return None;
        }
        self.admin1.get(&(cc.to_owned(), adm1.to_owned())).map(String::as_str)
    }

    /// Second-level division name, e.g. `City of Edinburgh`.
    ///
    /// Returns `None` when the place has no `adm2` code. The original import
    /// fell back to an arbitrary district of the same region in that case.
    pub fn admin_name(&self, cc: &str, adm1: &str, adm2: &str) -> Option<&str> {
        if adm2.is_empty() {
            return None;
        }
        self.admin2
            .get(&(cc.to_owned(), adm1.to_owned(), adm2.to_owned()))
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDINBURGH: &str = "2650225\tEdinburgh\tEdinburgh\tDun Eideann,Edimburgo\t55.95206\t-3.19648\tP\tPPLA\tGB\t\tSCT\tV2\t\t\t506520\t\t74\tEurope/London\t2023-01-05";

    #[test]
    fn place_rows_parse() {
        let line = PlaceLine::parse(EDINBURGH).unwrap();
        assert_eq!(line.fcode(), "PPLA");
        assert_eq!(line.population(), 506520);
        let place = line.to_place().unwrap();
        assert_eq!(place.id, 2650225);
        assert_eq!((place.lat, place.lng), (55.95206, -3.19648));
        assert_eq!((place.fclass.as_str(), place.cc.as_str()), ("P", "GB"));
        assert_eq!((place.adm1.as_str(), place.adm2.as_str()), ("SCT", "V2"));
        assert_eq!(place.zone_name, "Europe/London");
        assert_eq!(place.date, "2023-01-05");
    }

    #[test]
    fn short_or_malformed_place_rows_are_rejected() {
        assert!(PlaceLine::parse("1\tonly\tthree").is_none());
        let bad_lat = EDINBURGH.replace("55.95206", "north");
        assert!(PlaceLine::parse(&bad_lat).unwrap().to_place().is_none());
    }

    #[test]
    fn admin_codes_parse() {
        let a1 = parse_admin1("GR.ESYE22\tIonian Islands\tIonian Islands\t6697804").unwrap();
        assert_eq!((a1.cc.as_str(), a1.adm1.as_str(), a1.adm2.as_str()), ("GR", "ESYE22", ""));
        assert_eq!(a1.geoname_id, Some(6697804));
        let a2 = parse_admin2("GR.ESYE22.26\tLefkada\tLefkada\t258445").unwrap();
        assert_eq!((a2.adm1.as_str(), a2.adm2.as_str(), a2.name.as_str()), ("ESYE22", "26", "Lefkada"));
        assert!(parse_admin2("GR.ESYE22\tno second level\tx\t1").is_none());
    }

    #[test]
    fn names_resolve_only_with_a_matching_code() {
        let mut names = AdminNames::default();
        names.add(&parse_admin1("GR.ESYE22\tIonian Islands\tIonian Islands\t1").unwrap());
        names.add(&parse_admin2("GR.ESYE22.26\tLefkada\tLefkada\t2").unwrap());
        assert_eq!(names.region("GR", "ESYE22"), Some("Ionian Islands"));
        assert_eq!(names.admin_name("GR", "ESYE22", "26"), Some("Lefkada"));
        assert_eq!(names.admin_name("GR", "ESYE22", ""), None);
        assert_eq!(names.admin_name("GR", "ESYE22", "99"), None);
        assert_eq!(names.region("GR", ""), None);
        assert_eq!(names.region("IT", "ESYE22"), None);
    }

    #[test]
    fn feature_codes_parse() {
        let code = parse_feature_code("P.PPLA\tseat of a first-order administrative division\tseat of ...").unwrap();
        assert_eq!((code.fclass.as_str(), code.fcode.as_str()), ("P", "PPLA"));
        assert!(parse_feature_code("null\tnot available\t").is_none());
    }
}
