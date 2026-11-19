//! Tidies search results for display: shortens region names that only
//! repeat what the place name already says, and removes results that are
//! parts or duplicates of a larger place a few kilometres away.

use std::collections::HashMap;

use super::params::Coords;
use super::places::{Match, Place, distance_km, fold, words};

/// Results closer than this to a related, more important result are dropped.
pub const OVERLAP_KM: f64 = 10.0;

/// Districts and other lesser places give way to a city of at least
/// [`METROPOLIS`] people out to this distance, since such a city extends
/// well beyond 10 km: Köpenick is 12 km from the centre of Berlin.
pub const METROPOLIS_KM: f64 = 25.0;
pub const METROPOLIS: i64 = 1_000_000;

/// Words that say what kind of division a region is, as they appear before
/// its name: "State of ...", "Departamento de ...", "Changwat ...".
const KINDS: &[&str] = &[
    "state",
    "province",
    "region",
    "territory",
    "republic",
    "municipality",
    "parish",
    "emirate",
    "principality",
    "department",
    "prefecture",
    "estado",
    "departamento",
    "provincia",
    "distrito",
    "comunidad",
    "comunitat",
    "municipio",
    "concelho",
    "departement",
    "gouvernorat",
    "wilaya",
    "canton",
    "commune",
    "changwat",
    "provinsi",
    "provincie",
    "kanton",
    "gobolka",
    "land",
    "city",
];

/// Qualifiers that may accompany a kind: "National Capital Territory of".
const QUALIFIERS: &[&str] = &[
    "national",
    "capital",
    "union",
    "autonomous",
    "autonoma",
    "urban",
    "federal",
    "special",
    "free",
    "and",
    "hanseatic",
];

/// Further words ignored only when deciding whether a region is named after
/// the place itself: "Ciudad de ...", "District of ...". They are never
/// removed from a region that is shown, where the short form could be
/// mistaken for another region ("México", "Columbia").
const CITY_WORDS: &[&str] = &["ciudad", "district", "county"];

const LINKS: &[&str] = &["of", "de", "del", "do", "da", "di", "du", "des"];

/// Kinds that follow the name instead: "Lagos State", "Kraków County".
const TRAILING_KINDS: &[&str] = &[
    "state",
    "province",
    "region",
    "district",
    "county",
    "oblast",
    "governorate",
    "municipality",
    "city",
    "prefecture",
    "department",
    "voivodeship",
    "division",
    "territory",
    "kraj",
    "kray",
    "megye",
    "novads",
    "rayon",
    "fylke",
];

fn first_word(text: &str) -> (&str, &str) {
    match text.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim_start()),
        None => (text, ""),
    }
}

/// The region's own name without a leading description of its kind:
/// `State of Mahārāshtra` -> `Mahārāshtra`, `Estado de Guerrero` ->
/// `Guerrero`. Names that merely contain such words are left alone:
/// `Rio de Janeiro`, `Bay of Plenty`, `Capital Region`.
fn without_kind(region: &str, also_city_words: bool) -> &str {
    let mut rest = region.trim();
    let mut found_kind = false;
    loop {
        let (word, tail) = first_word(rest);
        if tail.is_empty() {
            break;
        }
        let word = fold(word);
        let word = word.as_str();
        let is_kind = KINDS.contains(&word) || (also_city_words && CITY_WORDS.contains(&word));
        if !is_kind && !QUALIFIERS.contains(&word) {
            break;
        }
        found_kind |= is_kind;
        rest = tail;
    }
    if !found_kind {
        return region.trim();
    }
    let (word, tail) = first_word(rest);
    if !tail.is_empty() && LINKS.contains(&fold(word).as_str()) {
        rest = tail;
    }
    rest
}

/// Whether the region is simply named after the place: `Delhi` in
/// `National Capital Territory of Delhi`, `Lagos` in `Lagos State`.
pub(super) fn named_after(region: &str, place_name: &str) -> bool {
    let name = words(place_name);
    let mut core = words(without_kind(region, true));
    if core == name {
        return true;
    }
    if core.len() > 1
        && core
            .last()
            .is_some_and(|last| TRAILING_KINDS.contains(&last.as_str()))
    {
        core.pop();
    }
    core == name
}

/// The region as it should follow a place name in a label, or `None` when
/// it would only repeat the name.
pub fn region_for_label<'a>(place_name: &str, region: &'a str) -> Option<&'a str> {
    let region = region.trim();
    if region.is_empty() || named_after(region, place_name) {
        return None;
    }
    Some(without_kind(region, false))
}

/// How strongly a kind of place claims its spot: towns and cities first,
/// then other populated places (sections, localities, historical sites),
/// then everything else such as administrative divisions.
fn tier(place: &Place) -> u8 {
    match (place.fclass.as_deref(), place.fcode.as_deref()) {
        (_, Some("PPL" | "PPLA" | "PPLA2" | "PPLA3" | "PPLA4" | "PPLA5" | "PPLC" | "PPLG")) => 0,
        (Some("P"), _) => 1,
        _ => 2,
    }
}

/// Whether one name contains the other as whole words: `Delhi` and
/// `Delhi Cantonment`, `New Delhi`, `National Capital Territory of Delhi`.
fn related(a: &[String], b: &[String]) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    !short.is_empty() && long.windows(short.len()).any(|window| window == short)
}

struct Member {
    coords: Coords,
    region: Option<String>,
    /// Index of the group's leading place in `names`.
    leader: usize,
    /// Whether this member is itself the leader, and a metropolis.
    metropolis: bool,
}

/// Removes results that overlap a more important one.
///
/// Results are grouped around the most important place: a result joins a
/// group when it lies within [`OVERLAP_KM`] of one of its members, in the
/// same country and region, and its name (or the name the query matched)
/// and the leader's contain one another. Around a leader of a million
/// people or more, lesser places (not towns or cities) join from up to
/// [`METROPOLIS_KM`]. Only the leader of each group is kept, except that a
/// national capital is always kept. Importance is by kind of place (see [`tier`]),
/// then population. The order of the results is not changed.
pub fn without_overlaps(matches: Vec<Match>) -> Vec<Match> {
    if matches.len() < 2 {
        return matches;
    }
    let names: Vec<Vec<String>> = matches.iter().map(|m| words(&m.place.name)).collect();
    // The name the query matched, where that is not the place's main name:
    // a district of Berlin found through its other name "Berlin-Mitte".
    let matched: Vec<Vec<String>> = matches.iter().map(|m| words(&m.matched)).collect();
    let mut order: Vec<usize> = (0..matches.len()).collect();
    order.sort_by_key(|&i| {
        let place = &matches[i].place;
        (tier(place), std::cmp::Reverse(place.population), place.id)
    });

    let lat_reach = METROPOLIS_KM / 111.0;
    let mut by_country: HashMap<&str, Vec<Member>> = HashMap::new();
    let mut keep = vec![true; matches.len()];
    for &i in &order {
        let place = &matches[i].place;
        let coords = Coords {
            lat: place.lat,
            lng: place.lng,
        };
        let region = place.region.as_deref().filter(|r| !r.is_empty()).map(fold);
        let members = by_country
            .entry(place.cc.as_deref().unwrap_or(""))
            .or_default();
        // A "section" as large as Navi Mumbai is a city in its own right.
        let lesser = tier(place) > 0 && place.population < METROPOLIS;
        let is_town = tier(place) == 0;
        let leader = members
            .iter()
            .find(|member| {
                (member.coords.lat - coords.lat).abs() <= lat_reach
                    && (member.region.is_none() || region.is_none() || member.region == region)
                    // Two distinct towns stay separate however close they
                    // are. A town only gives way to a namesake, which is
                    // the same place listed twice, or to a metropolis that
                    // has absorbed it.
                    && (!is_town
                        || names[i] == names[member.leader]
                        || matches[member.leader].place.population >= METROPOLIS)
                    && (related(&names[i], &names[member.leader])
                        || related(&matched[i], &names[member.leader]))
                    && distance_km(member.coords, coords)
                        < if member.metropolis && lesser {
                            METROPOLIS_KM
                        } else {
                            OVERLAP_KM
                        }
            })
            .map(|member| member.leader);
        if leader.is_some() && place.fcode.as_deref() != Some("PPLC") {
            keep[i] = false;
        }
        members.push(Member {
            coords,
            region,
            leader: leader.unwrap_or(i),
            metropolis: leader.is_none() && place.population >= METROPOLIS,
        });
    }
    let mut keep = keep.into_iter();
    matches
        .into_iter()
        .filter(|_| keep.next().unwrap_or(true))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_lose_descriptions_of_their_kind() {
        for (region, short) in [
            ("State of Mahārāshtra", "Mahārāshtra"),
            ("Estado de Guerrero", "Guerrero"),
            ("National Capital Territory of Delhi", "Delhi"),
            ("Departamento del Meta", "Meta"),
            ("Changwat Phuket", "Phuket"),
            ("Provincia de La Rioja", "La Rioja"),
            ("Free and Hanseatic City of Hamburg", "Hamburg"),
            ("Free State", "Free State"),
            // Not descriptions of a kind of division.
            ("Rio de Janeiro", "Rio de Janeiro"),
            ("Bay of Plenty", "Bay of Plenty"),
            ("District of Columbia", "District of Columbia"),
            ("Ciudad de México", "Ciudad de México"),
            ("Capital Region", "Capital Region"),
            ("Lagos State", "Lagos State"),
            ("Scotland", "Scotland"),
        ] {
            assert_eq!(without_kind(region, false), short, "{region}");
        }
    }

    #[test]
    fn regions_named_after_the_place_are_left_out() {
        assert_eq!(
            region_for_label("Delhi", "National Capital Territory of Delhi"),
            None
        );
        assert_eq!(
            region_for_label("Hamburg", "Free and Hanseatic City of Hamburg"),
            None
        );
        assert_eq!(region_for_label("Lagos", "Lagos State"), None);
        assert_eq!(region_for_label("México", "Estado de México"), None);
        assert_eq!(region_for_label("Edinburgh", "Edinburgh"), None);
        assert_eq!(region_for_label("Edinburgh", ""), None);
        assert_eq!(
            region_for_label("New Delhi", "National Capital Territory of Delhi"),
            Some("Delhi")
        );
        assert_eq!(
            region_for_label("Mumbai", "State of Mahārāshtra"),
            Some("Mahārāshtra")
        );
        assert_eq!(
            region_for_label("Ikeja", "Lagos State"),
            Some("Lagos State")
        );
        // A different place whose name is only part of the region's.
        assert_eq!(region_for_label("York", "New York"), Some("New York"));
        assert_eq!(region_for_label("Edinburgh", "Scotland"), Some("Scotland"));
    }

    fn place(
        id: i64,
        name: &str,
        fcode: &str,
        population: i64,
        lat: f64,
        lng: f64,
        region: &str,
    ) -> Match {
        let fclass = if fcode.starts_with("PP") { "P" } else { "A" };
        Match {
            place: Place {
                id,
                name: name.into(),
                ascii_name: None,
                cc: Some(if region == "Ontario" { "CA" } else { "IN" }.into()),
                fclass: Some(fclass.into()),
                fcode: Some(fcode.into()),
                lat,
                lng,
                population,
                zone_name: None,
                admin_name: None,
                region: Some(region.into()),
                region_is_own_name: false,
                short_region: None,
            },
            matched: name.into(),
            score: 0.0,
        }
    }

    fn kept(matches: Vec<Match>) -> Vec<String> {
        without_overlaps(matches)
            .into_iter()
            .map(|m| m.place.name)
            .collect()
    }

    #[test]
    fn parts_of_a_city_give_way_to_it() {
        let nct = "National Capital Territory of Delhi";
        let names = kept(vec![
            place(1, "Central Delhi", "ADM2", 582_320, 28.64857, 77.21895, nct),
            place(2, "Delhi Cantonment", "PPL", 110_351, 28.6, 77.13333, nct),
            place(3, "Delhi", "PPLA", 11_034_555, 28.65195, 77.23149, nct),
            place(4, "New Delhi", "PPLC", 317_797, 28.62137, 77.2148, nct),
            place(5, "Delhi", "PPL", 4_000, 42.8501, -80.49968, "Ontario"),
            // Close by, but neither name contains the other.
            place(6, "Shahdara", "PPL", 300_000, 28.67, 77.29, nct),
            // Related by name but far away.
            place(7, "Delhi Road", "PPL", 900, 29.4, 77.3, nct),
        ]);
        // The cantonment is over 10 km from Delhi itself but joins the group
        // through New Delhi, which stays because it is the national capital.
        assert_eq!(
            names,
            ["Delhi", "New Delhi", "Delhi", "Shahdara", "Delhi Road"]
        );
    }

    #[test]
    fn distinct_towns_stay_separate_however_close() {
        // About 5 km apart, with related names, but two settlements.
        let names = kept(vec![
            place(1, "Brighton", "PPL", 140_000, 28.60, 77.20, "x"),
            place(2, "New Brighton", "PPL", 15_000, 28.64, 77.22, "x"),
            // The same town listed twice.
            place(3, "Brighton", "PPL", 300, 28.61, 77.21, "x"),
            // Not a settlement: a district of the first.
            place(4, "Brighton Centre", "PPLX", 9_000, 28.60, 77.21, "x"),
        ]);
        assert_eq!(names, ["Brighton", "New Brighton"]);
    }

    #[test]
    fn a_city_outranks_a_larger_division() {
        let names = kept(vec![
            place(1, "North West Delhi", "ADM2", 30_000_000, 28.66, 77.2, "x"),
            place(2, "Delhi", "PPL", 5_000, 28.65, 77.23, "x"),
        ]);
        assert_eq!(names, ["Delhi"]);
        // Different regions are never merged, however close.
        let names = kept(vec![
            place(1, "Kansas City", "PPL", 500_000, 39.1, -94.58, "Missouri"),
            place(2, "Kansas City", "PPL", 150_000, 39.11, -94.63, "Kansas"),
        ]);
        assert_eq!(names.len(), 2);
    }
}
