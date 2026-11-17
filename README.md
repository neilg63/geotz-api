# geotz-api

[![mirror](https://img.shields.io/badge/mirror-github-blue)](https://github.com/neilg63/geotz-api)
[![crates.io](https://img.shields.io/crates/v/geotz-api.svg)](https://crates.io/crates/geotz-api)
[![docs.rs](https://docs.rs/geotz-api/badge.svg)](https://docs.rs/geotz-api)

A web service that matches place names and coordinates to time zones, and gives the UTC offset in force at any date and time, including historical offsets and daylight-saving changes. It answers from a local SQLite database built from [GeoNames](https://www.geonames.org) and [TimeZoneDB](https://timezonedb.com) data, with no calls to remote services at run time.

Built with axum and sqlx. One binary, `geotz-api`, holds both the web service and the tools that download the data and keep it current.

## Running the service

Import the data first (next section), then:

```bash
cargo run --release -- serve
```

It listens on `127.0.0.1:8089` by default; change that with `--host` and `--port` (see [Settings](#settings)).

JSON keys are snake_case (`zone_name`). `serve --camel-case`, or `CAMEL_CASE=true` (or `1`) in the environment or `.env`, switches every key to camelCase (`zoneName`). The database is opened read-only, and an `import` can run while the service is up.

The endpoints are documented in [docs/API.md](docs/API.md).

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
| `PORT` | `serve --port` | `8089` |
| `CAMEL_CASE` | `serve --camel-case` | off |

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

`check` exits with status 1 when a file is missing or has changed on the server, so a scheduled job can test it. It also records its finding in the database, which the service reports at `/` as `update_available`. No scraping is involved: for GeoNames it sends an HTTP `HEAD` request per file and reads the `Last-Modified` date; for TimeZoneDB, whose server sends no such date, it reads the date and checksum inside the zip.

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
| `--no-space-check` | `import`, `fetch`, `build` | Skip the free-disk-space check. |
| `--fcodes <FILE>` | `import`, `build` | Place feature codes to keep, whitespace-separated, `#` for comments. |
| `--languages <FILE>` | `import`, `build` | Languages that supply search names; see [Search names](#search-names). |
| `--keep-unpopulated` | `import`, `build` | Keep `P*` codes with no population too (about 5 million more rows). |
| `--rebuild` | `import` | Reload the tables even when nothing changed. |
| `--discard-sources` | `import`, `build` | Delete the downloaded source files once imported; see [Freeing disk space](#freeing-disk-space). |
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

`place_names` holds the names a place can be found by, each with a weight. GeoNames lists every spelling it knows for a place (94 for Munich) with no indication of importance; only a curated subset is kept.

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

`import --only timezones` loads two tables from the SQL dump TimeZoneDB publishes. The dump is parsed directly; no MySQL server is involved.

| Table | Rows | Contents |
| --- | --- | --- |
| `time_zones` | about 141,600 | One row per offset change per zone: `zone_name`, `country_code`, `abbreviation`, `time_start` (Unix seconds), `gmt_offset` (seconds), `dst` (0 or 1) |
| `countries` | 249 | `country_code`, `country_name` |

Differences from TimeZoneDB's own layout:

- The tables have plural names: its `time_zone` and `country` are stored as `time_zones` and `countries`.
- `time_zones` has a primary key on (`zone_name`, `time_start`), which is exactly the lookup the service makes.
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

## Disk space

Before downloading anything, `import` works out how much space the run needs and stops if the disk has less, so a server never ends up with a half-finished download filling it:

```
disk space needed: about 3.9 GiB (2.8 GiB while downloading and unpacking, 862.9 MiB while loading the database)
```

The figure is calculated for each run, not fixed:

- From the size of each file the server reports, with archives counted at 4.5 times their size once unpacked (the real files measure 3.8 to 4.25).
- Files are fetched one at a time, so an archive only overlaps with its own unpacked contents, and a file being replaced is only doubled while its replacement arrives. A routine update of files already present therefore needs far less than a first download.
- Loading needs room for the database plus the same again for SQLite's write-ahead log, which holds the new data until the import commits.
- 5% is added as a margin.

A first import of all countries needs about 3.9 GiB at its peak and settles at about 3 GiB (0.4 GiB with `--discard-sources`). When nothing needs downloading there is no check. `--no-space-check` skips it.

## Freeing disk space

The unpacked source files take about 2.6 GB for all countries, against roughly 400 MB for the database. The service never reads them, so on a server they can go:

```bash
cargo run --release -- import --discard-sources
```

This imports as usual (or confirms everything is current) and then deletes the source files, reporting the space freed. Only the two small `manifest.json` files remain, recording which versions were imported.

Afterwards:

- `import` and `check` still detect updates, by comparing the manifest with the publishers' servers. While nothing has changed, nothing is downloaded.
- When any GeoNames file changes, or a rebuild is needed for another reason (`--rebuild`, or a different feature-code or language list), all the GeoNames files are downloaded again, since a rebuild reads every one. That is about 600 MB for all countries.
- Without `--discard-sources` on that later run, the re-downloaded files stay. Put the option in the scheduled command to keep the disk clear.
- `--offline` and `build` need the files, and fail with a clear message if they have been discarded.

## Which rows are imported

A row is kept when both hold:

- Its feature code is in the list of 75 codes in [src/import/fcodes.txt](src/import/fcodes.txt): populated places, administrative divisions, airports and a few other kinds.
- If the code starts with `P`, its population is greater than zero.

The second rule keeps the database to about 1.2 million places. Without it the same 75 codes give 6.4 million rows, 4.7 million of them `PPL` entries with no population. `--keep-unpopulated` turns the rule off.

## How updates work

- Every import attempt is recorded in the `import_log` table (data set, start and finish times, `imported` / `unchanged` / `failed`, row count, and the error for a failure). The service shows the last ten at `/`.

- `toponyms.pl_id` is the GeoNames id, and the admin code tables carry `geoname_id` too, so rows keep their identity from one import to the next.
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

Where the division's own feature row was imported (matched by the `geoname_id` in the code file), its name is used instead: "Khuzestan Province", not the code file's "Khuzestan". The code-file name is the fallback.

`admin_name` is left NULL when a place has no `adm2` code, rather than guessing a district.

## Spatial index

SQLite has no geometry type, so nearest-place lookups use an R*Tree index, `toponyms_rtree`, built by the importer from each place's `lat` and `lng`. R*Tree is part of the SQLite that sqlx bundles; no extension is needed.

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

## Windows

The service is developed for and tested on Linux. It cross-compiles for Windows without changes, but has not been run there, so treat Windows as suitable for testing rather than production.

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

Points to be aware of:

- **Building natively** needs the Visual Studio C++ build tools, and the TLS library used for downloads may also ask for NASM.
- **Files open in another program** (an editor, antivirus scan or indexer) cannot be replaced or deleted on Windows, so an import or `--discard-sources` can stop with an error. Running it again recovers.
- **Keep the database on a local disk**, not a network share.
- **Scheduling:** use Task Scheduler to run `geotz-api import` where the examples here use cron.
- The first `serve` may raise a firewall prompt.

## Development

```bash
cargo test
```

## Licence

The code is licensed under either of the [MIT licence](LICENSE-MIT) or the [Apache Licence, Version 2.0](LICENSE-APACHE), at your option.

The data it imports has its own terms: GeoNames data is under Creative Commons Attribution 4.0, and TimeZoneDB data under Creative Commons Attribution 3.0. Both require attribution if you redistribute the data or a database built from it.
