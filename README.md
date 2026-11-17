# geotz-api

Rust rewrite of the GeoTimeZone backend: axum, sqlx and SQLite. One binary, `geotz-api`, holds both the web service and the data tools.

## Running the service

Import the data first (next section), then:

```bash
cargo run --release -- serve
```

It listens on `127.0.0.1:8089` by default; change that with `--host` and `--port` (see [Settings](#settings)). The database is opened read-only, and an `import` can run while the service is up.

The endpoints are documented in [docs/API.md](docs/API.md). The behaviour of the service being replaced is documented in `../geotz/docs/API.md`.

## Settings

Each setting is taken from the first of these that provides it: the command-line option, the environment variable, a `.env` file, the default. The `.env` file is read from the directory the program is run in, or the nearest parent directory that has one. [sample.env](sample.env) lists every variable.

| Variable | Option | Default |
| --- | --- | --- |
| `DATA_PATH` | `--data-path` | `data` |
| `GEONAMES_BASE_DOWNLOAD_URL` | `--geonames-url` | `https://download.geonames.org/export/dump/` |
| `TIMEZONEDB_BASE_DOWNLOAD_URL` | `--timezonedb-url` | `https://timezonedb.com/files/` |
| `GEOTZ_DB` | `--db` | `<DATA_PATH>/geotz.db` |
| `GEOTZ_DATA_DIR` | `--data-dir` | `<DATA_PATH>/geonames` |
| `GEOTZ_TZ_DIR` | `--tz-dir` | `<DATA_PATH>/timezonedb` |
| `GEOTZ_HOST` | `serve --host` | `127.0.0.1` |
| `GEOTZ_PORT` | `serve --port` | `8089` |

The two download URLs are base addresses; file names are appended, so a mirror must keep the publishers' file names and layout.

## Importing data

```bash
cargo run --release -- import
```

The same command does the first import and every later update, for two independent data sets:

| Data set | `--only` value | Source | Tables |
| --- | --- | --- | --- |
| Places | `geonames` | [GeoNames dumps](https://download.geonames.org/export/dump/) | `toponyms`, `toponyms_rtree`, `place_names`, `place_names_fts`, `admin1_codes`, `admin2_codes`, `feature_codes` |
| Offset history | `timezones` | [TimeZoneDB](https://timezonedb.com/download) `TimeZoneDB.sql.zip` | `time_zones`, `countries` |

For each data set it:

1. Checks the source and downloads only what changed since the last run.
2. Unzips archives and discards them, keeping the unpacked files under `data/`.
3. Reloads that data set's tables in `data/geotz.db` inside one transaction.

Step 3 is skipped for a data set whose source and import settings are unchanged. `--rebuild` forces it.

To import one data set:

```bash
cargo run --release -- import --only timezones
```

To see whether an update is available without changing anything:

```bash
cargo run --release -- check
```

`check` exits with status 1 when a file is missing or has changed on the server, so a scheduled job can test it. It also records its finding in the database, which the service reports at `/` as `updateAvailable`. No scraping is involved: for GeoNames it sends an HTTP `HEAD` request per file and reads the `Last-Modified` date; for TimeZoneDB, whose server sends no such date, it reads the date and checksum inside the zip.

A daily cron entry is enough to keep the data current:

```
15 4 * * * cd /path/to/geotz-api && ./target/release/geotz-api import
``` GeoNames republishes its dumps daily; TimeZoneDB publishes when the IANA database changes.

A full place import reads 13.5 million place rows and 19 million alternate names, and writes about 1.2 million places and 1.6 million search names in roughly 45 seconds. The time-zone import takes about a second. The database is about 400 MB.

### Commands

| Command | What it does |
| --- | --- |
| `serve` | Run the web service |
| `import` | Fetch, then reload whatever changed. `--only` limits it to one data set. |
| `check` | Report which source files changed on the server. `--only` limits it to one data set. |
| `fetch` | Download and unpack GeoNames files only |
| `build` | Rebuild the place tables from GeoNames files already present |

TimeZoneDB data is downloaded and loaded only by `import` (with no `--only`, or with `--only timezones`). `check` reports whether a newer release exists. `fetch` and `build` are the two halves of the GeoNames import and do not touch it.

### Options

| Option | Applies to | Meaning |
| --- | --- | --- |
| `--only <geonames\|timezones>` | `import`, `check` | Work on one data set. Default: both. |
| `--data-path <DIR>` | all | Directory for the database and downloaded files. Default `data`. |
| `--data-dir <DIR>` | all | Where the GeoNames text files live. Default `<data path>/geonames`. |
| `--tz-dir <DIR>` | all | Where the TimeZoneDB SQL file lives. Default `<data path>/timezonedb`. |
| `--db <FILE>` | all | SQLite file. Default `<data path>/geotz.db`. |
| `--geonames-url <URL>` | `import`, `check`, `fetch` | Where GeoNames files are downloaded from. |
| `--timezonedb-url <URL>` | `import`, `check` | Where `TimeZoneDB.sql.zip` is downloaded from. |
| `--countries GB,IE` | all | Places only: use per-country files, not `allCountries`. Handy for quick tests. |
| `--force` | `import`, `fetch` | Download everything again. |
| `--offline` | all | Never contact the servers. |
| `--fcodes <FILE>` | `import`, `build` | Place feature codes to keep, whitespace-separated, `#` for comments. |
| `--languages <FILE>` | `import`, `build` | Languages that supply search names; see [Search names](#search-names). |
| `--keep-unpopulated` | `import`, `build` | Keep `P*` codes with no population too (about 5 million more rows). |
| `--rebuild` | `import` | Reload the tables even when nothing changed. |
| `--max-drop <PERCENT>` | `import`, `build` | Abort if a table's row count falls by more than this since the last import. Default 10; 100 disables. |

### GeoNames files downloaded

| File | Used for |
| --- | --- |
| `allCountries.zip` (or `XX.zip` per country) | Places |
| `admin1CodesASCII.txt` | `region` names |
| `admin2Codes.txt` | `admin_name` names |
| `featureCodes_en.txt` | `feature_codes` table |
| `alternateNamesV2.zip` (or `alternatenames/XX.zip` per country) | Search names, tagged by language |
| `countryInfo.txt` | Official languages of each country (not stored as a table) |
| `iso-languagecodes.txt` | Checking the language codes in the language list |

`data/geonames/manifest.json` records the ETag and date of each file last fetched; that is how unchanged files are skipped once the zips are gone.

## Search names

`place_names` holds the names a place can be found by, each with a weight. It replaces the old `alt_names` column, which listed every spelling GeoNames knows (94 for Munich) with no indication of language or importance.

Names come from GeoNames' language-tagged `alternateNamesV2.txt`. A name is kept only when it is in Latin script and its language is one of:

| Tier | Weight | Which languages |
| --- | --- | --- |
| The place's own name | 100 (95 for its ASCII form) | `toponyms.name` |
| Official | 90 | The country's languages in `countryInfo.txt` |
| Regional | 80 | Listed per country in the language list, e.g. `regional.DE = bar nds hsb` |
| English | 70 | |
| Major | 50 | `major = fr es it pt de nl` in the language list |

Adjustments: +5 if GeoNames flags the name as preferred, −10 if colloquial, −30 if historic (so "Bombay" still finds Mumbai). Untagged names, and tags such as airport codes, postcodes and links, are dropped. Names are numbered in order of population, largest places first, so that a search for a short prefix can read the first few thousand matches and stop.

The same spelling in several languages is stored once, with its highest weight and every language it is tagged with in `langs` (e.g. `München` → `de nds nl`).

Munich ends up with eight names: Munich, München, Muenchen, Minga, Mnichow, Monaco di Baviera, Munique, Múnich.

The built-in language list is [src/import/languages.conf](src/import/languages.conf). Copy it, edit it and pass `--languages FILE` to change the choices; the next `import` rebuilds. Codes GeoNames does not recognise are reported as a warning.

`place_names_fts` is a full-text index over the names for prefix search, ignoring case and accents and matching the start of any word:

```sql
SELECT t.pl_id, t.name, t.cc, max(n.weight) AS weight
FROM place_names_fts f
JOIN place_names n ON n.id = f.rowid
JOIN toponyms t ON t.pl_id = n.pl_id
WHERE place_names_fts MATCH 'munch*'
GROUP BY t.pl_id;
```

Typical prefix searches take one or two milliseconds; two-letter prefixes about 25.

### Language preference

`langs` lets a search lean towards the searcher's language without excluding anything: `lookup?q=Monac&lang=it`. A name tagged with the requested language is scored as if it were the place's primary name; every other name still matches at its normal weight:

| Query | No `lang` | `lang=it` |
| --- | --- | --- |
| `Monac` | Monaco (MC) | Munich, via "Monaco di Baviera"; Monaco (MC) second |
| `Colon` | Colón (PA) | Colón (PA), then Köln via "Colonia" |
| `Munch` | Munich | Munich (the local name still works) |

Known limits:

- Non-Latin scripts are out of scope. Where the local language is not written in Latin script, its romanised form is usually an untagged name in GeoNames and is therefore not kept ("Moskva" does not find Moscow).
- Regional languages apply to the whole country, not a region within it, so Munich also gets its Sorbian name.
- Official languages follow GeoNames' list, which is generous for some countries (German and Slovene for Italy, French for India).

## Time zone history

`import --only timezones` loads the two tables from TimeZoneDB's MySQL dump, parsed directly in Rust; no MySQL is involved.

| Table | Rows | Contents |
| --- | --- | --- |
| `time_zones` | about 141,600 | One row per offset change per zone: `zone_name`, `country_code`, `abbreviation`, `time_start` (Unix seconds), `gmt_offset` (seconds), `dst` (0 or 1) |
| `countries` | 249 | `country_code`, `country_name` |

Differences from the MariaDB tables:

- The tables have plural names: TimeZoneDB's `time_zone` and `country` are stored as `time_zones` and `countries`.
- `time_zones` has a primary key on (`zone_name`, `time_start`), which is exactly the lookup the service makes. The old table had three separate single-column indexes and no key.
- `dst` is an integer 0 or 1, not the character `'0'` or `'1'`.
- `countries` is keyed by `country_code`.

The offset in force at time `t`:

```sql
SELECT * FROM time_zones
WHERE zone_name = ?1 AND time_start <= ?2
ORDER BY time_start DESC LIMIT 1;
```

How updates are detected: the TimeZoneDB server sends no ETag or Last-Modified header, so a release is identified by the checksum of `time_zone.sql` recorded inside the zip. The zip is under 1 MB, so `check` and `import` both download it to compare; `check` writes nothing. `data/timezonedb/manifest.json` holds the checksum and release date of the local copy.

`countries` is the only country table; GeoNames' `countryInfo.txt` is not imported.

## Schema changes

The schema version is kept in SQLite's `user_version`. When a release changes a table's name or columns, the affected tables are dropped on first use and the next `import` refills them, so run `import` after upgrading.

## Which rows are imported

A row is kept when both hold:

- Its feature code is in the list ([src/import/fcodes.txt](src/import/fcodes.txt), the 75 codes found in the old MariaDB table).
- If the code starts with `P`, its population is greater than zero.

The second rule is not in the code list itself. It reproduces the old table: without it the same 75 codes give 6.4 million rows, 4.7 million of them `PPL` with no population. `--keep-unpopulated` turns the rule off.

## How updates work

- Every import attempt is recorded in the `import_log` table (data set, start and finish times, `imported` / `unchanged` / `failed`, row count, and the error for a failure). The service shows the last ten at `/`.

- `toponyms.pl_id` is the GeoNames id, as before. The admin and country tables carry `geoname_id` too.
- Each import empties and refills the place tables in a single transaction. The result is the same as updating by id: changed places are updated, new ones added, and places GeoNames has deleted disappear.
- The database runs in WAL mode, so the service keeps answering from the old data until the import commits. A failed import changes nothing. No maintenance mode is needed.
- An import that would cut the number of places, or of time-zone periods, by more than 10% is aborted and rolled back, in case a source file is truncated or broken. The comparison is only made against a previous import with the same settings; `--max-drop` changes the limit.
- Only one import can run at a time; a second one started meanwhile stops with "database is locked" and changes nothing.
- During an import the `-wal` file beside the database grows to about the size of the database, so allow twice the disk space.
- Each data set reloads only its own tables, so a place import never touches the time-zone history and vice versa.

## Region and admin names

| Column | Source | When NULL |
| --- | --- | --- |
| `region` | `admin1CodesASCII.txt`, keyed by country + `adm1` | No such code in the file |
| `admin_name` | `admin2Codes.txt`, keyed by country + `adm1` + `adm2` | `adm2` is blank, or no such code |

Where the division's own feature row was imported (matched by the `geoname_id` in the code file), its name is used instead, as in the old table: "Khuzestan Province", not the code file's "Khuzestan". The code-file name is the fallback.

The old table filled `admin_name` for blank `adm2` with an arbitrary district of the same region; this import leaves it NULL.

## The `g` geometry column

MariaDB stored a `POINT` in `g` with a `SPATIAL KEY`. SQLite has no geometry type, and the column is not carried over. It was redundant: it held the same two numbers as `lat` and `lng`, and the old queries filtered on `lat`/`lng` and never used the spatial index.

Its replacement is an R*Tree index, `toponyms_rtree`, built by the importer from `lat` and `lng`. R*Tree is part of the SQLite that sqlx bundles; no extension is needed.

Find candidates in a box, then compute exact distances in Rust:

```sql
SELECT t.*
FROM toponyms_rtree r
JOIN toponyms t ON t.pl_id = r.id
WHERE r.min_lat >= ?1 AND r.max_lat <= ?2
  AND r.min_lng >= ?3 AND r.max_lng <= ?4;
```

For a radius of `d` km around (`lat`, `lng`):

- Latitude span: `lat ± d / 111.2`.
- Longitude span: `lng ± d / (111.2 × cos(lat))`, so the box is as wide in km as it is tall.
- Sort the candidates by haversine distance and drop any beyond `d`; the box corners are further away than the radius.
- If the box crosses ±180° longitude, run two queries, one for each side.

A box query around Edinburgh returns in about 0.15 ms. R*Tree stores coordinates as 32-bit floats (accurate to about a metre), so read exact coordinates from `toponyms`.

## Development

```bash
cargo test
```

## Licence

The code is licensed under either of the [MIT licence](LICENSE-MIT) or the [Apache Licence, Version 2.0](LICENSE-APACHE), at your option.

The data it imports has its own terms: GeoNames data is under Creative Commons Attribution 4.0, and TimeZoneDB data under Creative Commons Attribution 3.0. Both require attribution if you redistribute the data or a database built from it.
