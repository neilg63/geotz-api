use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use geotz_api::import::{self, Config, timezonedb};

#[derive(Parser)]
#[command(version, about = "GeoTimeZone API and data tools")]
struct Cli {
    /// Directory for everything the app stores: the database and the
    /// downloaded source files
    #[arg(long, global = true, default_value = "data", env = "DATA_PATH")]
    data_path: PathBuf,

    /// Directory holding the unpacked GeoNames files
    /// [default: <DATA_PATH>/geonames]
    #[arg(long, global = true, env = "GEOTZ_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Directory holding the unpacked TimeZoneDB file
    /// [default: <DATA_PATH>/timezonedb]
    #[arg(long, global = true, env = "GEOTZ_TZ_DIR")]
    tz_dir: Option<PathBuf>,

    /// SQLite database file [default: <DATA_PATH>/geotz.db]
    #[arg(long, global = true, env = "GEOTZ_DB")]
    db: Option<PathBuf>,

    /// Where GeoNames files are downloaded from
    #[arg(long, global = true, value_name = "URL", default_value = import::GEONAMES_DUMP_URL,
          env = "GEONAMES_BASE_DOWNLOAD_URL")]
    geonames_url: String,

    /// Where TimeZoneDB.sql.zip is downloaded from
    #[arg(long, global = true, value_name = "URL", default_value = import::TIMEZONEDB_FILES_URL,
          env = "TIMEZONEDB_BASE_DOWNLOAD_URL")]
    timezonedb_url: String,

    #[command(subcommand)]
    command: Command,
}

/// A data set with its own source, tables and update cycle.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum DataSet {
    /// Places: toponyms, admin and feature codes (GeoNames)
    Geonames,
    /// Offset history: time_zones and countries (TimeZoneDB)
    Timezones,
}

#[derive(Subcommand)]
enum Command {
    /// Run the web service
    Serve {
        /// Address to listen on
        #[arg(long, default_value = "127.0.0.1", env = "GEOTZ_HOST")]
        host: String,
        /// Port to listen on
        #[arg(long, default_value_t = 8089, env = "GEOTZ_PORT")]
        port: u16,
    },
    /// Download changed GeoNames and TimeZoneDB files, then reload the
    /// tables they feed.
    /// Use for both the first import and later updates.
    Import {
        /// Import one data set only (default: both)
        #[arg(long, value_enum)]
        only: Option<DataSet>,
        #[command(flatten)]
        fetch: FetchArgs,
        #[command(flatten)]
        build: BuildArgs,
    },
    /// Report which source files have changed on the server, without
    /// changing anything. Exits with status 1 when an update is available.
    Check {
        /// Check one data set only (default: both)
        #[arg(long, value_enum)]
        only: Option<DataSet>,
        #[command(flatten)]
        fetch: FetchArgs,
    },
    /// Download and unpack changed GeoNames files only (TimeZoneDB data is
    /// fetched by `import`)
    Fetch {
        #[command(flatten)]
        fetch: FetchArgs,
    },
    /// Rebuild the place tables from GeoNames files already downloaded
    /// (TimeZoneDB tables are reloaded by `import --only timezones --rebuild`)
    Build {
        #[command(flatten)]
        fetch: FetchArgs,
        #[command(flatten)]
        build: BuildArgs,
    },
}

#[derive(Args, Default)]
struct FetchArgs {
    /// Import only these countries' places (comma-separated ISO codes,
    /// e.g. GB,IE) from their own files, not allCountries
    #[arg(long, value_delimiter = ',')]
    countries: Vec<String>,

    /// Download every file again even if unchanged
    #[arg(long)]
    force: bool,

    /// Never contact the servers; use the files already present
    #[arg(long, conflicts_with = "force")]
    offline: bool,
}

#[derive(Args)]
struct BuildArgs {
    /// File listing the place feature codes to import (default: the 75
    /// built-in codes)
    #[arg(long)]
    fcodes: Option<PathBuf>,

    /// File choosing the languages of search names (default: built in;
    /// see src/import/languages.conf for the format)
    #[arg(long)]
    languages: Option<PathBuf>,

    /// Also keep places whose feature code starts with P and that have no
    /// population (about 5 million more rows)
    #[arg(long)]
    keep_unpopulated: bool,

    /// Reload the tables even when no source file has changed
    #[arg(long)]
    rebuild: bool,

    /// Abort, leaving the database unchanged, if a table's row count falls
    /// by more than this percentage since the last import (100 disables)
    #[arg(long, value_name = "PERCENT", default_value_t = 10.0)]
    max_drop: f64,
}

impl Default for BuildArgs {
    fn default() -> Self {
        BuildArgs { fcodes: None, languages: None, keep_unpopulated: false, rebuild: false, max_drop: 10.0 }
    }
}

struct Paths {
    data_dir: PathBuf,
    tz_dir: PathBuf,
    db: PathBuf,
    geonames_url: String,
    timezonedb_url: String,
}

fn config(paths: Paths, fetch: FetchArgs, build: &BuildArgs) -> Result<Config> {
    let fcodes = match &build.fcodes {
        Some(path) => import::load_fcodes(path)?,
        None => import::default_fcodes(),
    };
    let languages = match &build.languages {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read language list {}", path.display()))?;
            import::names::Languages::parse(&text)
                .with_context(|| format!("invalid language list {}", path.display()))?
        }
        None => import::names::Languages::builtin(),
    };
    Ok(Config {
        languages,
        geonames_url: import::base_url(&paths.geonames_url),
        timezonedb_url: import::base_url(&paths.timezonedb_url),
        data_dir: paths.data_dir,
        tz_dir: paths.tz_dir,
        db_path: paths.db,
        countries: fetch.countries,
        fcodes,
        keep_unpopulated: build.keep_unpopulated,
        force: fetch.force,
        offline: fetch.offline,
        max_drop_percent: build.max_drop,
    })
}

fn wants(only: Option<DataSet>, set: DataSet) -> bool {
    only.is_none_or(|chosen| chosen == set)
}

/// Runs one data set's import and records the outcome in `import_log`.
/// `run` returns the row count and a short description, or `None` when the
/// source was unchanged.
async fn logged<F>(config: &Config, data_set: &str, run: F) -> Result<()>
where
    F: Future<Output = Result<Option<(u64, String)>>>,
{
    let started_at = import::db::unix_now();
    let outcome = run.await;
    let (status, records, detail) = match &outcome {
        Ok(Some((rows, detail))) => ("imported", Some(*rows), detail.clone()),
        Ok(None) => ("unchanged", None, String::new()),
        Err(error) => ("failed", None, format!("{error:#}")),
    };
    if let Err(error) = import::db::log_import(&config.db_path, data_set, started_at, status, records, &detail).await {
        eprintln!("warning: could not write the import log: {error:#}");
    }
    outcome.map(|_| ())
}

fn scope(config: &Config) -> String {
    if config.countries.is_empty() { "all countries".into() } else { config.countries.join(",").to_uppercase() }
}

async fn build_places(config: &Config) -> Result<Option<(u64, String)>> {
    println!("Loading places into {}", config.db_path.display());
    let s = import::db::build(config).await?;
    println!("Imported {} places from {} source rows", s.imported, s.lines);
    println!("  skipped: {} other feature codes, {} without population, {} malformed",
        s.other_fcode, s.unpopulated, s.malformed);
    println!("  without region: {}, without admin name: {}, without time zone: {}",
        s.no_region, s.no_admin_name, s.no_zone);
    println!("  region or admin names taken from feature rows: {}", s.renamed);
    println!("  lookup rows: {} first-level and {} second-level divisions", s.admin1, s.admin2);
    println!("  search names: {} ({} of {} alternate names kept)",
        s.search_names, s.alt_names_kept, s.alt_names_read);
    if !s.unknown_languages.is_empty() {
        println!("  warning: unknown language codes in the language list: {}",
            s.unknown_languages.join(" "));
    }
    Ok(Some((s.imported, scope(config))))
}

async fn import_places(config: &Config, rebuild: bool) -> Result<Option<(u64, String)>> {
    println!("GeoNames: fetching into {}", config.data_dir.display());
    let downloaded = import::fetch_all(config).await?;
    if downloaded == 0 && !rebuild && import::db::is_current(config).await? {
        println!("Places already up to date (--rebuild to reload anyway)");
        return Ok(None);
    }
    build_places(config).await
}

async fn import_time_zones(config: &Config, rebuild: bool) -> Result<Option<(u64, String)>> {
    println!("TimeZoneDB: fetching into {}", config.tz_dir.display());
    match timezonedb::import(config, rebuild).await? {
        timezonedb::Outcome::UpToDate => {
            println!("Time zones already up to date (--rebuild to reload anyway)");
            Ok(None)
        }
        timezonedb::Outcome::Imported(s) => {
            let release = s.release_date.unwrap_or_else(|| "undated".into());
            println!("Imported {} offset periods for {} zones and {} countries (release {release})",
                s.periods, s.zones, s.countries);
            Ok(Some((s.periods, format!("release {release}"))))
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Exit quietly when output is piped into a command that stops reading
    // (such as `head`), instead of panicking on the closed pipe.
    #[cfg(unix)]
    // SAFETY: restores the default SIGPIPE action before any other thread
    // or signal handler exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    // Settings may come from a .env file in the working directory. Real
    // environment variables win over it, and command-line options over both.
    match dotenvy::dotenv() {
        Ok(_) => {}
        Err(error) if error.not_found() => {}
        Err(error) => eprintln!("warning: ignoring .env: {error}"),
    }
    let cli = Cli::parse();
    let paths = Paths {
        data_dir: cli.data_dir.unwrap_or_else(|| cli.data_path.join("geonames")),
        tz_dir: cli.tz_dir.unwrap_or_else(|| cli.data_path.join("timezonedb")),
        db: cli.db.unwrap_or_else(|| cli.data_path.join("geotz.db")),
        geonames_url: cli.geonames_url,
        timezonedb_url: cli.timezonedb_url,
    };
    match cli.command {
        Command::Serve { host, port } => {
            let pool = geotz_api::api::open_pool(&paths.db).await?;
            geotz_api::api::serve(pool, &host, port).await
        }
        Command::Import { only, fetch, build } => {
            let config = config(paths, fetch, &build)?;
            // Time zones first: quick, and independent of the place tables.
            if wants(only, DataSet::Timezones) {
                logged(&config, "timezonedb", import_time_zones(&config, build.rebuild)).await?;
            }
            if wants(only, DataSet::Geonames) {
                logged(&config, "geonames", import_places(&config, build.rebuild)).await?;
            }
            Ok(())
        }
        Command::Check { only, fetch } => {
            let config = config(paths, fetch, &BuildArgs::default())?;
            let mut stale = 0;
            if wants(only, DataSet::Timezones) {
                println!("TimeZoneDB: checking {}", timezonedb::zip_url(&config));
                let changed = timezonedb::check(&config).await?;
                import::db::record_check(&config.db_path, "timezonedb", changed).await?;
                stale += changed as usize;
            }
            if wants(only, DataSet::Geonames) {
                println!("GeoNames: checking {}", config.geonames_url);
                let changed = import::check_all(&config).await?;
                import::db::record_check(&config.db_path, "geonames", changed > 0).await?;
                stale += changed;
            }
            if stale == 0 {
                println!("Up to date");
                Ok(())
            } else {
                println!("{stale} file(s) to update: run `geotz-api import`");
                std::process::exit(1);
            }
        }
        Command::Fetch { fetch } => {
            let config = config(paths, fetch, &BuildArgs::default())?;
            println!("GeoNames: fetching into {}", config.data_dir.display());
            import::fetch_all(&config).await.map(|_| ())
        }
        Command::Build { fetch, build } => {
            let config = config(paths, fetch, &build)?;
            logged(&config, "geonames", build_places(&config)).await
        }
    }
}
