-- SQLite schema for GeoTimeZone. The GeoNames tables and the TimeZoneDB
-- tables are each refilled by their own import.

-- Flattened GeoNames features. Replaces the MariaDB `toponyms` table.
CREATE TABLE IF NOT EXISTS toponyms (
  pl_id      INTEGER PRIMARY KEY,  -- GeoNames id
  name       TEXT NOT NULL,
  ascii_name TEXT,
  cc         TEXT,                 -- ISO 3166-1 alpha-2, as published by GeoNames
  fclass     TEXT,                 -- GeoNames feature class (A, P, S, ...)
  fcode      TEXT,                 -- GeoNames feature code (PPL, ADM2, ...)
  lat        REAL NOT NULL,
  lng        REAL NOT NULL,
  population INTEGER NOT NULL DEFAULT 0,
  zone_name  TEXT,                 -- IANA time zone from the same GeoNames row
  date       TEXT,                 -- last modified, YYYY-MM-DD
  adm1       TEXT,
  adm2       TEXT,
  admin_name TEXT,                 -- second-level division name; NULL if adm2 is blank or unknown
  region     TEXT                  -- first-level division name; NULL if adm1 is unknown
);

-- Spatial index. Replaces the MariaDB `g geometry` column and its SPATIAL KEY.
-- Each place is stored as a zero-size box, so "within a box" queries are an
-- index lookup. Join on toponyms.pl_id = toponyms_rtree.id.
CREATE VIRTUAL TABLE IF NOT EXISTS toponyms_rtree USING rtree(
  id,
  min_lat, max_lat,
  min_lng, max_lng
);

-- Names a place can be searched by: its own name plus a curated subset of
-- GeoNames alternate names (official, regional, English and major languages,
-- Latin script only). A higher weight means a more expected form.
CREATE TABLE IF NOT EXISTS place_names (
  id     INTEGER PRIMARY KEY,           -- assigned in order of population, largest first
  pl_id  INTEGER NOT NULL,              -- toponyms.pl_id
  name   TEXT NOT NULL COLLATE NOCASE,
  langs  TEXT,                          -- space-separated ISO 639 codes the name is tagged with;
                                        -- NULL when it is only the place's own name
  weight INTEGER NOT NULL,              -- 100 own name ... 1 least expected
  UNIQUE (pl_id, name)
);

-- Prefix search over place_names.name, ignoring case and accents:
--   SELECT rowid FROM place_names_fts WHERE place_names_fts MATCH 'munch*'
-- rowid is place_names.id. Matches the start of any word in the name.
CREATE VIRTUAL TABLE IF NOT EXISTS place_names_fts USING fts5(
  name,
  content = 'place_names',
  content_rowid = 'id',
  tokenize = "unicode61 remove_diacritics 2",
  prefix = '2 3 4'
);

CREATE TABLE IF NOT EXISTS admin1_codes (
  cc         TEXT NOT NULL,
  adm1       TEXT NOT NULL,
  name       TEXT NOT NULL,
  ascii_name TEXT,
  geoname_id INTEGER,
  PRIMARY KEY (cc, adm1)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS admin2_codes (
  cc         TEXT NOT NULL,
  adm1       TEXT NOT NULL,
  adm2       TEXT NOT NULL,
  name       TEXT NOT NULL,
  ascii_name TEXT,
  geoname_id INTEGER,
  PRIMARY KEY (cc, adm1, adm2)
) WITHOUT ROWID;

-- Offset history from TimeZoneDB. The offset in force at time t is the row
-- with the greatest time_start <= t; the next row marks the end of the period.
-- The primary key serves exactly that lookup.
CREATE TABLE IF NOT EXISTS time_zones (
  zone_name    TEXT NOT NULL,     -- IANA name, e.g. Europe/London
  country_code TEXT NOT NULL,     -- ISO 3166-1 alpha-2; empty for Etc/UTC
  abbreviation TEXT NOT NULL,     -- e.g. BST
  time_start   INTEGER NOT NULL,  -- Unix seconds, UTC
  gmt_offset   INTEGER NOT NULL,  -- seconds east of UTC
  dst          INTEGER NOT NULL,  -- 1 during daylight saving time, else 0
  PRIMARY KEY (zone_name, time_start)
) WITHOUT ROWID;

-- Country names from TimeZoneDB, keyed like time_zones.country_code.
CREATE TABLE IF NOT EXISTS countries (
  country_code TEXT PRIMARY KEY,
  country_name TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS feature_codes (
  fcode       TEXT PRIMARY KEY,
  fclass      TEXT NOT NULL,
  name        TEXT,
  description TEXT
) WITHOUT ROWID;

-- One row per import attempt, newest last.
CREATE TABLE IF NOT EXISTS import_log (
  id          INTEGER PRIMARY KEY,
  data_set    TEXT NOT NULL,     -- 'geonames' or 'timezonedb'
  started_at  INTEGER NOT NULL,  -- Unix seconds
  finished_at INTEGER NOT NULL,
  status      TEXT NOT NULL,     -- 'imported', 'unchanged' or 'failed'
  records     INTEGER,           -- main table's row count after an import
  detail      TEXT               -- release, scope or error message
);

-- Current state: what the tables were built from, and the result of the
-- last check for newer source files.
CREATE TABLE IF NOT EXISTS import_meta (
  key   TEXT PRIMARY KEY,
  value TEXT
) WITHOUT ROWID;
