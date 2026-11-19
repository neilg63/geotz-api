# GeoTimeZone API (geotz-api)

The service answers from the local SQLite database only; it makes no calls to GeoNames or any other remote service.

```bash
cargo run --release -- serve --port 8089
```

## Conventions

- Every endpoint is `GET` and returns JSON with snake_case keys such as `zone_name` and `gmt_offset`. Starting the service with `--camel-case` (or `CAMEL_CASE=true`) switches every key to camelCase (`zoneName`, `gmtOffset`); values are never altered.
- Errors use one shape, `{"valid": false, "error": "..."}`, with a matching status:

| Status | When |
| --- | --- |
| 400 | A parameter is missing or cannot be read; the message names it |
| 404 | Unknown route, unknown zone name, or nothing matched a place lookup |
| 500 | Database failure |

- Name searches that find nothing return an empty list with status 200.
- Date-times in responses are `YYYY-MM-DDTHH:MM:SS`. Offsets are seconds east of UTC.

## Endpoints

| Path | Returns |
| --- | --- |
| `/` | Status: which data is loaded, when, and how much |
| `/timezone` | Time zone and offset for a zone name, position or place |
| `/geotz` | Nearest place (one object) with its time zone |
| `/geotime` | Place hierarchy (list) with its time zone |
| `/nearby` | Nearest town or village to a position |
| `/lookup` | Autocomplete matches, compact |
| `/localities` | Autocomplete matches, with place details |
| `/search` | Name search including administrative divisions |

## GET /

Reports the state of the data. `ready` is true when both data sets are loaded.

```json
{
  "message": "Welcome to GeoTimeZone",
  "version": "0.1.0",
  "ready": true,
  "sources": {
    "geonames": {
      "provides": "places, search names and their time-zone names",
      "imported": true,
      "imported_at": "2026-11-17T17:09:53Z",
      "records": { "places": 1192184, "search_names": 1608492 },
      "release": "2026-11-17",
      "scope": "allCountries",
      "checked_at": "2026-11-17T17:09:53Z",
      "update_available": false
    },
    "timezonedb": {
      "provides": "time-zone offset history and country names",
      "imported": true,
      "imported_at": "2026-11-17T17:10:45Z",
      "records": { "offset_periods": 141634, "zones": 419, "countries": 249 },
      "release": "2026-11-03",
      "checked_at": "2026-11-17T17:10:45Z",
      "update_available": false
    }
  },
  "recent_imports": [
    {
      "data_set": "geonames",
      "started_at": "2026-11-17T17:08:30Z",
      "finished_at": "2026-11-17T17:10:43Z",
      "status": "imported",
      "records": 1192184,
      "detail": "all countries"
    }
  ]
}
```

| Field | Meaning |
| --- | --- |
| `imported` | Whether the data set's tables hold any rows |
| `imported_at` | When the current data was loaded (UTC) |
| `records` | Row counts by table, as of that import |
| `release` | Date the publisher last modified the imported files |
| `checked_at`, `update_available` | When `geotz-api check` (or an import) last compared with the publisher, and whether newer files existed |
| `recent_imports` | The last ten import attempts, newest first; `status` is `imported`, `unchanged` or `failed`, and `detail` holds the error for a failure |

A data set that has never been imported shows only `provides` and `"imported": false`.

## Date and time parameters

Used by `/timezone`, `/geotz` and `/geotime`. The first one present is used.

| Param | Meaning |
| --- | --- |
| `dt` | UTC date-time |
| `dtl` | Local wall-clock date-time at the place |
| `jd` | Julian day (UTC) |
| `un` | Unix timestamp in seconds (UTC) |
| none | Now |

Date-times are ISO 8601, `YYYY-MM-DDTHH:MM:SS`, and may be incomplete. A space may replace the `T`, single digits need no leading zero, and a trailing `Z` or fraction of a second is ignored.

| Given | Read as |
| --- | --- |
| `2016-03-27T12:30` | `2016-03-27T12:30:00` |
| `2016-03-27T12` | `2016-03-27T12:00:00` |
| `2016-03-27T00` | `2016-03-27T00:00:00` |
| `2016-03-27` | `2016-03-27T12:00:00` |
| `2016-03` | `2016-03-01T12:00:00` |
| `2016` | `2016-01-01T12:00:00` |

- Missing minutes and seconds are zero; a missing month or day is the first.
- A date with no time is read as **noon**, not midnight. Clocks change in the small hours, so noon gives the offset in force for most of that day, even on the day summer time starts or ends. Give `T00` for midnight.

A UTC offset such as `+02:00` is not applied: `dt` is always UTC and `dtl` always local time at the place. Anything that is not a date in year-month-day order, or names an impossible date or time, is a 400 error.

### How `dtl` is resolved

`dtl` is converted to the UTC instant at which clocks in that zone showed that reading, and the offset returned is the one in force at that instant. For America/Chicago, where summer time began at 2010-03-14T08:00:00 UTC:

| `dtl` | UTC | Offset | Note |
| --- | --- | --- | --- |
| `2010-03-14T00:00:00` | 06:00:00 | −21600 (CST) | Before the change |
| `2010-03-14T01:59:59` | 07:59:59 | −21600 (CST) | Last second before |
| `2010-03-14T03:00:00` | 08:00:00 | −18000 (CDT) | First moment after |
| `2010-03-14T02:30:00` | 08:30:00 | −18000 (CDT) | Did not exist; read as 03:30 CDT |
| `2010-11-07T01:30:00` | 06:30:00 | −18000 (CDT) | Occurred twice; first by default |
| `2010-11-07T01:30:00` with `dst=0` | 07:30:00 | −21600 (CST) | Second occurrence |

- A reading skipped when clocks go forward is taken on the offset in force before the change, so it lands the same distance after the change.
- A reading repeated when clocks go back means the first occurrence unless `dst=0`. `dst` has no effect at any other time.
- `dtl` works the same way with `zone_name`, `loc` and `place`.

## Location parameters

Used by `/timezone`, `/geotz` and `/geotime`. The first that applies is used.

| Param | Meaning |
| --- | --- |
| `zone_name` | Zone name such as `Europe/London` (`/timezone` only). Also accepted as `zn`, `zoneName` or `zonename`. |
| `loc` | `lat,lng` in decimal degrees; spaces and a third value (altitude) are allowed |
| `place` or `q` | Place name, resolved with the same search as `/lookup`; `cc` and `reg` narrow it |

The four spellings of `zone_name` are interchangeable and always accepted: the `--camel-case` setting changes only the keys of the JSON output, never the names of query parameters.

A recognised `zone_name` is answered directly from the offset history: no place search is made, and `loc`, `place`, `cc` and `reg` are ignored. Letter case does not matter and a space is read as an underscore, so `europe/london` and `America/New York` are accepted. If the name is not recognised, `loc` or `place` is used when given; otherwise the response is a 404.

With `loc`, the zone is that of the nearest town or village, searched within 25 km, then 100 km, then 300 km; failing that, any feature with a time zone within 300 km. Beyond that the position is treated as open sea.

## Time zone object

Returned by `/timezone` and as `time` in `/geotz` and `/geotime`.

| Field | Meaning |
| --- | --- |
| `zone_name` | IANA name, or a sea-area name such as `North_Atlantic/03W` |
| `country_code` | ISO code of the zone's country; empty at sea |
| `abbreviation` | e.g. `CDT`; `SOL` or `LOC` when derived from longitude |
| `gmt_offset` | Seconds east of UTC at the reference time |
| `dst` | Whether summer time is in force |
| `utc`, `local_dt` | The reference time in UTC and on local clocks |
| `ref_unix`, `ref_jd` | The reference time as Unix seconds and Julian day |
| `week_day` | Local weekday: `iso` (Mon=1…Sun=7), `sun` (Sun=1…Sat=7), `abbr` |
| `period` | `start`/`start_utc` and `end`/`end_utc` of the current offset, and `next_utc_offset`; empty when derived from longitude |
| `solar_utc_offset` | Longitude × 240 seconds; present when a position is known |

Dates before a zone's first record:

- With a position: `SOL` (solar time to the second) before 1900, `LOC` (whole-hour zone by longitude) from 1900.
- With `zone_name` only: the zone's first record, local mean time, is extended backwards.

## GET /timezone

By zone name, with `dt`, `dtl`, `jd` or `un`, or none of them for the current time:

```
GET /timezone?zone_name=America/Chicago&dtl=2010-03-14T03:00:00
GET /timezone?zn=Europe/London
```

By position or place name:

```
GET /timezone?loc=43,-94&dtl=2010-03-14T03:00:00
```

The response has the same shape either way, except that `solar_utc_offset` is only present when a position is known:

```json
{
  "zone_name": "America/Chicago",
  "country_code": "US",
  "abbreviation": "CDT",
  "gmt_offset": -18000,
  "dst": true,
  "local_dt": "2010-03-14T03:00:00",
  "utc": "2010-03-14T08:00:00",
  "period": {
    "start": 1268553600,
    "start_utc": "2010-03-14T08:00:00",
    "next_utc_offset": -21600,
    "end": 1289113200,
    "end_utc": "2010-11-07T07:00:00"
  },
  "week_day": { "iso": 7, "sun": 1, "abbr": "Sun" },
  "ref_unix": 1268553600,
  "ref_jd": 2455269.8333333335,
  "solar_utc_offset": -22560
}
```

## GET /geotz

Same parameters. Returns `{"place": ..., "time": ...}`; `place` is omitted at sea.

`place`:

| Field | Meaning |
| --- | --- |
| `name`, `toponym` | Place name |
| `lat`, `lng` | Its coordinates |
| `distance` | km from the requested position; 0 when found by name |
| `fcode`, `pop` | GeoNames feature code and population |
| `admin_name`, `region` | Second- and first-level division names; empty if unknown |
| `cc`, `country_name` | ISO country code and name |
| `zone_name` | Zone name |

## GET /geotime

Same parameters. Returns `{"placenames": [...], "time": ...}`. `placenames` runs from country to locality and is empty at sea:

| `fcode` | `name` | `toponym` |
| --- | --- | --- |
| `PCLI` | Country code | Country name |
| `ADM1` | Region | Region |
| `ADM2` | Second-level division | Same |
| the place's own | Place name | Place name |

Each entry also has `lat`, `lng` (those of the place) and `pop` (0 above the place itself).

## GET /nearby

| Param | Default | Meaning |
| --- | --- | --- |
| `loc` | required | `lat,lng` |
| `radius` | 100 | Search radius in km, up to 500 |
| `fuzzy` | – | Tolerance in degrees; used if `radius` is absent |

Returns the `place` object above for the nearest town or village, or 404 if none is in range.

## Name search: /lookup, /localities, /search

All three run the same search and differ in what they return.

| Param | Kind | Meaning |
| --- | --- | --- |
| `q` (or `place`) | required | At least two letters. Each word must begin a word of the name; case and accents are ignored. |
| `cc` | filter | Only these countries: `IT`, or `IT,CH`. `UK` is read as `GB`. |
| `reg` | filter | Region or district name must begin with this |
| `lang` | preference | Favour names in these languages: `it`, `it:80`, `it:60,en:30` |
| `loc` | preference | Favour places near `lat,lng`: `44,12` or `44,12:25` |
| `max` | – | Results to return; default 20 (50 for `/search`), at most 100 |

Filters exclude; preferences only reorder. A preference takes a strength from 0 to 100 after a colon, 50 if omitted.

- `lang`: a name tagged with the language is scored as if it were the place's main name (fully from strength 50), with up to 50% added at strength 100.
- `loc`: multiplies the score by up to 1 + strength/100 at that exact position, fading over roughly 1,500 km.

Ranking combines how expected the matched name is for the place, the logarithm of its population, how much of the matched word has been typed, and whether the match is on the name's first word.

| Request | First result |
| --- | --- |
| `/lookup?q=Monac` | Monaco (MC) |
| `/lookup?q=Monac&lang=it` | Munich, via "Monaco di Baviera" |
| `/lookup?q=Venice&cc=IT` | Venice, Veneto only |
| `/lookup?q=Venice&loc=27.9,-82.5` | Venice, Florida |
| `/lookup?q=pechino` | Beijing |

### Overlapping results

All three endpoints leave out results that are parts or duplicates of a more important result nearby, so that `q=berlin` offers Berlin and not its districts; typing `spandau` still finds Spandau. A result is dropped when all of these hold:

- it lies within 10 km of a more important result, or of another result already grouped with it;
- both are in the same country and region;
- one name contains the other as whole words, as `Delhi` and `Delhi Cantonment` do. The name that the query matched counts too, so a district found through its other name "Berlin-Mitte" is treated as part of Berlin.

Importance is by kind of place first: towns and cities (`PPL`, `PPLA`–`PPLA5`, `PPLC`, `PPLG`), then other populated places such as sections of a city, then administrative divisions and anything else. Within a kind the larger population wins.

- Around a city of a million people or more, the distance is 25 km for anything that is not itself a town or city and has fewer than a million people, since districts of a large city lie well beyond 10 km from its centre.
- Two distinct towns are always listed separately, however close: a town gives way only to another entry of the same name (the same place listed twice) or to a city of a million or more that has absorbed it, as Delhi has Delhi Cantonment. Brighton and New Brighton, 5 km apart, both appear.
- A national capital (`PPLC`) is never dropped, so `q=delhi` returns both Delhi and New Delhi but not Delhi Cantonment or Central Delhi.
- Places that merely share the typed prefix (San Jose and Santa Clara) or lie in different regions (Kansas City, Missouri and Kansas City, Kansas) are not affected.

### /lookup

Populated places only. `text` is meant to fill an autocomplete box, so it is kept short: `Name, Region (CC)`.

| Place | Region as stored | `text` |
| --- | --- | --- |
| Delhi | National Capital Territory of Delhi | `Delhi (IN)` |
| New Delhi | National Capital Territory of Delhi | `New Delhi, Delhi (IN)` |
| Mumbai | State of Mahārāshtra | `Mumbai, Maharashtra (IN)` |
| Bandung | Jawa Barat | `Bandung, West Java (ID)` |
| Jakarta | Daerah Khusus Ibukota Jakarta | `Jakarta (ID)` |
| Mexico City | Ciudad de México | `Mexico City (MX)` |
| Moscow | Moskva | `Moscow (RU)` |
| Khimki | Moscow Oblast | `Khimki, Moscow Oblast (RU)` |
| Munich | Bavaria | `Munich, Bavaria (DE)` |

- The region shown is its conventional short name from the GeoNames region codes, which is usually the English one, in place of the formal local name.
- The region is left out altogether when it is named after the place itself. This is decided from the stored names: either the region is one of the place's names (Ciudad de México for Mexico City), or one of the region's names is the place's (Moskva is also known as Moscow). A region that is a separate entity, such as Bavaria or Lazio, shares no name with its city and is kept.
- Other fields are unchanged: `/localities` and `/search` still return the full region name in `admin_name`.

```json
[
  { "id": 2867714, "lat": 48.13743, "lng": 11.57549, "text": "Munich, Bavaria (DE)", "zone_name": "Europe/Berlin" }
]
```

### /localities

Populated places only. `matched` appears when the query matched an alternate name.

```json
[
  {
    "id": 1275339, "name": "Mumbai", "ascii_name": "Mumbai", "matched": "Bombay",
    "admin_name": "State of Mahārāshtra", "cc": "IN",
    "lat": 19.07283, "lng": 72.88261, "population": 12691836, "zone_name": "Asia/Kolkata"
  }
]
```

### /search

Populated places and administrative divisions.

```json
{
  "count": 1,
  "message": "OK",
  "results": [
    { "lat": 55.95206, "lng": -3.19648, "name": "Edinburgh", "toponym": "Edinburgh", "fcode": "PPLA", "pop": 514990, "country_code": "GB", "admin_name": "Scotland" }
  ]
}
```
