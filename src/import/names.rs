//! Chooses which GeoNames alternate names become search names.
//!
//! GeoNames lists every known spelling of a place (over a hundred for a large
//! city). Search needs only the forms people actually type: the official
//! local name, a few regional ones, English and a handful of major languages.
//! Names are limited to Latin script; other alphabets are out of scope.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};

/// Built-in language choices; see the file for the format.
const DEFAULT_LANGUAGES: &str = include_str!("languages.conf");

/// Weight of a place's own name in `toponyms.name`.
pub const WEIGHT_MAIN: i64 = 100;
/// Weight of the ASCII form of the main name, when different.
pub const WEIGHT_ASCII: i64 = 95;
const WEIGHT_OFFICIAL: i64 = 90;
const WEIGHT_REGIONAL: i64 = 80;
const WEIGHT_ENGLISH: i64 = 70;
const WEIGHT_MAJOR: i64 = 50;
const BONUS_PREFERRED: i64 = 5;
const PENALTY_COLLOQUIAL: i64 = 10;
const PENALTY_HISTORIC: i64 = 30;

/// Which languages supply search names, and for which countries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Languages {
    /// Languages accepted for every place, at the lowest weight.
    major: HashSet<String>,
    /// Extra languages per country, e.g. `DE` -> `bar`.
    regional: HashMap<String, HashSet<String>>,
    /// Official languages per country, from GeoNames `countryInfo.txt`.
    official: HashMap<String, HashSet<String>>,
}

impl Languages {
    pub fn builtin() -> Self {
        Self::parse(DEFAULT_LANGUAGES).expect("built-in languages.conf is valid")
    }

    /// Parses lines of `major = fr es it` and `regional.DE = bar nds`.
    /// `#` starts a comment. Repeated keys add to earlier ones.
    pub fn parse(text: &str) -> Result<Self> {
        let mut languages = Languages::default();
        for (number, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .with_context(|| format!("line {}: expected `key = languages`", number + 1))?;
            let codes = value.split_whitespace().map(str::to_lowercase);
            match key.trim() {
                "major" => languages.major.extend(codes),
                key => match key.strip_prefix("regional.") {
                    Some(cc) if cc.len() == 2 => {
                        languages
                            .regional
                            .entry(cc.to_uppercase())
                            .or_default()
                            .extend(codes);
                    }
                    _ => bail!("line {}: unknown key `{key}`", number + 1),
                },
            }
        }
        Ok(languages)
    }

    /// Adds official languages from `countryInfo.txt` (column 16, such as
    /// `de-CH,fr-CH,it-CH,rm`). Region suffixes are dropped.
    pub fn add_country_info(&mut self, text: &str) {
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() < 16 || cols[0].len() != 2 {
                continue;
            }
            let codes = cols[15]
                .split(',')
                .map(base_language)
                .filter(|c| !c.is_empty());
            self.official
                .entry(cols[0].to_owned())
                .or_default()
                .extend(codes);
        }
    }

    /// Uses GeoNames `iso-languagecodes.txt` (ISO 639-3, 639-2, 639-1, name)
    /// to tidy the configured codes: a three-letter code that has a
    /// two-letter equivalent is replaced by it, since that is the form the
    /// alternate names use (`deu` -> `de`). Returns the codes the file does
    /// not know, which are probably typing mistakes.
    pub fn check_codes(&mut self, iso_codes: &str) -> Vec<String> {
        let mut short_form: HashMap<&str, &str> = HashMap::new();
        let mut known: HashSet<&str> = HashSet::new();
        for line in iso_codes.lines().skip(1) {
            let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
            if cols.len() < 3 {
                continue;
            }
            let (iso3, iso1) = (cols[0], cols[2]);
            for code in [iso3, iso1] {
                if !code.is_empty() {
                    known.insert(code);
                }
            }
            if !iso3.is_empty() && !iso1.is_empty() {
                short_form.insert(iso3, iso1);
            }
        }
        let mut unknown = Vec::new();
        let mut tidy = |set: &mut HashSet<String>| {
            *set = set
                .drain()
                .map(|code| match short_form.get(code.as_str()) {
                    Some(short) => (*short).to_owned(),
                    None => {
                        if !known.contains(code.as_str()) {
                            unknown.push(code.clone());
                        }
                        code
                    }
                })
                .collect();
        };
        tidy(&mut self.major);
        self.regional.values_mut().for_each(&mut tidy);
        unknown.sort_unstable();
        unknown.dedup();
        unknown
    }

    /// A stable description of the choices, stored with the import so a
    /// change of language settings triggers a rebuild.
    pub fn signature(&self) -> String {
        let sorted = |set: &HashSet<String>| {
            let mut codes: Vec<&str> = set.iter().map(String::as_str).collect();
            codes.sort_unstable();
            codes.join(" ")
        };
        let mut regional: Vec<String> = self
            .regional
            .iter()
            .map(|(cc, set)| format!("{cc}={}", sorted(set)))
            .collect();
        regional.sort_unstable();
        format!(
            "latin, by population; major={}; regional: {}",
            sorted(&self.major),
            regional.join(", ")
        )
    }

    /// Weight for a name in `lang` belonging to a place in country `cc`, or
    /// `None` if that language is not wanted there.
    fn base_weight(&self, cc: &str, lang: &str) -> Option<i64> {
        let listed = |map: &HashMap<String, HashSet<String>>| {
            map.get(cc).is_some_and(|set| set.contains(lang))
        };
        if listed(&self.official) {
            Some(WEIGHT_OFFICIAL)
        } else if listed(&self.regional) {
            Some(WEIGHT_REGIONAL)
        } else if lang == "en" {
            Some(WEIGHT_ENGLISH)
        } else if self.major.contains(lang) {
            Some(WEIGHT_MAJOR)
        } else {
            None
        }
    }

    /// Decides whether an alternate name is kept for a place in `cc`, and
    /// with what weight.
    pub fn weigh(&self, cc: &str, alt: &AltName<'_>) -> Option<i64> {
        if alt.lang.is_empty() || !is_latin(alt.name) {
            return None;
        }
        let mut weight = self.base_weight(cc, &base_language(alt.lang))?;
        if alt.preferred {
            weight += BONUS_PREFERRED;
        }
        if alt.colloquial {
            weight -= PENALTY_COLLOQUIAL;
        }
        if alt.historic {
            weight -= PENALTY_HISTORIC;
        }
        Some(weight.max(1))
    }
}

/// `de-CH` -> `de`. Codes with an underscore (`fr_1793`, revolutionary-era
/// French names) are GeoNames pseudo-languages and are left as they are.
pub fn base_language(code: &str) -> String {
    code.split('-').next().unwrap_or("").trim().to_lowercase()
}

/// True when every letter is from the Latin script (including accented
/// letters such as ü, ł, ş and Vietnamese forms) and there is at least one.
pub fn is_latin(name: &str) -> bool {
    let mut letters = 0;
    for c in name.chars().filter(|c| c.is_alphabetic()) {
        let code = c as u32;
        // Basic Latin to Latin Extended-B, and Latin Extended Additional.
        if !(code <= 0x024F || (0x1E00..=0x1EFF).contains(&code)) {
            return false;
        }
        letters += 1;
    }
    letters > 0
}

/// One row of `alternateNamesV2.txt`:
/// id, geonameid, language, name, preferred, short, colloquial, historic, from, to.
#[derive(Debug, PartialEq)]
pub struct AltName<'a> {
    pub geoname_id: i64,
    pub lang: &'a str,
    pub name: &'a str,
    pub preferred: bool,
    pub colloquial: bool,
    pub historic: bool,
}

impl<'a> AltName<'a> {
    pub fn parse(line: &'a str) -> Option<Self> {
        let mut cols = line.split('\t');
        cols.next()?;
        let geoname_id = cols.next()?.parse().ok()?;
        let lang = cols.next()?;
        let name = cols.next()?.trim();
        let mut flag = || cols.next() == Some("1");
        let preferred = flag();
        let _short = flag();
        let colloquial = flag();
        let historic = flag();
        if name.is_empty() {
            return None;
        }
        Some(AltName {
            geoname_id,
            lang,
            name,
            preferred,
            colloquial,
            historic,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn languages() -> Languages {
        let mut l = Languages::parse("major = fr it es\nregional.DE = bar\n").unwrap();
        l.add_country_info("#ISO\tcomment\nDE\tDEU\t276\tGM\tGermany\tBerlin\t357021\t82927922\tEU\t.de\tEUR\tEuro\t49\t#####\t^(\\d{5})$\tde\t2921044\tCH,PL\t\nCH\tCHE\t756\tSZ\tSwitzerland\tBern\t41290\t8516543\tEU\t.ch\tCHF\tFranc\t41\t####\t^(\\d{4})$\tde-CH,fr-CH,it-CH,rm\t2658434\tDE,IT\t\n");
        l
    }

    fn alt(lang: &'static str, name: &'static str) -> AltName<'static> {
        AltName {
            geoname_id: 1,
            lang,
            name,
            preferred: false,
            colloquial: false,
            historic: false,
        }
    }

    #[test]
    fn munich_keeps_only_the_forms_people_use() {
        let l = languages();
        assert_eq!(
            l.weigh(
                "DE",
                &AltName {
                    preferred: true,
                    ..alt("de", "München")
                }
            ),
            Some(95)
        );
        assert_eq!(l.weigh("DE", &alt("de", "Muenchen")), Some(90));
        assert_eq!(l.weigh("DE", &alt("bar", "Minga")), Some(80));
        assert_eq!(l.weigh("DE", &alt("en", "Munich")), Some(70));
        assert_eq!(l.weigh("DE", &alt("it", "Monaco di Baviera")), Some(50));
        assert_eq!(
            l.weigh("DE", &alt("pl", "Monachium")),
            None,
            "not a listed language"
        );
        assert_eq!(
            l.weigh("DE", &alt("ru", "Мюнхен")),
            None,
            "not Latin script"
        );
        assert_eq!(
            l.weigh("DE", &alt("", "Munchen")),
            None,
            "untagged names are dropped"
        );
        assert_eq!(l.weigh("DE", &alt("iata", "MUC")), None);
        assert_eq!(
            l.weigh("FR", &alt("fr_1793", "Port-la-Montagne")),
            None,
            "pseudo-language"
        );
    }

    #[test]
    fn regional_and_official_languages_apply_per_country() {
        let l = languages();
        assert_eq!(
            l.weigh("AT", &alt("bar", "Wean")),
            None,
            "bar is regional for DE only here"
        );
        assert_eq!(
            l.weigh("CH", &alt("fr", "Genève")),
            Some(90),
            "official beats major"
        );
        assert_eq!(
            l.weigh("CH", &alt("fr-CH", "Genève")),
            Some(90),
            "region suffix ignored"
        );
        assert_eq!(l.weigh("CH", &alt("rm", "Genevra")), Some(90));
    }

    #[test]
    fn flags_adjust_the_weight() {
        let l = languages();
        assert_eq!(
            l.weigh(
                "IN",
                &AltName {
                    historic: true,
                    ..alt("en", "Bombay")
                }
            ),
            Some(40)
        );
        assert_eq!(
            l.weigh(
                "DE",
                &AltName {
                    colloquial: true,
                    ..alt("en", "Munich")
                }
            ),
            Some(60)
        );
        assert_eq!(
            l.weigh(
                "DE",
                &AltName {
                    historic: true,
                    ..alt("it", "Monaco")
                }
            ),
            Some(20)
        );
    }

    #[test]
    fn latin_script_detection() {
        for name in [
            "München",
            "Łódź",
            "Hà Nội",
            "Saint-Étienne",
            "O'Fallon",
            "İstanbul",
            "Aix 2",
        ] {
            assert!(is_latin(name), "{name}");
        }
        for name in [
            "Мюнхен",
            "慕尼黑",
            "ميونخ",
            "Αθήνα",
            "München Мюнхен",
            "12345",
            "",
        ] {
            assert!(!is_latin(name), "{name}");
        }
    }

    #[test]
    fn alternate_name_rows_parse() {
        let row = AltName::parse("1562893\t2867714\tde\tMünchen\t1\t\t\t\t\t").unwrap();
        assert_eq!(
            row,
            AltName {
                geoname_id: 2867714,
                lang: "de",
                name: "München",
                preferred: true,
                colloquial: false,
                historic: false
            }
        );
        let historic = AltName::parse("9\t1275339\ten\tBombay\t\t\t\t1\t\t1995").unwrap();
        assert!(historic.historic && !historic.preferred);
        let short = AltName::parse("7\t5\tfr\tParis").unwrap();
        assert!(!short.preferred && !short.historic);
        assert!(AltName::parse("7\tnot-a-number\tfr\tParis").is_none());
    }

    #[test]
    fn language_code_file_tidies_and_validates() {
        let iso = "ISO 639-3\tISO 639-2\tISO 639-1\tLanguage Name\nbar\t\t\tBavarian\ndeu\tdeu / ger*\tde\tGerman\nfra\tfra / fre*\tfr\tFrench\n";
        let mut l = Languages::parse("major = deu fr xx\nregional.DE = bar brr\n").unwrap();
        assert_eq!(
            l.check_codes(iso),
            vec!["brr".to_string(), "xx".to_string()]
        );
        assert!(l.major.contains("de") && !l.major.contains("deu"));
        assert!(l.regional["DE"].contains("bar"));
    }

    #[test]
    fn config_errors_and_builtin() {
        assert!(Languages::parse("minor = fr").is_err());
        assert!(Languages::parse("regional.GER = bar").is_err());
        assert!(Languages::parse("just some words").is_err());
        let builtin = Languages::builtin();
        assert!(builtin.major.contains("fr") && !builtin.major.contains("ru"));
        assert!(builtin.regional["DE"].contains("bar"));
        assert_eq!(builtin.signature(), Languages::builtin().signature());
    }
}
