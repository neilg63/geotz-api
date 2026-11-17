//! The HTTP service. All data comes from the local SQLite database.

pub mod params;
pub mod places;
pub mod status;
pub mod time;

use std::path::Path;

use anyhow::Context;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use params::{Coords, Params};
use places::{Match, Place, Scope, SearchOptions};
use time::{TimeZone, When};

/// An error returned to the client as `{"valid": false, "error": "..."}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        ApiError { status: StatusCode::BAD_REQUEST, message: message.into() }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        ApiError { status: StatusCode::NOT_FOUND, message: message.into() }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        eprintln!("database error: {error}");
        ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: "database error".into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "valid": false, "error": self.message }))).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

fn to_json(value: impl Serialize) -> ApiResult {
    serde_json::to_value(value)
        .map(Json)
        .map_err(|_| ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: "cannot encode response".into() })
}

/// Opens the database read-only for serving.
pub async fn open_pool(db_path: &Path) -> anyhow::Result<SqlitePool> {
    anyhow::ensure!(
        db_path.is_file(),
        "{} does not exist; run `geotz-api import` first",
        db_path.display()
    );
    let options = SqliteConnectOptions::new().filename(db_path).read_only(true);
    SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .with_context(|| format!("cannot open {}", db_path.display()))
}

pub fn router(pool: SqlitePool) -> Router {
    Router::new()
        .route("/", get(welcome))
        .route("/timezone", get(timezone))
        .route("/geotz", get(geotz))
        .route("/geotime", get(geotime))
        .route("/nearby", get(nearby))
        .route("/lookup", get(lookup))
        .route("/localities", get(localities))
        .route("/search", get(search))
        .fallback(|| async { ApiError::not_found("route not found") })
        .with_state(pool)
}

pub async fn serve(pool: SqlitePool, host: &str, port: u16) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .with_context(|| format!("cannot listen on {host}:{port}"))?;
    println!("Listening on http://{}", listener.local_addr()?);
    axum::serve(listener, router(pool))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

// ------------------------------------------------------------ response shapes

/// One level of a place hierarchy, as returned by `/geotime` and `/search`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlaceRow {
    lat: f64,
    lng: f64,
    name: String,
    toponym: String,
    fcode: String,
    pop: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    country_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    admin_name: Option<String>,
}

impl PlaceRow {
    fn level(place: &Place, name: &str, toponym: &str, fcode: &str) -> Self {
        PlaceRow {
            lat: place.lat,
            lng: place.lng,
            name: name.to_owned(),
            toponym: toponym.to_owned(),
            fcode: fcode.to_owned(),
            pop: 0,
            country_code: None,
            admin_name: None,
        }
    }

    fn place(place: &Place) -> Self {
        PlaceRow {
            pop: place.population,
            ..PlaceRow::level(place, &place.name, &place.name, place.fcode.as_deref().unwrap_or(""))
        }
    }

    fn search_result(place: &Place) -> Self {
        PlaceRow {
            country_code: place.cc.clone(),
            admin_name: place.region.clone(),
            ..PlaceRow::place(place)
        }
    }
}

/// A single place with its context, as returned by `/nearby` and `/geotz`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NearbyPlace {
    lat: f64,
    lng: f64,
    name: String,
    toponym: String,
    fcode: String,
    /// Kilometres from the requested position.
    distance: f64,
    pop: i64,
    admin_name: String,
    region: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cc: Option<String>,
    country_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    zone_name: Option<String>,
}

async fn nearby_place(pool: &SqlitePool, place: &Place, distance: f64) -> Result<NearbyPlace, ApiError> {
    let country_name = match &place.cc {
        Some(cc) => places::country_name(pool, cc).await?.unwrap_or_else(|| cc.clone()),
        None => String::new(),
    };
    Ok(NearbyPlace {
        lat: place.lat,
        lng: place.lng,
        name: place.name.clone(),
        toponym: place.name.clone(),
        fcode: place.fcode.clone().unwrap_or_default(),
        distance: (distance * 1000.0).round() / 1000.0,
        pop: place.population,
        admin_name: place.admin_name.clone().unwrap_or_default(),
        region: place.region.clone().unwrap_or_default(),
        cc: place.cc.clone(),
        country_name,
        zone_name: place.zone_name.clone(),
    })
}

/// Autocomplete entry, as returned by `/lookup`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LookupItem {
    id: i64,
    lat: f64,
    lng: f64,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    zone_name: Option<String>,
}

/// Fuller entry, as returned by `/localities`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Locality {
    id: i64,
    name: String,
    ascii_name: String,
    admin_name: String,
    lat: f64,
    lng: f64,
    cc: String,
    population: i64,
    zone_name: String,
    /// The name that matched the query, when it is not `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    matched: Option<String>,
}

// ------------------------------------------------------------------ location

/// Where a time-zone request is about.
struct Located {
    /// Position used for solar time: the one requested, or the place found.
    coords: Coords,
    /// The place representing the position, with its distance in km.
    place: Option<(Place, f64)>,
}

fn search_options(params: &Params, default_max: usize) -> Result<SearchOptions, ApiError> {
    Ok(SearchOptions {
        countries: params.countries()?,
        region: params.region().map(str::to_owned),
        languages: params.languages()?,
        proximity: params.proximity()?,
        include_admin: false,
        max: params.max(default_max, 100)?,
    })
}

/// Resolves `loc`, or failing that a place name, to a position.
async fn locate(pool: &SqlitePool, params: &Params) -> Result<Located, ApiError> {
    if let Some(coords) = params.coords()? {
        return Ok(Located { coords, place: places::locate(pool, coords).await? });
    }
    let Some(query) = params.query() else {
        return Err(ApiError::bad_request(
            "give a position as loc=lat,lng or a place name as place=NAME (optionally with cc and reg)",
        ));
    };
    let options = SearchOptions { max: 1, ..search_options(params, 1)? };
    let Some(found) = places::search(pool, query, &options).await?.into_iter().next() else {
        return Err(ApiError::not_found(format!("no place found matching {query:?}")));
    };
    let coords = Coords { lat: found.place.lat, lng: found.place.lng };
    Ok(Located { coords, place: Some((found.place, 0.0)) })
}

/// The time zone at a located position: the place's zone if it has one,
/// otherwise a longitude-based zone for open sea.
async fn time_at(
    pool: &SqlitePool,
    located: &Located,
    when: When,
    first_occurrence: bool,
) -> Result<TimeZone, ApiError> {
    let lng = located.coords.lng;
    if let Some(zone) = located.place.as_ref().and_then(|(place, _)| place.zone_name.as_deref())
        && let Some(found) = time::zone_time(pool, zone, when, Some(lng), first_occurrence).await?
    {
        return Ok(found);
    }
    Ok(TimeZone::at_sea(located.coords.lat, lng, when))
}

// ------------------------------------------------------------------ handlers

/// `GET /`: which data sets are loaded, when, and how many records.
async fn welcome(State(pool): State<SqlitePool>) -> ApiResult {
    to_json(status::status(&pool).await?)
}

/// `GET /timezone`: the offset in force for a zone name, position or place.
async fn timezone(State(pool): State<SqlitePool>, Query(params): Query<Params>) -> ApiResult {
    let when = params.when()?;
    let first = params.first_occurrence()?;
    if let Some(zone) = params.zone() {
        return match time::zone_time(&pool, zone, when, None, first).await? {
            Some(found) => to_json(found),
            None => Err(ApiError::not_found(format!("unknown time zone {zone:?}"))),
        };
    }
    let located = locate(&pool, &params).await?;
    to_json(time_at(&pool, &located, when, first).await?)
}

/// `GET /geotz`: the nearest place as one object, with its time zone.
async fn geotz(State(pool): State<SqlitePool>, Query(params): Query<Params>) -> ApiResult {
    let when = params.when()?;
    let located = locate(&pool, &params).await?;
    let time = time_at(&pool, &located, when, params.first_occurrence()?).await?;
    let place = match &located.place {
        Some((place, distance)) => Some(nearby_place(&pool, place, *distance).await?),
        None => None,
    };
    let mut body = json!({ "time": time });
    if let Some(place) = place {
        body["place"] = serde_json::to_value(place).unwrap_or(Value::Null);
    }
    Ok(Json(body))
}

/// `GET /geotime`: the place as a hierarchy from country to locality, with
/// its time zone.
async fn geotime(State(pool): State<SqlitePool>, Query(params): Query<Params>) -> ApiResult {
    let when = params.when()?;
    let located = locate(&pool, &params).await?;
    let time = time_at(&pool, &located, when, params.first_occurrence()?).await?;
    let mut placenames = Vec::new();
    if let Some((place, _)) = &located.place {
        if let Some(cc) = &place.cc {
            let country = places::country_name(&pool, cc).await?.unwrap_or_else(|| cc.clone());
            placenames.push(PlaceRow::level(place, cc, &country, "PCLI"));
        }
        if let Some(region) = &place.region {
            placenames.push(PlaceRow::level(place, region, region, "ADM1"));
        }
        if let Some(admin) = place.admin_name.as_ref().filter(|a| Some(*a) != place.region.as_ref()) {
            let level = if place.region.is_some() { "ADM2" } else { "ADM1" };
            placenames.push(PlaceRow::level(place, admin, admin, level));
        }
        placenames.push(PlaceRow::place(place));
    }
    Ok(Json(json!({ "placenames": placenames, "time": time })))
}

/// `GET /nearby`: the closest town or village to a position.
async fn nearby(State(pool): State<SqlitePool>, Query(params): Query<Params>) -> ApiResult {
    let Some(coords) = params.coords()? else {
        return Err(ApiError::bad_request("loc is required, e.g. loc=55.95,-3.19"));
    };
    let radius = params.radius_km()?;
    match places::nearest_within(&pool, coords, radius, Scope::Populated).await? {
        Some((place, distance)) => to_json(nearby_place(&pool, &place, distance).await?),
        None => Err(ApiError::not_found(format!("no place within {radius:.0} km"))),
    }
}

async fn find(pool: &SqlitePool, params: &Params, default_max: usize, include_admin: bool) -> Result<Vec<Match>, ApiError> {
    let Some(query) = params.query() else { return Ok(Vec::new()) };
    let options = SearchOptions { include_admin, ..search_options(params, default_max)? };
    places::search(pool, query, &options).await
}

/// `GET /lookup`: compact autocomplete results.
async fn lookup(State(pool): State<SqlitePool>, Query(params): Query<Params>) -> ApiResult {
    let items: Vec<LookupItem> = find(&pool, &params, 20, false)
        .await?
        .into_iter()
        .map(|m| LookupItem {
            id: m.place.id,
            lat: m.place.lat,
            lng: m.place.lng,
            text: m.place.label(),
            zone_name: m.place.zone_name,
        })
        .collect();
    to_json(items)
}

/// `GET /localities`: autocomplete results with full place details.
async fn localities(State(pool): State<SqlitePool>, Query(params): Query<Params>) -> ApiResult {
    let items: Vec<Locality> = find(&pool, &params, 20, false)
        .await?
        .into_iter()
        .map(|m| {
            let place = m.place;
            Locality {
                id: place.id,
                matched: (m.matched != place.name).then_some(m.matched),
                ascii_name: place.ascii_name.unwrap_or_else(|| place.name.clone()),
                admin_name: place.region.or(place.admin_name).unwrap_or_default(),
                name: place.name,
                lat: place.lat,
                lng: place.lng,
                cc: place.cc.unwrap_or_default(),
                population: place.population,
                zone_name: place.zone_name.unwrap_or_default(),
            }
        })
        .collect();
    to_json(items)
}

/// `GET /search`: populated places and administrative divisions by name.
async fn search(State(pool): State<SqlitePool>, Query(params): Query<Params>) -> ApiResult {
    let searchable = params.query().is_some_and(|q| places::match_expression(q).is_some());
    let results: Vec<PlaceRow> = find(&pool, &params, 50, true)
        .await?
        .iter()
        .map(|m| PlaceRow::search_result(&m.place))
        .collect();
    let message = if searchable { "OK" } else { "Enter a place name of 2 or more letters via ?q=NAME" };
    Ok(Json(json!({ "count": results.len(), "message": message, "results": results })))
}
