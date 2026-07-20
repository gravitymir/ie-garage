use axum::extract::{DefaultBodyLimit, Json, Path, Query};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use std::sync::atomic::{AtomicU64, Ordering};
use axum::response::{Redirect, Response};
use axum::routing::{delete, get, post, put};
use axum::Router;
use base64::Engine;
use regex::Regex;
use reqwest::Client;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

// ---------- query types ----------

#[derive(serde::Deserialize)]
struct StepAQuery {
    plate: String,
}

#[derive(serde::Deserialize)]
struct StepBQuery {
    plate: String,
    token: String,
}

// ---------- base paths ----------

fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .and_then(|exe_dir| {
            let name = exe_dir.file_name().and_then(|n| n.to_str())?;
            if (name == "release" || name == "debug")
                && exe_dir
                    .parent()
                    .and_then(|p| p.file_name().and_then(|n| n.to_str()))
                    == Some("target")
            {
                exe_dir.join("..").join("..").canonicalize().ok()
            } else {
                Some(exe_dir)
            }
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

fn cars_dir() -> PathBuf {
    PathBuf::from("cars")
}
fn workers_dir() -> PathBuf {
    PathBuf::from("workers")
}
fn store_dir() -> PathBuf {
    PathBuf::from("store")
}
fn store_items_dir() -> PathBuf {
    store_dir().join("items")
}
fn store_divisions_file() -> PathBuf {
    store_dir().join("divisions.json")
}
fn store_division_photos_dir() -> PathBuf {
    store_dir().join("division-photos")
}
fn settings_dir() -> PathBuf {
    PathBuf::from("settings")
}
// Autofill rules live under settings/ to mirror the UI hierarchy (the
// AUT card is a Settings sub-page, so its data belongs alongside the
// other settings JSONs). The old top-level autofill/rules.json is
// migrated on first startup — see migrate_autofill_location().
fn autofill_rules_file() -> PathBuf {
    settings_dir().join("autofill.json")
}
fn settings_file() -> PathBuf {
    settings_dir().join("settings.json")
}
fn chat_dir() -> PathBuf {
    PathBuf::from("chat")
}
fn chat_messages_file() -> PathBuf {
    chat_dir().join("messages.json")
}
fn scanner_dir() -> PathBuf {
    PathBuf::from("scanner")
}
fn scanner_devices_file() -> PathBuf {
    scanner_dir().join("devices.json")
}
fn scanner_pending_tokens_file() -> PathBuf {
    scanner_dir().join("pending-tokens.json")
}
fn scanner_scans_file() -> PathBuf {
    scanner_dir().join("scans.json")
}
fn legacy_database_dir() -> PathBuf {
    PathBuf::from("database")
}

// ---------- sanitization ----------

fn sanitize_filename(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect();
    if cleaned.is_empty() {
        "untitled".to_string()
    } else {
        cleaned
    }
}

fn sanitize_plate(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if cleaned.is_empty() {
        "UNKNOWN".to_string()
    } else {
        cleaned.to_uppercase()
    }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

// ---------- main ----------

#[tokio::main]
async fn main() {
    let initial_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let base = base_dir();
    let mut public_dir = base.join("public");
    if !public_dir.is_dir() {
        public_dir = initial_cwd.join("public");
    }
    if public_dir.is_dir() {
        println!("Serving static files from: {}", public_dir.display());
    }
    let _ = std::env::set_current_dir(&base);

    // Ensure storage roots exist
    let _ = std::fs::create_dir_all(cars_dir());
    let _ = std::fs::create_dir_all(workers_dir());
    let _ = std::fs::create_dir_all(store_items_dir());
    let _ = std::fs::create_dir_all(store_division_photos_dir());
    // Seed empty divisions file if missing
    if !store_divisions_file().exists() {
        let _ = std::fs::write(store_divisions_file(), "[]");
    }
    let _ = std::fs::create_dir_all(settings_dir());
    let _ = std::fs::create_dir_all(chat_dir());
    if !chat_messages_file().exists() {
        let _ = std::fs::write(chat_messages_file(), "[]");
    }
    let _ = std::fs::create_dir_all(scanner_dir());
    for f in [
        scanner_devices_file(),
        scanner_pending_tokens_file(),
        scanner_scans_file(),
    ] {
        if !f.exists() {
            let _ = std::fs::write(f, "[]");
        }
    }
    // Move any legacy autofill/rules.json to settings/autofill.json.
    // Runs before we ever try to seed the file — otherwise we'd write an
    // empty "[]" over user's rules on the first startup after upgrading.
    migrate_autofill_location();
    if !autofill_rules_file().exists() {
        let _ = std::fs::write(autofill_rules_file(), "[]");
    }

    // One-time migration from flat database/*.json to cars/{plate}/jobs/*.json
    migrate_legacy_database();
    // Backfill fuel_type="unknown" on existing cars that don't have one.
    backfill_fuel_type();
    // Fold the retired Closed/Finished job status into Work done, rename
    // any leftover "closed" timeline events, and stamp work_done_ms on
    // legacy terminal jobs from the pre-migration file mtime. Runs every
    // startup — no-op once every job on disk already matches the new
    // model.
    migrate_job_statuses();
    // Warm up the per-car derived cache (engine_summary + displacement).
    // Listing endpoints will now read these from car.json instead of
    // walking the oil archive on every request.
    backfill_car_derived_cache();

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(|| async { Redirect::temporary("/index.html") }))
        // mpmoil proxy
        .route("/api/step-a", get(step_a))
        .route("/api/step-b", get(step_b))
        // cars
        .route("/api/cars", get(list_cars))
        .route("/api/cars/:plate", get(get_car).put(upsert_car).delete(delete_car))
        .route(
            "/api/cars/:plate/photo",
            get(get_car_photo)
                .post(upload_car_photo)
                .delete(delete_car_photo),
        )
        .route("/api/cars/:plate/photo/thumb", get(get_car_photo_thumb))
        // oil archive per car
        .route(
            "/api/cars/:plate/oil",
            get(list_car_oils).post(save_car_oil),
        )
        .route(
            "/api/cars/:plate/oil/:name",
            get(get_car_oil).delete(delete_car_oil),
        )
        .route(
            "/api/cars/:plate/oil-fetch-all",
            post(fetch_all_car_oils),
        )
        // jobs per car
        .route(
            "/api/cars/:plate/jobs",
            get(list_car_jobs).post(save_car_job),
        )
        .route(
            "/api/cars/:plate/jobs/:name",
            get(get_car_job).delete(delete_car_job),
        )
        // all jobs (global)
        .route("/api/jobs", get(list_all_jobs))
        // Cheap counter for the home page — walks cars/*/jobs and tallies
        // .json files. No car.json / worker / oil parsing, so it's O(dir
        // reads) instead of O(full-payload build). Return shape is
        // { "count": N } to keep it plainly distinct from list_all_jobs.
        .route("/api/jobs/count", get(count_all_jobs))
        // workers
        .route("/api/workers", get(list_workers).post(create_worker))
        .route(
            "/api/workers/:id",
            get(get_worker).put(update_worker).delete(delete_worker),
        )
        .route(
            "/api/workers/:id/photo",
            get(get_worker_photo)
                .post(upload_worker_photo)
                .delete(delete_worker_photo),
        )
        .route("/api/workers/:id/photo/thumb", get(get_worker_photo_thumb))
        // nominal authentication (worker name + password)
        .route("/api/auth/login", post(auth_login))
        // autofill rules
        .route(
            "/api/autofill/rules",
            get(list_autofill_rules).post(create_autofill_rule),
        )
        .route(
            "/api/autofill/rules/:id",
            get(get_autofill_rule)
                .put(update_autofill_rule)
                .delete(delete_autofill_rule),
        )
        // store divisions
        .route(
            "/api/store/divisions",
            get(list_divisions).post(create_division),
        )
        .route(
            "/api/store/divisions/:id",
            put(update_division).delete(delete_division),
        )
        .route(
            "/api/store/divisions/:id/photo",
            get(get_division_photo)
                .post(upload_division_photo)
                .delete(delete_division_photo),
        )
        // store items
        .route(
            "/api/store/items",
            get(list_store_items).post(create_store_item),
        )
        .route(
            "/api/store/items/:id",
            get(get_store_item)
                .put(update_store_item)
                .delete(delete_store_item),
        )
        .route(
            "/api/store/items/:id/images",
            post(upload_store_item_image),
        )
        .route(
            "/api/store/items/:id/images/:filename",
            delete(delete_store_item_image),
        )
        // Stocktake: bulk-apply barcodes-from-scanner file to current_count.
        .route("/api/stocktake", post(stocktake))
        // Shared workshop chat — one JSON on disk everyone reads from.
        .route(
            "/api/chat/feed",
            get(list_chat_feed).post(post_chat_message),
        )
        .route("/api/chat/event", post(post_chat_event))
        // Scanner: phone-app pairing + scan intake. One pairing token per QR,
        // consumed on POST /api/scanner/register in exchange for a per-device
        // bearer token that then authorizes POST /api/scanner/scan.
        .route("/api/scanner/pair", post(create_scanner_pair))
        .route("/api/scanner/pair/:token/qr", get(get_scanner_pair_qr))
        .route("/api/scanner/pair/:token/status", get(get_scanner_pair_status))
        .route("/api/scanner/register", post(register_scanner_device))
        .route("/api/scanner/devices", get(list_scanner_devices))
        .route("/api/scanner/devices/:id", delete(delete_scanner_device))
        .route("/api/scanner/scan", post(post_scanner_scan))
        .route("/api/scanner/scans", get(list_scanner_scans))
        // Settings: one shared JSON blob (branding, screensaver, print flags)
        // plus a separate logo file for anything that shows a company mark.
        .route("/api/settings", get(get_settings).put(update_settings))
        .route(
            "/api/settings/logo",
            get(get_settings_logo)
                .post(upload_settings_logo)
                .delete(delete_settings_logo),
        )
        // Browsers auto-request /favicon.ico with no HTML link tag needed.
        // Serve the SVG file for that path with an image/svg+xml MIME —
        // modern browsers (Chrome/Firefox/Safari/Edge) all accept an SVG
        // under the .ico URL and render it just fine.
        .route("/favicon.ico", get(serve_favicon))
        // serve car + store images
        .nest_service("/cars-files", ServeDir::new("cars"))
        .nest_service("/store-files", ServeDir::new("store"))
        .fallback_service(ServeDir::new(public_dir))
        .layer(DefaultBodyLimit::max(25 * 1024 * 1024))
        // Tell browsers to revalidate every response. This avoids the
        // "old chat.js / auth.js still loaded" trap after a UI change.
        // ETag / If-None-Match (set by ServeDir) keeps revalidation cheap.
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ))
        .layer(cors);

    // Port 3000 — matches the existing ASUS port-forward rule on the
    // shop's 4G-AC68U so the app is reachable from the outer LAN and
    // (later) from the internet without changing router config.
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("Failed to bind to 0.0.0.0:3000");

    println!("Server running at http://localhost:3000");
    axum::serve(listener, app).await.expect("server error");
}

// ---------- mpmoil proxy ----------

async fn step_a(Query(params): Query<StepAQuery>) -> Result<Response, (StatusCode, String)> {
    let plate = params.plate.trim();
    if plate.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "plate is required".to_string()));
    }

    let client = build_client()?;
    let csrf = fetch_csrf(&client).await?;
    let url = format!(
        "https://www.mpmoil.ie/products/recommendation?license_plate={}&_={}",
        urlencoding::encode(plate),
        now_millis()
    );

    proxy_json(&client, &url, &csrf).await
}

async fn step_b(Query(params): Query<StepBQuery>) -> Result<Response, (StatusCode, String)> {
    let plate = params.plate.trim();
    let token = params.token.trim();
    if plate.is_empty() || token.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "plate and token are required".to_string(),
        ));
    }

    let client = build_client()?;
    let csrf = fetch_csrf(&client).await?;
    let url = format!(
        "https://www.mpmoil.ie/products/recommendation/cars/brand/model/{}/{}?_={}",
        urlencoding::encode(token),
        urlencoding::encode(plate),
        now_millis()
    );

    proxy_json(&client, &url, &csrf).await
}

fn build_client() -> Result<Client, (StatusCode, String)> {
    reqwest::ClientBuilder::new()
        .cookie_store(true)
        .build()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn fetch_csrf(client: &Client) -> Result<String, (StatusCode, String)> {
    let html = client
        .get("https://www.mpmoil.ie/products/recommendation")
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?
        .text()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let re = Regex::new(r#"name="csrf-token" content="([^"]+)""#)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    re.captures(&html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .ok_or((StatusCode::BAD_GATEWAY, "csrf token not found".to_string()))
}

async fn proxy_json(
    client: &Client,
    url: &str,
    csrf: &str,
) -> Result<Response, (StatusCode, String)> {
    let res = client
        .get(url)
        .header("accept", "application/json, text/javascript, */*; q=0.01")
        .header("x-requested-with", "XMLHttpRequest")
        .header("x-csrf-token", csrf)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let status = res.status();
    let body = res
        .text()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(response)
}

async fn fetch_json(client: &Client, url: &str, csrf: &str) -> Result<Value, (StatusCode, String)> {
    let res = client
        .get(url)
        .header("accept", "application/json, text/javascript, */*; q=0.01")
        .header("x-requested-with", "XMLHttpRequest")
        .header("x-csrf-token", csrf)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let body = res
        .text()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    serde_json::from_str(&body).map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))
}

// ---------- cars ----------

#[derive(Serialize)]
struct CarSummary {
    plate: String,
    make: String,
    model: String,
    customer_name: String,
    customer_phone: String,
    fuel_type: String,
    job_count: usize,
    last_job_ms: u128,
    // Sort key for the cars list — the most recent job.json mtime under
    // this car (0 if the car has no jobs). Deliberately does NOT include
    // car.json mtime: editing the phone number or swapping the photo
    // shouldn't fling a car to the top of the list. Only actual repair
    // activity (a job save, a work-done stamp) moves the car up.
    photo: Option<String>,
    photo_updated_ms: u128,
    // Status of the most recently-touched job that isn't fully closed —
    // one of "open" / "paused" / "work_done" (or None if every job on
    // this car is closed / there are no jobs). Drives the little inline
    // badge on /cars.html so a mechanic sees at a glance which cars
    // still have outstanding work.
    open_status: Option<String>,
}

async fn list_cars() -> Result<Json<Value>, (StatusCode, String)> {
    let dir = cars_dir();
    if !dir.exists() {
        return Ok(Json(json!({ "items": [] })));
    }
    let mut items: Vec<CarSummary> = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let plate = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let car_json_path = path.join("car.json");
        let car_json_mtime_ms: u128 = std::fs::metadata(&car_json_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let car_json: Value = std::fs::read_to_string(&car_json_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        let make = car_json
            .get("make")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let model = car_json
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let customer_name = car_json
            .get("customer_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let customer_phone = car_json
            .get("customer_phone")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let fuel_type = car_json
            .get("fuel_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let photo = car_json
            .get("photo")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let photo_updated_ms = car_json
            .get("photo_updated_ms")
            .and_then(|v| v.as_u64())
            .map(|n| n as u128)
            .unwrap_or(0);

        let jobs_dir = path.join("jobs");
        let mut job_count = 0usize;
        let mut last_job_ms: u128 = 0;
        // Track the most recently-modified non-closed job so we can surface
        // its status on the list row. Peeking inside every job JSON is a
        // little more work than the old metadata-only scan, but for a
        // small-workshop dataset (dozens to a few hundred cars) it's fine.
        let mut open_status: Option<String> = None;
        let mut open_status_ms: u128 = 0;
        if jobs_dir.is_dir() {
            if let Ok(job_entries) = std::fs::read_dir(&jobs_dir) {
                for je in job_entries.flatten() {
                    let jp = je.path();
                    if jp.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    job_count += 1;
                    let ms = std::fs::metadata(&jp)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    if ms > last_job_ms {
                        last_job_ms = ms;
                    }
                    // Read status from the JSON. Legacy records without an
                    // explicit "status" field derive it from the old
                    // finished-bool the same way list_car_jobs does.
                    let job_json: Value = std::fs::read_to_string(&jp)
                        .ok()
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or(Value::Null);
                    let status = job_json
                        .get("status")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            if job_json
                                .get("finished")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                            {
                                "closed".to_string()
                            } else {
                                "open".to_string()
                            }
                        });
                    let s_lower = status.to_lowercase();
                    // Terminal states (work_done, plus legacy closed /
                    // finished from before the merge) don't warrant a
                    // badge — the car has no outstanding work. Only
                    // in-flight statuses (open, paused) surface.
                    let terminal = matches!(
                        s_lower.as_str(),
                        "work_done" | "closed" | "finished"
                    );
                    if !terminal && ms >= open_status_ms {
                        open_status_ms = ms;
                        open_status = Some(status);
                    }
                }
            }
        }

        // car_json_mtime_ms is intentionally NOT folded into the sort key
        // anymore. It's still read (kept for potential debug/UI use) but
        // photo swaps and phone-number edits no longer bump a car up.
        let _ = car_json_mtime_ms;
        items.push(CarSummary {
            plate,
            make,
            model,
            customer_name,
            customer_phone,
            fuel_type,
            job_count,
            last_job_ms,
            photo,
            photo_updated_ms,
            open_status,
        });
    }
    // Sort by most-recent repair activity. Cars with no jobs
    // (last_job_ms == 0) fall to the bottom, which is fine — they'll
    // move up naturally the moment the first job is created.
    items.sort_by(|a, b| b.last_job_ms.cmp(&a.last_job_ms));
    Ok(Json(json!({ "items": items })))
}

async fn get_car(Path(plate): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let path = cars_dir().join(&plate).join("car.json");
    let content = std::fs::read_to_string(&path)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let json: Value = serde_json::from_str(&content)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(json))
}

async fn upsert_car(
    Path(plate): Path<String>,
    Json(mut payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let dir = cars_dir().join(&plate);
    std::fs::create_dir_all(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Read any existing car.json so we can preserve fields the form may not send.
    let existing: Value = std::fs::read_to_string(dir.join("car.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);

    if let Some(obj) = payload.as_object_mut() {
        obj.insert("plate".to_string(), Value::String(plate.clone()));
        obj.insert(
            "updated_ms".to_string(),
            Value::Number(serde_json::Number::from(now_millis() as u64)),
        );
        // Preserve fields managed outside the form unless explicitly overridden.
        for k in ["photo", "photo_updated_ms", "created_ms", "mpmoil_variant"] {
            if !obj.contains_key(k) {
                if let Some(v) = existing.get(k) {
                    obj.insert(k.to_string(), v.clone());
                }
            }
        }
        // fuel_type: if the form didn't send it, keep the existing value; if
        // there's no existing value, default to "unknown".
        if !obj.contains_key("fuel_type") {
            let preserved = existing
                .get("fuel_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            obj.insert("fuel_type".to_string(), Value::String(preserved));
        }
    }
    let pretty = serde_json::to_string_pretty(&payload)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(dir.join("car.json"), pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // mpmoil_variant may have changed → displacement (derived from
    // variant name) may have changed too. Cheap to recompute.
    refresh_car_derived_cache(&dir);
    Ok(Json(json!({ "ok": true, "plate": plate })))
}

async fn delete_car(Path(plate): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let path = cars_dir().join(&plate);
    if path.exists() {
        std::fs::remove_dir_all(&path)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}

// ---------- car photo ----------

fn photo_content_type(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
}

fn remove_existing_car_photos(dir: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if name.starts_with("photo.") || name == "thumb.jpg" {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }
}

/// Resolve the full-size photo file for a car from its car.json.
fn car_photo_file(dir: &std::path::Path) -> Option<PathBuf> {
    let car_json: Value = std::fs::read_to_string(dir.join("car.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())?;
    let photo = car_json.get("photo").and_then(|v| v.as_str())?;
    let p = dir.join(sanitize_filename(photo));
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// Build a small, lightweight JPEG thumbnail (max 400px, quality 65) from image bytes.
fn generate_thumbnail(src_bytes: &[u8], dst: &std::path::Path) -> Result<(), String> {
    let img = image::load_from_memory(src_bytes).map_err(|e| e.to_string())?;
    let thumb = img.thumbnail(400, 400); // fits within 400x400, keeps aspect ratio
    let rgb = thumb.to_rgb8();
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 65);
        encoder
            .encode(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
            .map_err(|e| e.to_string())?;
    }
    std::fs::write(dst, buf.into_inner()).map_err(|e| e.to_string())
}

async fn get_car_photo(Path(plate): Path<String>) -> Result<Response, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let dir = cars_dir().join(&plate);
    let car_json: Value = std::fs::read_to_string(dir.join("car.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let photo = car_json
        .get("photo")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::NOT_FOUND, "no photo".to_string()))?
        .to_string();
    let safe = sanitize_filename(&photo);
    let path = dir.join(&safe);
    let bytes = std::fs::read(&path)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let ext = std::path::Path::new(&safe)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let ct = photo_content_type(ext);
    let mut response = Response::new(bytes.into());
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(ct),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    Ok(response)
}

async fn get_car_photo_thumb(Path(plate): Path<String>) -> Result<Response, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let dir = cars_dir().join(&plate);
    let thumb_path = dir.join("thumb.jpg");
    // Lazily build the thumbnail for photos uploaded before thumbnails existed.
    if !thumb_path.exists() {
        if let Some(full) = car_photo_file(&dir) {
            if let Ok(bytes) = std::fs::read(&full) {
                let _ = generate_thumbnail(&bytes, &thumb_path);
            }
        }
    }
    let bytes = std::fs::read(&thumb_path).map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let mut response = Response::new(bytes.into());
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(response)
}

async fn upload_car_photo(
    Path(plate): Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let dir = cars_dir().join(&plate);
    std::fs::create_dir_all(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let raw_name = payload
        .get("filename")
        .and_then(|v| v.as_str())
        .unwrap_or("photo.jpg");
    let data_b64 = payload
        .get("data_base64")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "data_base64 required".to_string()))?;

    let b64_clean = if let Some(idx) = data_b64.find("base64,") {
        &data_b64[idx + 7..]
    } else {
        data_b64
    };

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_clean.trim())
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let ext = std::path::Path::new(raw_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| matches!(s.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif"))
        .unwrap_or_else(|| "jpg".to_string());

    remove_existing_car_photos(&dir);

    let file_name = format!("photo.{}", ext);
    std::fs::write(dir.join(&file_name), &bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Generate a lightweight thumbnail for the cars list (best-effort).
    let _ = generate_thumbnail(&bytes, &dir.join("thumb.jpg"));

    let car_path = dir.join("car.json");
    let mut car_json: Value = std::fs::read_to_string(&car_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({ "plate": plate.clone() }));
    if let Some(obj) = car_json.as_object_mut() {
        obj.insert("plate".to_string(), Value::String(plate.clone()));
        obj.insert("photo".to_string(), Value::String(file_name.clone()));
        let now = now_millis() as u64;
        obj.insert(
            "photo_updated_ms".to_string(),
            Value::Number(serde_json::Number::from(now)),
        );
        obj.insert(
            "updated_ms".to_string(),
            Value::Number(serde_json::Number::from(now)),
        );
    }
    let pretty = serde_json::to_string_pretty(&car_json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(&car_path, pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({
        "ok": true,
        "filename": file_name,
        "photo_updated_ms": now_millis() as u64,
    })))
}

async fn delete_car_photo(
    Path(plate): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let dir = cars_dir().join(&plate);
    remove_existing_car_photos(&dir);
    let car_path = dir.join("car.json");
    if let Ok(content) = std::fs::read_to_string(&car_path) {
        if let Ok(mut v) = serde_json::from_str::<Value>(&content) {
            if let Some(obj) = v.as_object_mut() {
                obj.remove("photo");
                obj.remove("photo_updated_ms");
                obj.insert(
                    "updated_ms".to_string(),
                    Value::Number(serde_json::Number::from(now_millis() as u64)),
                );
                let _ = std::fs::write(
                    &car_path,
                    serde_json::to_string_pretty(&v).unwrap_or(content),
                );
            }
        }
    }
    Ok(Json(json!({ "ok": true })))
}

// ---------- oil archive ----------

#[derive(Serialize)]
struct OilItem {
    name: String,
    title: String,
    saved_ms: u128,
}

async fn list_car_oils(Path(plate): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let dir = cars_dir().join(&plate).join("oil");
    if !dir.exists() {
        return Ok(Json(json!({ "items": [] })));
    }
    let mut items: Vec<OilItem> = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .flatten()
    {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown.json")
            .to_string();
        let content = std::fs::read_to_string(&p).unwrap_or_default();
        let parsed: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
        let title = parsed
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| name.trim_end_matches(".json").to_string());
        let saved_ms = std::fs::metadata(&p)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        items.push(OilItem {
            name,
            title,
            saved_ms,
        });
    }
    items.sort_by(|a, b| a.title.cmp(&b.title));
    Ok(Json(json!({ "items": items })))
}

async fn get_car_oil(
    Path((plate, name)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let name = sanitize_filename(&name);
    let file = if name.to_lowercase().ends_with(".json") {
        name
    } else {
        format!("{name}.json")
    };
    let path = cars_dir().join(&plate).join("oil").join(&file);
    let content =
        std::fs::read_to_string(&path).map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let json: Value = serde_json::from_str(&content)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(json))
}

async fn save_car_oil(
    Path(plate): Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let dir = cars_dir().join(&plate).join("oil");
    std::fs::create_dir_all(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let suggested = format!("oil-{}", now_millis());
    let file_name = payload
        .get("file_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or(suggested);
    let mut file_name = sanitize_filename(&file_name);
    if !file_name.to_lowercase().ends_with(".json") {
        file_name.push_str(".json");
    }
    let pretty = serde_json::to_string_pretty(&payload)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(dir.join(&file_name), pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    refresh_car_derived_cache(&cars_dir().join(&plate));
    Ok(Json(json!({ "ok": true, "file_name": file_name })))
}

async fn delete_car_oil(
    Path((plate, name)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let name = sanitize_filename(&name);
    let file = if name.to_lowercase().ends_with(".json") {
        name
    } else {
        format!("{name}.json")
    };
    let path = cars_dir().join(&plate).join("oil").join(&file);
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    refresh_car_derived_cache(&cars_dir().join(&plate));
    Ok(Json(json!({ "ok": true })))
}

/// Walk every mpmoil variant for this plate and save each oil recommendation as its own JSON.
async fn fetch_all_car_oils(
    Path(plate): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let dir = cars_dir().join(&plate).join("oil");
    std::fs::create_dir_all(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let client = build_client()?;
    let csrf = fetch_csrf(&client).await?;

    // step-a
    let step_a_url = format!(
        "https://www.mpmoil.ie/products/recommendation?license_plate={}&_={}",
        urlencoding::encode(&plate),
        now_millis()
    );
    let step_a_json = fetch_json(&client, &step_a_url, &csrf).await?;

    // Save raw step-a for reference
    let pretty = serde_json::to_string_pretty(&step_a_json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = std::fs::write(dir.join("_step-a.json"), pretty);

    // Find variants (an array somewhere in the response)
    let variants = extract_variants(&step_a_json);
    let mut saved = Vec::<Value>::new();
    let mut errors = Vec::<Value>::new();

    for variant in variants.iter() {
        let token = variant
            .get("slug_or_id")
            .or_else(|| variant.get("slug"))
            .or_else(|| variant.get("id"))
            .or_else(|| variant.get("code"))
            .and_then(|v| v.as_str().map(String::from).or_else(|| v.as_i64().map(|n| n.to_string())));
        let label = variant
            .get("name")
            .or_else(|| variant.get("title"))
            .or_else(|| variant.get("label"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| token.clone().unwrap_or_else(|| "variant".to_string()));

        let Some(token) = token else {
            errors.push(json!({ "variant": variant, "error": "no token" }));
            continue;
        };

        let step_b_url = format!(
            "https://www.mpmoil.ie/products/recommendation/cars/brand/model/{}/{}?_={}",
            urlencoding::encode(&token),
            urlencoding::encode(&plate),
            now_millis()
        );

        match fetch_json(&client, &step_b_url, &csrf).await {
            Ok(step_b_json) => {
                let snapshot = json!({
                    "title": label,
                    "token": token,
                    "plate": plate,
                    "fetched_ms": now_millis() as u64,
                    "variant": variant,
                    "data": step_b_json,
                });
                let mut file_name = sanitize_filename(&label);
                if file_name.is_empty() {
                    file_name = sanitize_filename(&token);
                }
                let file_name = format!("{}.json", file_name);
                let pretty = serde_json::to_string_pretty(&snapshot).unwrap_or_default();
                if std::fs::write(dir.join(&file_name), pretty).is_ok() {
                    saved.push(json!({ "file_name": file_name, "title": label }));
                }
            }
            Err((_, msg)) => {
                errors.push(json!({ "token": token, "title": label, "error": msg }));
            }
        }
    }

    // New oil archives just landed — engine_summary derived from them
    // is now stale. Recompute + write to car.json so /api/jobs stays
    // instant on the next load.
    let car_dir = cars_dir().join(&plate);
    refresh_car_derived_cache(&car_dir);

    Ok(Json(json!({
        "ok": true,
        "variants_found": variants.len(),
        "saved": saved,
        "errors": errors,
    })))
}

fn extract_variants(json: &Value) -> Vec<Value> {
    if let Some(arr) = json.as_array() {
        return arr.clone();
    }
    for key in ["items", "variants", "data", "results", "cars", "types"] {
        if let Some(arr) = json.get(key).and_then(|v| v.as_array()) {
            return arr.clone();
        }
    }
    if let Some(obj) = json.as_object() {
        for (_, v) in obj {
            if let Some(arr) = v.as_array() {
                if arr.iter().all(|item| item.is_object()) {
                    return arr.clone();
                }
            }
        }
    }
    Vec::new()
}

// ---------- jobs per car ----------

#[derive(Serialize)]
struct JobItem {
    name: String,
    saved_ms: u128,
    date_in: String,
    time_in: String,
    time_out: String,
    work_summary: String,
    // Odometer reading captured on this job — plain km string (whole
    // number in practice; the raw field is a free-form input so we
    // pass it through as text and let the frontend truncate). Rendered
    // between the job number and the description on car.html's job
    // list so past mileages are visible at a glance.
    kilometrage: String,
    // Status shown as a badge next to each row on car.html. Same vocabulary
    // as jobs.html: open / paused / work_done / closed. Legacy "finished"
    // records surface as "finished" here — the frontend maps them to
    // "closed" for display.
    status: String,
}

async fn list_car_jobs(Path(plate): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let dir = cars_dir().join(&plate).join("jobs");
    if !dir.exists() {
        return Ok(Json(json!({ "items": [] })));
    }
    let mut items: Vec<JobItem> = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .flatten()
    {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown.json")
            .to_string();
        let content = std::fs::read_to_string(&p).unwrap_or_default();
        let json: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
        let date_in = json
            .get("date_in")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let time_in = json
            .get("time_in")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let time_out = json
            .get("time_out")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let work_summary = json
            .get("work")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|it| it.get("description").and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default();
        let kilometrage = json
            .get("kilometrage")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let saved_ms = std::fs::metadata(&p)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let status = json
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                // Fallback for pre-status records: derive from the legacy
                // boolean flag.
                if json.get("finished").and_then(|v| v.as_bool()).unwrap_or(false) {
                    "finished".to_string()
                } else {
                    "open".to_string()
                }
            });
        items.push(JobItem {
            name,
            saved_ms,
            date_in,
            time_in,
            time_out,
            work_summary,
            kilometrage,
            status,
        });
    }
    items.sort_by(|a, b| b.saved_ms.cmp(&a.saved_ms));
    Ok(Json(json!({ "items": items })))
}

async fn get_car_job(
    Path((plate, name)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let name = sanitize_filename(&name);
    let file = if name.to_lowercase().ends_with(".json") {
        name
    } else {
        format!("{name}.json")
    };
    let path = cars_dir().join(&plate).join("jobs").join(&file);
    let content =
        std::fs::read_to_string(&path).map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let json: Value = serde_json::from_str(&content)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(json))
}

/// Next sequential job name for a car: "{plate}_{N}" where N is max existing + 1.
fn next_job_name(plate: &str) -> String {
    let dir = cars_dir().join(plate).join("jobs");
    let prefix = format!("{}_", plate);
    let mut max_n: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                if let Some(rest) = stem.strip_prefix(&prefix) {
                    if let Ok(n) = rest.parse::<u64>() {
                        if n > max_n {
                            max_n = n;
                        }
                    }
                }
            }
        }
    }
    format!("{}_{}", plate, max_n + 1)
}

async fn save_car_job(
    Path(plate): Path<String>,
    Json(mut payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let dir = cars_dir().join(&plate).join("jobs");
    std::fs::create_dir_all(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let suggested = next_job_name(&plate);
    let file_name = payload
        .get("file_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or(suggested);
    let mut file_name = sanitize_filename(&file_name);
    if !file_name.to_lowercase().ends_with(".json") {
        file_name.push_str(".json");
    }

    if let Some(obj) = payload.as_object_mut() {
        obj.insert("plate".to_string(), Value::String(plate.clone()));
        obj.insert("file_name".to_string(), Value::String(file_name.clone()));
    }

    let pretty = serde_json::to_string_pretty(&payload)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(dir.join(&file_name), pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Auto-create car.json from job data if missing
    let car_path = cars_dir().join(&plate).join("car.json");
    if !car_path.exists() {
        let make = payload
            .get("vehicle")
            .and_then(|v| v.get("make"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let model = payload
            .get("vehicle")
            .and_then(|v| v.get("model"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let reg_no = payload
            .get("vehicle")
            .and_then(|v| v.get("reg_no"))
            .and_then(|v| v.as_str())
            .unwrap_or(&plate);
        let customer_name = payload
            .get("customer_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let customer_phone = payload
            .get("customer_phone")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let car = json!({
            "plate": plate,
            "reg_no": reg_no,
            "make": make,
            "model": model,
            "customer_name": customer_name,
            "customer_phone": customer_phone,
            "created_ms": now_millis() as u64,
            "updated_ms": now_millis() as u64,
            "source": "auto-from-job",
        });
        let pretty = serde_json::to_string_pretty(&car).unwrap_or_default();
        let _ = std::fs::write(&car_path, pretty);
    }

    Ok(Json(json!({ "ok": true, "file_name": file_name, "plate": plate })))
}

async fn delete_car_job(
    Path((plate, name)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let plate = sanitize_plate(&plate);
    let name = sanitize_filename(&name);
    let file = if name.to_lowercase().ends_with(".json") {
        name
    } else {
        format!("{name}.json")
    };
    let path = cars_dir().join(&plate).join("jobs").join(&file);
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}

// ---------- all jobs (global view) ----------

#[derive(Serialize)]
struct GlobalJobItem {
    plate: String,
    name: String,
    make: String,
    model: String,
    customer_name: String,
    customer_phone: String,
    date_in: String,
    time_in: String,
    time_out: String,
    saved_ms: u128,
    last_active_ms: u128,
    worker_id: String,
    worker_name: String,
    status: String,
    fuel_type: String,
    displacement: String,    // "1.4", "2.0" – from variant name
    engine_summary: String,  // "CJCB · 136HP, 100KW, 4200RPM · 5 L"
    kilometrage: String,     // odometer at the time of this job, km as text
    // True when the car has a photo on disk — used by the hover-preview on
    // /jobs.html to know whether to try loading /api/cars/:plate/photo/thumb.
    has_photo: bool,
    photo_updated_ms: u128,  // cache-buster for the thumb URL
}

/// Build the compact engine summary string from a car's first oil archive.
/// Returns an empty string when there's no oil data yet.
fn build_engine_summary(car_dir: &std::path::Path) -> String {
    let oil_dir = car_dir.join("oil");
    if !oil_dir.is_dir() {
        return String::new();
    }
    let mut chosen: Option<std::path::PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(&oil_dir) {
        for e in entries.flatten() {
            let p = e.path();
            let name = match p.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if !name.ends_with(".json") || name.starts_with('_') {
                continue;
            }
            chosen = Some(p);
            break;
        }
    }
    let path = match chosen { Some(p) => p, None => return String::new() };
    let content = match std::fs::read_to_string(&path) { Ok(c) => c, Err(_) => return String::new() };
    let json: Value = match serde_json::from_str(&content) { Ok(v) => v, Err(_) => return String::new() };

    let engine = json
        .pointer("/data/type/components")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.iter().find(|c| c.get("name").and_then(|v| v.as_str()) == Some("Engine")));
    let engine = match engine { Some(e) => e, None => return String::new() };

    let code = engine.get("code").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let hp  = engine.get("hp").and_then(|v| v.as_u64()).map(|n| format!("{}HP", n)).unwrap_or_default();
    let kw  = engine.get("kw").and_then(|v| v.as_u64()).map(|n| format!("{}KW", n)).unwrap_or_default();
    let rpm = engine.get("rpm").and_then(|v| v.as_u64()).map(|n| format!("{}RPM", n)).unwrap_or_default();
    let cap = engine.pointer("/capacities/0/value").and_then(|v| v.as_str()).unwrap_or("");
    let cap_unit = engine.pointer("/capacities/0/unit").and_then(|v| v.as_str()).unwrap_or("");
    let cap_str = if cap.is_empty() {
        String::new()
    } else if cap_unit.starts_with("liter") || cap_unit == "L" {
        format!("{} L", cap)
    } else if cap_unit.is_empty() {
        cap.to_string()
    } else {
        format!("{} {}", cap, cap_unit)
    };

    let specs_combined = [hp, kw, rpm].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
    let pieces: Vec<String> = [code, specs_combined, cap_str].into_iter().filter(|s| !s.is_empty()).collect();
    pieces.join(" · ")
}

/// Extract engine displacement (e.g. "1.4", "2.0") from a variant name.
fn extract_displacement(name: &str) -> String {
    let s = name.trim();
    if s.is_empty() { return String::new(); }
    if let Some(re) = Regex::new(r"\b(\d{1,2}\.\d)\b").ok() {
        if let Some(m) = re.captures(s).and_then(|c| c.get(1)) {
            if let Ok(n) = m.as_str().parse::<f32>() {
                if (0.5..=10.0).contains(&n) {
                    return m.as_str().to_string();
                }
            }
        }
    }
    String::new()
}

/// Materialize derived per-car fields (`engine_summary`, `displacement`)
/// into car.json, so listing endpoints can read them straight from the
/// already-loaded car.json instead of walking the oil archive on every
/// request. Runs once at startup for every car (idempotent) and again
/// whenever the oil archive is (re)fetched.
///
/// Returns `(engine_summary, displacement)` from the fresh compute so the
/// caller can also use the values right away.
fn refresh_car_derived_cache(car_dir: &std::path::Path) -> (String, String) {
    let car_json_path = car_dir.join("car.json");
    let content = match std::fs::read_to_string(&car_json_path) {
        Ok(s) => s,
        Err(_) => return (String::new(), String::new()),
    };
    let mut json: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return (String::new(), String::new()),
    };

    let engine_summary = build_engine_summary(car_dir);
    let variant_name = json
        .pointer("/mpmoil_variant/name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let displacement = extract_displacement(&variant_name);

    // Only write back if something actually changed — keeps mtime stable
    // and the cars-list sort key (last_job_ms) unaffected.
    let cur_engine = json.get("engine_summary").and_then(|v| v.as_str()).unwrap_or("");
    let cur_disp   = json.get("displacement").and_then(|v| v.as_str()).unwrap_or("");
    if cur_engine != engine_summary || cur_disp != displacement {
        if let Some(obj) = json.as_object_mut() {
            obj.insert("engine_summary".to_string(), Value::String(engine_summary.clone()));
            obj.insert("displacement".to_string(),   Value::String(displacement.clone()));
            if let Ok(pretty) = serde_json::to_string_pretty(&json) {
                let _ = std::fs::write(&car_json_path, pretty);
            }
        }
    }

    (engine_summary, displacement)
}

/// Read a string field from car.json; if missing or empty, fall back
/// to the given compute closure (which is not called when the field
/// is present, so cache-hit is free).
fn json_str_or_else<F: FnOnce() -> String>(json: &Value, key: &str, compute: F) -> String {
    match json.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => compute(),
    }
}

/// One-shot startup pass: walks every car and materializes its derived
/// cache fields in car.json. Cars that already have up-to-date values
/// are cheap (no write). Cars that don't get one JSON parse + one write.
fn backfill_car_derived_cache() {
    let root = cars_dir();
    if !root.is_dir() { return; }
    let entries = match std::fs::read_dir(&root) { Ok(e) => e, Err(_) => return };
    let mut cnt = 0usize;
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() { continue; }
        refresh_car_derived_cache(&p);
        cnt += 1;
    }
    if cnt > 0 {
        eprintln!("Refreshed derived cache on {} car(s).", cnt);
    }
}

// Fast total-jobs count used by the home page tile. Skips reading any
// job JSON — the tile just needs the number, so we only ask the OS how
// many .json entries live under each cars/*/jobs directory. On a 140-
// car / 158-job dataset this returns in single-digit milliseconds
// versus multiple hundreds for the full list_all_jobs response.
async fn count_all_jobs() -> Json<Value> {
    let root = cars_dir();
    let mut total: u64 = 0;
    if let Ok(cars) = std::fs::read_dir(&root) {
        for c in cars.flatten() {
            let jobs = c.path().join("jobs");
            if let Ok(entries) = std::fs::read_dir(&jobs) {
                for e in entries.flatten() {
                    if e.path().extension().and_then(|s| s.to_str()) == Some("json") {
                        total += 1;
                    }
                }
            }
        }
    }
    Json(json!({ "count": total }))
}

async fn list_all_jobs() -> Result<Json<Value>, (StatusCode, String)> {
    let root = cars_dir();
    if !root.exists() {
        return Ok(Json(json!({ "items": [] })));
    }
    // Build worker_id -> "First Last" once so we don't re-read worker.json
    // per job in the loop below.
    let mut worker_names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Ok(wentries) = std::fs::read_dir(workers_dir()) {
        for we in wentries.flatten() {
            let wp = we.path();
            if !wp.is_dir() { continue; }
            let id = match wp.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if let Ok(content) = std::fs::read_to_string(wp.join("worker.json")) {
                if let Ok(j) = serde_json::from_str::<Value>(&content) {
                    let s = |k: &str| j.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                    let name = format!("{} {}", s("first_name"), s("last_name")).trim().to_string();
                    let display = if name.is_empty() { id.clone() } else { name };
                    worker_names.insert(id, display);
                }
            }
        }
    }
    let mut items: Vec<GlobalJobItem> = Vec::new();
    for car_entry in std::fs::read_dir(&root)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .flatten()
    {
        let car_path = car_entry.path();
        if !car_path.is_dir() {
            continue;
        }
        let plate = match car_path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let car_json: Value = std::fs::read_to_string(car_path.join("car.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        let make = car_json
            .get("make")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let model = car_json
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let customer_name = car_json
            .get("customer_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let customer_phone = car_json
            .get("customer_phone")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let fuel_type = car_json
            .get("fuel_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let variant_name = car_json
            .pointer("/mpmoil_variant/name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Prefer the pre-computed derived cache in car.json — populated
        // at startup by backfill_car_derived_cache() and refreshed after
        // every oil-fetch. Fallback to computing on the fly when the
        // field is missing (fresh car whose cache hasn't been built yet
        // — extremely rare, only until the next server restart).
        let engine_summary = json_str_or_else(&car_json, "engine_summary", || build_engine_summary(&car_path));
        let displacement   = json_str_or_else(&car_json, "displacement",   || extract_displacement(&variant_name));

        // Photo state per car (once) — the hover-preview on /jobs.html asks
        // for the thumb only when `has_photo` is true, so unfilled cars don't
        // trigger a 404 image request per hover.
        let has_photo = car_json
            .get("photo")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let photo_updated_ms = car_json
            .get("photo_updated_ms")
            .and_then(|v| v.as_u64())
            .map(|n| n as u128)
            .unwrap_or(0);

        let jobs_dir = car_path.join("jobs");
        if !jobs_dir.is_dir() {
            continue;
        }
        for je in std::fs::read_dir(&jobs_dir)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .flatten()
        {
            let p = je.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown.json")
                .to_string();
            let content = std::fs::read_to_string(&p).unwrap_or_default();
            let json: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
            let date_in = json
                .get("date_in")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let time_in = json
                .get("time_in")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let time_out = json
                .get("time_out")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let worker_id = json
                .get("worker_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let worker_name = if worker_id.is_empty() {
                String::new()
            } else {
                worker_names.get(&worker_id).cloned().unwrap_or_else(|| worker_id.clone())
            };
            let status = json
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    // Fallback: derive from `finished` flag for older records.
                    if json.get("finished").and_then(|v| v.as_bool()).unwrap_or(false) {
                        "finished".to_string()
                    } else {
                        "open".to_string()
                    }
                });
            let saved_ms = std::fs::metadata(&p)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis())
                .unwrap_or(0);
            // The file mtime is bumped on every save (incl. pause / resume /
            // finish), so it doubles as a "last activity" timestamp.
            let last_active_ms = saved_ms;
            let kilometrage = json
                .get("kilometrage")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            items.push(GlobalJobItem {
                plate: plate.clone(),
                name,
                make: make.clone(),
                model: model.clone(),
                customer_name: customer_name.clone(),
                customer_phone: customer_phone.clone(),
                date_in,
                time_in,
                time_out,
                saved_ms,
                last_active_ms,
                worker_id,
                worker_name,
                status,
                fuel_type: fuel_type.clone(),
                displacement: displacement.clone(),
                engine_summary: engine_summary.clone(),
                kilometrage,
                has_photo,
                photo_updated_ms,
            });
        }
    }
    items.sort_by(|a, b| b.last_active_ms.cmp(&a.last_active_ms));
    Ok(Json(json!({ "items": items })))
}

// ---------- parts catalog removed — superseded by /api/store/items ----------
// The parts catalog was a strict subset of Store (same fields, no stock
// counts / divisions / barcodes). Its endpoints and disk folder were
// removed; Store handles both "what this part is + photos" and inventory.

// ---------- workers ----------

fn worker_photo_file(dir: &std::path::Path) -> Option<PathBuf> {
    let j: Value = std::fs::read_to_string(dir.join("worker.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())?;
    let photo = j.get("photo").and_then(|v| v.as_str())?;
    let p = dir.join(sanitize_filename(photo));
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

fn read_worker_summary(dir: &std::path::Path) -> Option<Value> {
    let id = dir.file_name()?.to_str()?.to_string();
    let j: Value = std::fs::read_to_string(dir.join("worker.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())?;
    let s = |k: &str| j.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let n = |k: &str| j.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let has_password = j
        .get("password")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    Some(json!({
        "id": id,
        "first_name": s("first_name"),
        "last_name": s("last_name"),
        "patronymic": s("patronymic"),
        "phone": s("phone"),
        "photo": j.get("photo").and_then(|v| v.as_str()),
        "photo_updated_ms": n("photo_updated_ms"),
        "created_ms": n("created_ms"),
        "updated_ms": n("updated_ms"),
        "has_password": has_password,
    }))
}

async fn list_workers() -> Result<Json<Value>, (StatusCode, String)> {
    let dir = workers_dir();
    if !dir.exists() {
        return Ok(Json(json!({ "items": [] })));
    }
    let mut items: Vec<Value> = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .flatten()
    {
        let p = entry.path();
        if p.is_dir() {
            if let Some(s) = read_worker_summary(&p) {
                items.push(s);
            }
        }
    }
    items.sort_by(|a, b| {
        let am = a.get("created_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let bm = b.get("created_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        bm.cmp(&am)
    });
    Ok(Json(json!({ "items": items })))
}

async fn create_worker(
    Json(mut payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = format!("w-{}", now_millis());
    let dir = workers_dir().join(&id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("id".to_string(), Value::String(id.clone()));
        let now = now_millis() as u64;
        obj.insert("created_ms".to_string(), Value::Number(serde_json::Number::from(now)));
        obj.insert("updated_ms".to_string(), Value::Number(serde_json::Number::from(now)));
    }
    let pretty = serde_json::to_string_pretty(&payload)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(dir.join("worker.json"), pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true, "id": id })))
}

async fn get_worker(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let content = std::fs::read_to_string(workers_dir().join(&id).join("worker.json"))
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let json: Value = serde_json::from_str(&content)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    // Password is returned as-is. This is nominal-only auth (плайн-текст
    // by user request, see /api/auth/login) and the mechanic wants the
    // edit form to display what's currently stored, so they can see it
    // and re-type it on other devices without guessing.
    Ok(Json(json))
}

async fn update_worker(
    Path(id): Path<String>,
    Json(mut payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = workers_dir().join(&id);
    if !dir.exists() {
        return Err((StatusCode::NOT_FOUND, "worker not found".to_string()));
    }
    let existing: Value = std::fs::read_to_string(dir.join("worker.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let created_ms = existing
        .get("created_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| now_millis() as u64);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("id".to_string(), Value::String(id.clone()));
        obj.insert("created_ms".to_string(), Value::Number(serde_json::Number::from(created_ms)));
        obj.insert("updated_ms".to_string(), Value::Number(serde_json::Number::from(now_millis() as u64)));
        // Preserve photo fields (managed via the photo endpoint, not the form).
        if !obj.contains_key("photo") {
            if let Some(ph) = existing.get("photo") {
                obj.insert("photo".to_string(), ph.clone());
            }
        }
        if !obj.contains_key("photo_updated_ms") {
            if let Some(pu) = existing.get("photo_updated_ms") {
                obj.insert("photo_updated_ms".to_string(), pu.clone());
            }
        }
        // Password stored as-is (empty string included). The edit form
        // loads the current value into the input, so whatever comes back
        // in the PUT payload is the mechanic's explicit choice — no
        // "empty means keep existing" magic anymore.
        if !obj.contains_key("password") {
            obj.insert("password".to_string(), Value::String(String::new()));
        }
    }
    let pretty = serde_json::to_string_pretty(&payload)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(dir.join("worker.json"), pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true, "id": id })))
}

async fn delete_worker(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = workers_dir().join(&id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn get_worker_photo(Path(id): Path<String>) -> Result<Response, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = workers_dir().join(&id);
    let path = worker_photo_file(&dir).ok_or((StatusCode::NOT_FOUND, "no photo".to_string()))?;
    let bytes = std::fs::read(&path).map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let ct = photo_content_type(ext);
    let mut response = Response::new(bytes.into());
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(response)
}

async fn get_worker_photo_thumb(Path(id): Path<String>) -> Result<Response, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = workers_dir().join(&id);
    let thumb_path = dir.join("thumb.jpg");
    if !thumb_path.exists() {
        if let Some(full) = worker_photo_file(&dir) {
            if let Ok(bytes) = std::fs::read(&full) {
                let _ = generate_thumbnail(&bytes, &thumb_path);
            }
        }
    }
    let bytes = std::fs::read(&thumb_path).map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let mut response = Response::new(bytes.into());
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(response)
}

async fn upload_worker_photo(
    Path(id): Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = workers_dir().join(&id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let raw_name = payload.get("filename").and_then(|v| v.as_str()).unwrap_or("photo.jpg");
    let data_b64 = payload
        .get("data_base64")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "data_base64 required".to_string()))?;
    let b64_clean = if let Some(idx) = data_b64.find("base64,") {
        &data_b64[idx + 7..]
    } else {
        data_b64
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_clean.trim())
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let ext = std::path::Path::new(raw_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| matches!(s.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif"))
        .unwrap_or_else(|| "jpg".to_string());

    remove_existing_car_photos(&dir);
    let file_name = format!("photo.{}", ext);
    std::fs::write(dir.join(&file_name), &bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = generate_thumbnail(&bytes, &dir.join("thumb.jpg"));

    let wpath = dir.join("worker.json");
    let mut wj: Value = std::fs::read_to_string(&wpath)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({ "id": id.clone() }));
    if let Some(obj) = wj.as_object_mut() {
        obj.insert("id".to_string(), Value::String(id.clone()));
        obj.insert("photo".to_string(), Value::String(file_name.clone()));
        let now = now_millis() as u64;
        obj.insert("photo_updated_ms".to_string(), Value::Number(serde_json::Number::from(now)));
        obj.insert("updated_ms".to_string(), Value::Number(serde_json::Number::from(now)));
    }
    let pretty = serde_json::to_string_pretty(&wj)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(&wpath, pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({
        "ok": true,
        "filename": file_name,
        "photo_updated_ms": now_millis() as u64,
    })))
}

async fn delete_worker_photo(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = workers_dir().join(&id);
    remove_existing_car_photos(&dir);
    let wpath = dir.join("worker.json");
    if let Ok(content) = std::fs::read_to_string(&wpath) {
        if let Ok(mut v) = serde_json::from_str::<Value>(&content) {
            if let Some(obj) = v.as_object_mut() {
                obj.remove("photo");
                obj.remove("photo_updated_ms");
                obj.insert("updated_ms".to_string(), Value::Number(serde_json::Number::from(now_millis() as u64)));
                let _ = std::fs::write(&wpath, serde_json::to_string_pretty(&v).unwrap_or(content));
            }
        }
    }
    Ok(Json(json!({ "ok": true })))
}

// ---------- nominal authentication ----------
// Lightweight worker-name + password login. NOT for production.
// Passwords are stored as plain text in worker.json — by user request ("номинальная авторизация").
async fn auth_login(
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let worker_id = payload
        .get("worker_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let password = payload
        .get("password")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if worker_id.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "worker_id required".to_string()));
    }
    let id = sanitize_filename(&worker_id);
    let path = workers_dir().join(&id).join("worker.json");
    let content = std::fs::read_to_string(&path)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid credentials".to_string()))?;
    let json: Value = serde_json::from_str(&content)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid credentials".to_string()))?;
    let stored = json.get("password").and_then(|v| v.as_str()).unwrap_or("");
    if stored != password {
        return Err((StatusCode::UNAUTHORIZED, "invalid credentials".to_string()));
    }
    let s = |k: &str| json.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    Ok(Json(json!({
        "ok": true,
        "worker": {
            "id": id,
            "first_name": s("first_name"),
            "last_name": s("last_name"),
            "patronymic": s("patronymic"),
        }
    })))
}

// ---------- autofill rules ----------

fn read_autofill_rules() -> Vec<Value> {
    std::fs::read_to_string(autofill_rules_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default()
}

fn write_autofill_rules(list: &[Value]) -> Result<(), (StatusCode, String)> {
    let pretty = serde_json::to_string_pretty(list)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(autofill_rules_file(), pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(())
}

fn normalize_rule_payload(payload: &mut Value) {
    if let Some(obj) = payload.as_object_mut() {
        // Trim name/trigger
        for k in ["name", "trigger"] {
            if let Some(s) = obj.get(k).and_then(|v| v.as_str()).map(|s| s.trim().to_string()) {
                obj.insert(k.to_string(), Value::String(s));
            }
        }
        // applies_to: which fuel types this rule targets. Stored as an array
        // of strings; an empty array (or one containing "any") means the rule
        // applies to every car regardless of fuel type. For backward compat
        // a plain string is also accepted on input.
        let allowed: &[&str] = &["petrol", "diesel", "petrol-electric", "diesel-electric", "electric"];
        let raw = obj.remove("applies_to").unwrap_or(Value::Null);
        let mut values: Vec<String> = match raw {
            Value::String(s) => vec![s.trim().to_lowercase()],
            Value::Array(arr) => arr
                .into_iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_lowercase()))
                .collect(),
            _ => vec![],
        };
        // "any" or empty input both mean "apply to every car" — drop "any" and
        // any non-recognised values; if nothing remains, the empty array is
        // the canonical "applies to anything" marker.
        values.retain(|s| s != "any" && !s.is_empty() && allowed.iter().any(|x| *x == s.as_str()));
        // Deduplicate preserving order.
        let mut seen = std::collections::HashSet::new();
        values.retain(|s| seen.insert(s.clone()));
        let normalised: Vec<Value> = values.into_iter().map(Value::String).collect();
        obj.insert("applies_to".to_string(), Value::Array(normalised));
        // Ensure parts is an array of {used, number, quantity, unit}.
        // `number` stays in the schema for back-compat but is normally empty —
        // job.html now pulls it from the car's job history at autofill time.
        let parts: Vec<Value> = obj
            .get("parts")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|item| {
                        let used = item.get("used").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                        let number = item.get("number").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                        let quantity = item.get("quantity").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                        let unit = item.get("unit").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                        json!({ "used": used, "number": number, "quantity": quantity, "unit": unit })
                    })
                    .filter(|p| {
                        p.get("used").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();
        obj.insert("parts".to_string(), Value::Array(parts));

        // services: array of {desc} objects — labour lines with no parts
        // (tracking, road-test, brake-bleed). A bare string in the array
        // is treated as {desc: <string>} for forward compat.
        let services: Vec<Value> = obj
            .get("services")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let desc = match item {
                            Value::String(s) => s.trim().to_string(),
                            Value::Object(o) => o.get("desc").and_then(|v| v.as_str()).unwrap_or("").trim().to_string(),
                            _ => String::new(),
                        };
                        if desc.is_empty() { None } else { Some(json!({ "desc": desc })) }
                    })
                    .collect()
            })
            .unwrap_or_default();
        obj.insert("services".to_string(), Value::Array(services));

        // oils block:
        //   enabled            (bool)         — append all MPM-recommended engine oils on autofill
        //   older_than_years   (number|null)  — extra rule only if the car is at least N years old
        //   older_specific     (string)       — text of the extra oil row to append in that case
        let raw_oils = obj.remove("oils").unwrap_or(Value::Null);
        let oils_obj = raw_oils.as_object();
        let enabled = oils_obj
            .and_then(|o| o.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let older_than_years = oils_obj
            .and_then(|o| o.get("older_than_years"))
            .and_then(|v| {
                v.as_u64().map(|n| n as i64)
                    .or_else(|| v.as_i64())
                    .or_else(|| v.as_f64().map(|f| f as i64))
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
            })
            .filter(|n| *n > 0);
        // older_specific is normalised to { used, number }. A bare string from
        // a legacy rule is treated as { used: <string>, number: "" }.
        let older_specific = match oils_obj.and_then(|o| o.get("older_specific")) {
            Some(Value::String(s)) => json!({ "used": s.trim().to_string(), "number": "" }),
            Some(Value::Object(o)) => {
                let used = o.get("used").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let number = o.get("number").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                json!({ "used": used, "number": number })
            }
            _ => json!({ "used": "", "number": "" }),
        };
        obj.insert(
            "oils".to_string(),
            json!({
                "enabled": enabled,
                "older_than_years": older_than_years,
                "older_specific": older_specific,
            }),
        );
    }
}

async fn list_autofill_rules() -> Result<Json<Value>, (StatusCode, String)> {
    Ok(Json(json!({ "items": read_autofill_rules() })))
}

async fn get_autofill_rule(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let list = read_autofill_rules();
    let found = list
        .into_iter()
        .find(|r| r.get("id").and_then(|v| v.as_str()) == Some(&id))
        .ok_or((StatusCode::NOT_FOUND, "rule not found".to_string()))?;
    Ok(Json(found))
}

async fn create_autofill_rule(
    Json(mut payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = format!("r-{}", now_millis());
    normalize_rule_payload(&mut payload);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("id".to_string(), Value::String(id.clone()));
        let now = now_millis() as u64;
        obj.insert("created_ms".to_string(), Value::Number(serde_json::Number::from(now)));
        obj.insert("updated_ms".to_string(), Value::Number(serde_json::Number::from(now)));
    }
    let mut list = read_autofill_rules();
    list.push(payload);
    write_autofill_rules(&list)?;
    Ok(Json(json!({ "ok": true, "id": id })))
}

async fn update_autofill_rule(
    Path(id): Path<String>,
    Json(mut payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    normalize_rule_payload(&mut payload);
    let mut list = read_autofill_rules();
    let idx = list
        .iter()
        .position(|r| r.get("id").and_then(|v| v.as_str()) == Some(&id))
        .ok_or((StatusCode::NOT_FOUND, "rule not found".to_string()))?;
    let created_ms = list[idx]
        .get("created_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| now_millis() as u64);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("id".to_string(), Value::String(id.clone()));
        obj.insert("created_ms".to_string(), Value::Number(serde_json::Number::from(created_ms)));
        obj.insert("updated_ms".to_string(), Value::Number(serde_json::Number::from(now_millis() as u64)));
    }
    list[idx] = payload;
    write_autofill_rules(&list)?;
    Ok(Json(json!({ "ok": true, "id": id })))
}

async fn delete_autofill_rule(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let mut list = read_autofill_rules();
    list.retain(|r| r.get("id").and_then(|v| v.as_str()) != Some(&id));
    write_autofill_rules(&list)?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- migration ----------

/// Sweep over every existing car.json and normalise the `fuel_type` field.
///   • missing → "unknown"
///   • "hybrid" → "petrol-electric" (we now use Irish-reg-cert categories;
///     most hybrids are petrol-electric, the rare diesel-electric ones can
///     be re-classified manually)
// Fold every legacy Closed / Finished job into the new single-terminal
// Work done model.  For each job JSON under cars/*/jobs/*.json:
//   1. status closed | finished    → status work_done
//      (and: no status + finished:true → status work_done)
//   2. events of type "closed" — if the job already carries a work_done
//      event they're dropped as duplicates; otherwise they're renamed
//      in place to work_done (so the timeline keeps one terminal event).
//   3. work_done_ms is stamped from the file's mtime BEFORE we rewrite it
//      — that's the best available approximation of "when the mechanic
//      last touched this job". Frozen legacy jobs then correctly resolve
//      to isWorkDoneToday() = false on the client.
// Runs on every startup; no-op once every file already matches.
fn migrate_job_statuses() {
    let root = cars_dir();
    if !root.is_dir() {
        return;
    }
    let car_entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut status_rewrites  = 0usize;
    let mut events_touched   = 0usize;
    let mut wdms_filled      = 0usize;

    for car_entry in car_entries.flatten() {
        let car_dir = car_entry.path();
        if !car_dir.is_dir() {
            continue;
        }
        let jobs_dir = car_dir.join("jobs");
        if !jobs_dir.is_dir() {
            continue;
        }
        let job_entries = match std::fs::read_dir(&jobs_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for je in job_entries.flatten() {
            let jp = je.path();
            if jp.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }

            // Snapshot mtime BEFORE parsing/rewriting — the write below
            // would otherwise reset it to "now" and every legacy job
            // would resolve as isWorkDoneToday()=true, incorrectly
            // unlocking Reopen.
            let pre_mtime_ms: u64 = std::fs::metadata(&jp)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);

            let content = match std::fs::read_to_string(&jp) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut json: Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let obj = match json.as_object_mut() {
                Some(o) => o,
                None => continue,
            };

            let mut changed = false;

            // ---- 1. status normalisation ---------------------------
            let current_status = obj
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let is_legacy_terminal = matches!(
                current_status.as_deref(),
                Some("closed") | Some("finished")
            );
            let no_status_but_finished_bool = current_status.is_none()
                && obj
                    .get("finished")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
            if is_legacy_terminal || no_status_but_finished_bool {
                obj.insert(
                    "status".to_string(),
                    Value::String("work_done".to_string()),
                );
                status_rewrites += 1;
                changed = true;
            }

            // ---- 2. events cleanup ---------------------------------
            if let Some(events) = obj.get_mut("events").and_then(|v| v.as_array_mut()) {
                let has_work_done_event = events.iter().any(|ev| {
                    ev.get("type").and_then(|v| v.as_str()) == Some("work_done")
                });
                if has_work_done_event {
                    // Both events present — drop the closed ones so the
                    // timeline has a single terminal marker.
                    let before = events.len();
                    events.retain(|ev| {
                        ev.get("type").and_then(|v| v.as_str()) != Some("closed")
                    });
                    let dropped = before - events.len();
                    if dropped > 0 {
                        events_touched += dropped;
                        changed = true;
                    }
                } else {
                    // Rename closed → work_done in place. Preserves the
                    // moment the job actually finished for print + total.
                    for ev in events.iter_mut() {
                        if let Some(evo) = ev.as_object_mut() {
                            if evo.get("type").and_then(|v| v.as_str()) == Some("closed") {
                                evo.insert(
                                    "type".to_string(),
                                    Value::String("work_done".to_string()),
                                );
                                events_touched += 1;
                                changed = true;
                            }
                        }
                    }
                }
            }

            // ---- 3. work_done_ms stamping --------------------------
            // Re-read status after step 1 rewrote it.
            let now_status = obj
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("open")
                .to_string();
            if now_status == "work_done" {
                let has_wdm = obj
                    .get("work_done_ms")
                    .and_then(|v| v.as_u64())
                    .map(|n| n > 0)
                    .unwrap_or(false);
                if !has_wdm && pre_mtime_ms > 0 {
                    obj.insert(
                        "work_done_ms".to_string(),
                        Value::Number(serde_json::Number::from(pre_mtime_ms)),
                    );
                    wdms_filled += 1;
                    changed = true;
                }
            }

            if changed {
                if let Ok(pretty) = serde_json::to_string_pretty(&json) {
                    let _ = std::fs::write(&jp, pretty);
                }
            }
        }
    }

    if status_rewrites > 0 {
        println!(
            "Migrated {} jobs: status closed/finished → work_done.",
            status_rewrites
        );
    }
    if events_touched > 0 {
        println!(
            "Cleaned up {} closed timeline events (renamed / de-duplicated).",
            events_touched
        );
    }
    if wdms_filled > 0 {
        println!(
            "Stamped work_done_ms on {} legacy terminal jobs from mtime.",
            wdms_filled
        );
    }
}

// Move autofill/rules.json to settings/autofill.json on first startup
// after the reshuffle. Runs every boot — becomes a no-op once the file
// already lives in the new spot. The old autofill/ directory is
// removed too (if empty) so file-explorer doesn't dangle an empty dir.
fn migrate_autofill_location() {
    let old_dir  = PathBuf::from("autofill");
    let old_file = old_dir.join("rules.json");
    let new_file = autofill_rules_file();
    if !old_file.is_file() {
        // Nothing to move — but still take a swing at removing the empty
        // shell directory so it doesn't linger from a bare `cargo run`
        // that pre-dated this change.
        if old_dir.is_dir() {
            let _ = std::fs::remove_dir(&old_dir);
        }
        return;
    }
    // Don't clobber a real file that already lives at the new spot —
    // that would be a downgrade if the user hand-edited it.
    if new_file.exists() {
        let _ = std::fs::remove_file(&old_file);
        let _ = std::fs::remove_dir(&old_dir);
        return;
    }
    if let Some(parent) = new_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::rename(&old_file, &new_file).is_ok() {
        let _ = std::fs::remove_dir(&old_dir);
        println!("Migrated autofill/rules.json → settings/autofill.json");
    }
}

fn backfill_fuel_type() {
    let root = cars_dir();
    if !root.is_dir() {
        return;
    }
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut filled = 0usize;
    let mut renamed = 0usize;
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let car_json_path = p.join("car.json");
        if !car_json_path.is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(&car_json_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut json: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let obj = match json.as_object_mut() {
            Some(o) => o,
            None => continue,
        };
        let current = obj
            .get("fuel_type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let next = match current.as_deref() {
            None => Some("unknown".to_string()),                 // missing
            Some("hybrid") => Some("petrol-electric".to_string()), // legacy value
            _ => None,                                            // already fine
        };
        if let Some(new_val) = next {
            obj.insert("fuel_type".to_string(), Value::String(new_val));
            if let Ok(pretty) = serde_json::to_string_pretty(&json) {
                if std::fs::write(&car_json_path, pretty).is_ok() {
                    if current.is_none() { filled += 1; } else { renamed += 1; }
                }
            }
        }
    }
    if filled > 0 {
        println!("Backfilled fuel_type=unknown on {} existing cars.", filled);
    }
    if renamed > 0 {
        println!("Migrated fuel_type hybrid→petrol-electric on {} cars.", renamed);
    }
}

fn migrate_legacy_database() {
    let legacy = legacy_database_dir();
    if !legacy.is_dir() {
        return;
    }
    let entries = match std::fs::read_dir(&legacy) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut migrated = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let json: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let reg_no = json
            .get("vehicle")
            .and_then(|v| v.get("reg_no"))
            .and_then(|v| v.as_str())
            .unwrap_or("UNKNOWN");
        let plate = sanitize_plate(reg_no);
        let car_dir = cars_dir().join(&plate);
        let jobs_dir = car_dir.join("jobs");
        if std::fs::create_dir_all(&jobs_dir).is_err() {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("job.json")
            .to_string();
        let dest = jobs_dir.join(&file_name);
        if !dest.exists() {
            if std::fs::write(&dest, &content).is_ok() {
                migrated += 1;
            }
        }
        // Seed car.json if missing
        let car_path = car_dir.join("car.json");
        if !car_path.exists() {
            let make = json
                .get("vehicle")
                .and_then(|v| v.get("make"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let model = json
                .get("vehicle")
                .and_then(|v| v.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let car = json!({
                "plate": plate,
                "reg_no": reg_no,
                "make": make,
                "model": model,
                "customer_name": "",
                "customer_phone": "",
                "created_ms": now_millis() as u64,
                "updated_ms": now_millis() as u64,
                "source": "migrated",
            });
            let _ = std::fs::write(&car_path, serde_json::to_string_pretty(&car).unwrap_or_default());
        }
    }
    if migrated > 0 {
        println!("Migrated {} legacy job files into cars/", migrated);
    }
}

// ---------- store: divisions ----------

fn read_divisions() -> Vec<Value> {
    std::fs::read_to_string(store_divisions_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default()
}

fn write_divisions(list: &[Value]) -> Result<(), (StatusCode, String)> {
    let pretty = serde_json::to_string_pretty(list)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(store_divisions_file(), pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(())
}

async fn list_divisions() -> Result<Json<Value>, (StatusCode, String)> {
    let list = read_divisions();
    // Add item_count per division
    let items = read_all_store_items();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for item in &items {
        if let Some(div) = item.get("division_id").and_then(|v| v.as_str()) {
            *counts.entry(div.to_string()).or_insert(0) += 1;
        }
    }
    let enriched: Vec<Value> = list
        .into_iter()
        .map(|mut d| {
            if let Some(obj) = d.as_object_mut() {
                let id = obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let count = *counts.get(&id).unwrap_or(&0);
                obj.insert(
                    "item_count".to_string(),
                    Value::Number(serde_json::Number::from(count as u64)),
                );
                // Derived: does this division have a photo on disk?
                // Presence is more reliable than a stored flag (photo files
                // can be deleted out-of-band).
                obj.insert(
                    "has_photo".to_string(),
                    Value::Bool(find_division_photo(&id).is_some()),
                );
                // Default missing fields so the frontend can rely on shape.
                obj.entry("description")
                    .or_insert(Value::String(String::new()));
            }
            d
        })
        .collect();
    Ok(Json(json!({ "items": enriched })))
}

// Find whichever photo file lives for a given division id (any supported ext).
fn find_division_photo(id: &str) -> Option<PathBuf> {
    let dir = store_division_photos_dir();
    if !dir.is_dir() {
        return None;
    }
    let stem = sanitize_filename(id);
    let entries = std::fs::read_dir(&dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        if let Some(name) = p.file_stem().and_then(|s| s.to_str()) {
            if name == stem {
                return Some(p);
            }
        }
    }
    None
}

async fn create_division(
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name is required".to_string()));
    }
    let id = format!("d-{}", now_millis());
    let mut list = read_divisions();
    list.push(json!({
        "id": id,
        "name": name,
        "created_ms": now_millis() as u64,
        "updated_ms": now_millis() as u64,
    }));
    write_divisions(&list)?;
    Ok(Json(json!({ "ok": true, "id": id })))
}

async fn update_division(
    Path(id): Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    // Partial update: any of name / description may be present. name, when
    // provided, must be non-empty (it's the visible label). description is
    // optional and can be an empty string to clear it.
    let name_update = payload
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());
    let desc_update = payload
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());
    if let Some(n) = &name_update {
        if n.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "name is required".to_string()));
        }
    }
    let mut list = read_divisions();
    let mut found = false;
    for d in list.iter_mut() {
        if d.get("id").and_then(|v| v.as_str()) == Some(&id) {
            if let Some(obj) = d.as_object_mut() {
                if let Some(n) = &name_update {
                    obj.insert("name".to_string(), Value::String(n.clone()));
                }
                if let Some(desc) = &desc_update {
                    obj.insert("description".to_string(), Value::String(desc.clone()));
                }
                obj.insert(
                    "updated_ms".to_string(),
                    Value::Number(serde_json::Number::from(now_millis() as u64)),
                );
                found = true;
            }
        }
    }
    if !found {
        return Err((StatusCode::NOT_FOUND, "division not found".to_string()));
    }
    write_divisions(&list)?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- store: division photos ----------

async fn get_division_photo(
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    let path = find_division_photo(&id)
        .ok_or((StatusCode::NOT_FOUND, "no photo".to_string()))?;
    let bytes = std::fs::read(&path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mime = match path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    };
    let mut resp = Response::new(bytes.into());
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(resp)
}

async fn upload_division_photo(
    Path(id): Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    // Must reference a real division — refuse orphan photos.
    let list = read_divisions();
    let exists = list
        .iter()
        .any(|d| d.get("id").and_then(|v| v.as_str()) == Some(&id));
    if !exists {
        return Err((StatusCode::NOT_FOUND, "division not found".to_string()));
    }
    let raw_name = payload
        .get("filename")
        .and_then(|v| v.as_str())
        .unwrap_or("photo.png");
    let data_b64 = payload
        .get("data_base64")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "data_base64 required".to_string()))?;
    let b64_clean = if let Some(idx) = data_b64.find("base64,") {
        &data_b64[idx + 7..]
    } else {
        data_b64
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_clean.trim())
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let ext = std::path::Path::new(raw_name)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase())
        .filter(|s| ["png", "jpg", "jpeg", "svg", "webp", "gif"].contains(&s.as_str()))
        .unwrap_or_else(|| "png".to_string());

    std::fs::create_dir_all(store_division_photos_dir())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Drop any pre-existing photo (different ext) so only one file per id lives.
    if let Some(old) = find_division_photo(&id) {
        let _ = std::fs::remove_file(old);
    }
    let target = store_division_photos_dir().join(format!("{}.{}", id, ext));
    std::fs::write(&target, bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn delete_division_photo(
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if let Some(p) = find_division_photo(&id) {
        std::fs::remove_file(&p)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn delete_division(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    // Refuse to delete a division that still holds items — the store must be
    // cleared of everything in this group first.
    let item_count = read_all_store_items()
        .iter()
        .filter(|item| item.get("division_id").and_then(|v| v.as_str()) == Some(&id))
        .count();
    if item_count > 0 {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "Cannot delete: {} item(s) still in this division. Move or delete them first.",
                item_count
            ),
        ));
    }
    let mut list = read_divisions();
    list.retain(|d| d.get("id").and_then(|v| v.as_str()) != Some(&id));
    write_divisions(&list)?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- store: items ----------

#[derive(Serialize)]
struct StoreItemSummary {
    id: String,
    name: String,
    id_number: String,
    producer: String,
    description: String,
    notes: String,
    division_id: String,
    min_count: i64,
    max_count: i64,
    current_count: i64,
    // wheel/tyre-specific fields (empty strings for non-wheel items)
    wheel_size: String,
    wheel_season: String,
    wheel_type: String,
    wheel_pcd: String,
    wheel_et: String,
    // Barcodes / QR codes / manufacturer SKUs that scanners will match on.
    // Any code the user wants associated with this item goes here.
    barcodes: Vec<String>,
    images: Vec<String>,
    created_ms: u128,
    updated_ms: u128,
}

fn num_field(v: &Value, key: &str) -> i64 {
    v.get(key)
        .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|n| n as i64)))
        .unwrap_or(0)
}

fn read_store_item_dir(dir: &std::path::Path) -> Option<StoreItemSummary> {
    let id = dir.file_name()?.to_str()?.to_string();
    let item_json: Value = std::fs::read_to_string(dir.join("item.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())?;
    let name = item_json
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let id_number = item_json
        .get("id_number")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let producer = item_json
        .get("producer")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = item_json
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let notes = item_json
        .get("notes")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let division_id = item_json
        .get("division_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let min_count = num_field(&item_json, "min_count");
    let max_count = num_field(&item_json, "max_count");
    let current_count = num_field(&item_json, "current_count");
    let str_of = |key: &str| {
        item_json
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let wheel_size = str_of("wheel_size");
    let wheel_season = str_of("wheel_season");
    let wheel_type = str_of("wheel_type");
    let wheel_pcd = str_of("wheel_pcd");
    let wheel_et = str_of("wheel_et");
    let barcodes: Vec<String> = item_json
        .get("barcodes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let created_ms = item_json
        .get("created_ms")
        .and_then(|v| v.as_u64())
        .map(|n| n as u128)
        .unwrap_or(0);
    let updated_ms = item_json
        .get("updated_ms")
        .and_then(|v| v.as_u64())
        .map(|n| n as u128)
        .unwrap_or(0);

    let images_dir = dir.join("images");
    let mut images = Vec::new();
    if images_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&images_dir) {
            for e in entries.flatten() {
                if let Some(n) = e.file_name().to_str() {
                    images.push(n.to_string());
                }
            }
        }
    }
    images.sort();

    Some(StoreItemSummary {
        id,
        name,
        id_number,
        producer,
        description,
        notes,
        division_id,
        min_count,
        max_count,
        current_count,
        wheel_size,
        wheel_season,
        wheel_type,
        wheel_pcd,
        wheel_et,
        barcodes,
        images,
        created_ms,
        updated_ms,
    })
}

fn read_all_store_items() -> Vec<Value> {
    let dir = store_items_dir();
    if !dir.exists() {
        return vec![];
    }
    let mut items = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            if let Some(s) = read_store_item_dir(&p) {
                if let Ok(v) = serde_json::to_value(s) {
                    items.push(v);
                }
            }
        }
    }
    items
}

#[derive(serde::Deserialize)]
struct StoreItemsQuery {
    division: Option<String>,
}

async fn list_store_items(
    Query(params): Query<StoreItemsQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut items = read_all_store_items();
    if let Some(div) = params.division.as_ref().filter(|s| !s.is_empty()) {
        let div = div.clone();
        items.retain(|v| {
            v.get("division_id").and_then(|x| x.as_str()).unwrap_or("") == div.as_str()
        });
    }
    // Sort by updated_ms desc
    items.sort_by(|a, b| {
        let am = a.get("updated_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let bm = b.get("updated_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        bm.cmp(&am)
    });
    Ok(Json(json!({ "items": items })))
}

async fn get_store_item(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = store_items_dir().join(&id);
    let summary = read_store_item_dir(&dir)
        .ok_or((StatusCode::NOT_FOUND, "store item not found".to_string()))?;
    Ok(Json(serde_json::to_value(summary).unwrap_or(Value::Null)))
}

fn normalize_store_payload(payload: &mut Value) {
    if let Some(obj) = payload.as_object_mut() {
        for k in ["min_count", "max_count", "current_count"] {
            if let Some(v) = obj.get(k) {
                let n = v
                    .as_i64()
                    .or_else(|| v.as_f64().map(|f| f as i64))
                    .or_else(|| {
                        v.as_str()
                            .and_then(|s| s.trim().parse::<i64>().ok())
                    })
                    .unwrap_or(0);
                obj.insert(k.to_string(), Value::Number(serde_json::Number::from(n)));
            }
        }
        // Barcodes: accept an array of strings or a newline / comma-separated
        // single string. Trim, drop empties, dedupe (case-insensitive), keep order.
        let raw = obj.remove("barcodes").unwrap_or(Value::Null);
        let candidates: Vec<String> = match raw {
            Value::Array(arr) => arr
                .into_iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
            Value::String(s) => s
                .split(|c: char| c == '\n' || c == ',')
                .map(|s| s.to_string())
                .collect(),
            _ => Vec::new(),
        };
        let mut seen = std::collections::HashSet::<String>::new();
        let normalised: Vec<Value> = candidates
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .filter(|s| seen.insert(s.to_lowercase()))
            .map(Value::String)
            .collect();
        obj.insert("barcodes".to_string(), Value::Array(normalised));
    }
}

async fn create_store_item(
    Json(mut payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = format!("s-{}", now_millis());
    let dir = store_items_dir().join(&id);
    std::fs::create_dir_all(dir.join("images"))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    normalize_store_payload(&mut payload);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("id".to_string(), Value::String(id.clone()));
        obj.insert(
            "created_ms".to_string(),
            Value::Number(serde_json::Number::from(now_millis() as u64)),
        );
        obj.insert(
            "updated_ms".to_string(),
            Value::Number(serde_json::Number::from(now_millis() as u64)),
        );
    }
    let pretty = serde_json::to_string_pretty(&payload)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(dir.join("item.json"), pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true, "id": id })))
}

async fn update_store_item(
    Path(id): Path<String>,
    Json(mut payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = store_items_dir().join(&id);
    if !dir.exists() {
        return Err((StatusCode::NOT_FOUND, "store item not found".to_string()));
    }
    let existing: Value = std::fs::read_to_string(dir.join("item.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let created_ms = existing
        .get("created_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| now_millis() as u64);
    normalize_store_payload(&mut payload);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("id".to_string(), Value::String(id.clone()));
        obj.insert(
            "created_ms".to_string(),
            Value::Number(serde_json::Number::from(created_ms)),
        );
        obj.insert(
            "updated_ms".to_string(),
            Value::Number(serde_json::Number::from(now_millis() as u64)),
        );
    }
    let pretty = serde_json::to_string_pretty(&payload)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(dir.join("item.json"), pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true, "id": id })))
}

async fn delete_store_item(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = store_items_dir().join(&id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn upload_store_item_image(
    Path(id): Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = store_items_dir().join(&id).join("images");
    std::fs::create_dir_all(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let raw_name = payload
        .get("filename")
        .and_then(|v| v.as_str())
        .unwrap_or("image.png");
    let data_b64 = payload
        .get("data_base64")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "data_base64 required".to_string()))?;

    let b64_clean = if let Some(idx) = data_b64.find("base64,") {
        &data_b64[idx + 7..]
    } else {
        data_b64
    };

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_clean.trim())
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let safe = sanitize_filename(raw_name);
    let safe = if safe.is_empty() {
        "image.png".to_string()
    } else {
        safe
    };
    let file_name = format!("{}-{}", now_millis(), safe);

    std::fs::write(dir.join(&file_name), bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Bump updated_ms
    let item_json_path = store_items_dir().join(&id).join("item.json");
    if let Ok(content) = std::fs::read_to_string(&item_json_path) {
        if let Ok(mut v) = serde_json::from_str::<Value>(&content) {
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "updated_ms".to_string(),
                    Value::Number(serde_json::Number::from(now_millis() as u64)),
                );
                let _ = std::fs::write(
                    &item_json_path,
                    serde_json::to_string_pretty(&v).unwrap_or(content),
                );
            }
        }
    }

    Ok(Json(json!({ "ok": true, "filename": file_name })))
}

async fn delete_store_item_image(
    Path((id, filename)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let filename = sanitize_filename(&filename);
    let path = store_items_dir().join(&id).join("images").join(&filename);
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}

// ---------- stocktake (inventory audit from scanner file) ----------
//
// Workflow: user scans every item in the warehouse on a phone / handheld
// scanner; the app dumps a file of barcodes; user uploads that file here.
// We match codes against `barcodes[]` on each store item and either report
// a preview (apply=false) or write the new `current_count` values back
// (apply=true).
//
// Input file: one scan per non-empty line. Lines starting with `#` are
// treated as comments. If a line has a numeric second token (comma /
// whitespace / tab separated), it's used as the count for that code —
// otherwise each line counts as one scan.
//
// mode = "strict" (default): items that were never scanned are set to 0
//        (a full stocktake covered everything, absence = zero on shelf).
// mode = "partial":          only matched items are touched; the rest are
//        left as they were.

#[derive(serde::Deserialize)]
struct StocktakePayload {
    #[serde(default)]
    text: String,
    #[serde(default)]
    apply: bool,
    #[serde(default)]
    mode: Option<String>,
}

async fn stocktake(
    Json(payload): Json<StocktakePayload>,
) -> Result<Json<Value>, (StatusCode, String)> {
    use std::collections::HashMap;

    // 1. Parse the file into a per-code count map.
    let mut counts: HashMap<String, i64> = HashMap::new();
    let mut total_scans: i64 = 0;
    let mut lines_parsed: i64 = 0;
    for raw_line in payload.text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        lines_parsed += 1;
        // Split on comma / tab / whitespace; keep the first non-empty token
        // as the code. If a second token is a positive int, it's the count.
        let tokens: Vec<&str> = line
            .split(|c: char| c == ',' || c == '\t' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .collect();
        if tokens.is_empty() {
            continue;
        }
        let code = tokens[0].trim().to_string();
        if code.is_empty() {
            continue;
        }
        let count: i64 = if tokens.len() >= 2 {
            tokens[1].trim().parse::<i64>().ok().filter(|n| *n > 0).unwrap_or(1)
        } else {
            1
        };
        *counts.entry(code).or_insert(0) += count;
        total_scans += count;
    }
    let unique_codes = counts.len();

    // 2. Load every store item and build code(lower) -> item_index map.
    let items = read_all_store_items();
    let mut code_to_item: HashMap<String, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        if let Some(bcs) = item.get("barcodes").and_then(|v| v.as_array()) {
            for b in bcs {
                if let Some(s) = b.as_str() {
                    let norm = s.trim().to_lowercase();
                    if !norm.is_empty() {
                        // Last wins; if two items share a barcode, we can't
                        // decide which — flag it in the report.
                        code_to_item.insert(norm, i);
                    }
                }
            }
        }
    }

    // 3. Match scans -> items. Accumulate per-item scanned totals and
    //    collect unmatched codes.
    let mut item_scans: HashMap<usize, i64> = HashMap::new();
    let mut unmatched: Vec<(String, i64)> = Vec::new();
    for (code, cnt) in &counts {
        let key = code.trim().to_lowercase();
        if let Some(&idx) = code_to_item.get(&key) {
            *item_scans.entry(idx).or_insert(0) += cnt;
        } else {
            unmatched.push((code.clone(), *cnt));
        }
    }
    unmatched.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // 4. Build the updates list (matched items with was/now/diff).
    let item_field = |item: &Value, key: &str| -> String {
        item.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let item_i64 = |item: &Value, key: &str| -> i64 {
        item.get(key)
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    };

    let mut updates: Vec<Value> = Vec::new();
    for (&idx, &now) in &item_scans {
        let item = &items[idx];
        let was = item_i64(item, "current_count");
        updates.push(json!({
            "id": item_field(item, "id"),
            "name": item_field(item, "name"),
            "id_number": item_field(item, "id_number"),
            "was": was,
            "now": now,
            "diff": now - was,
        }));
    }
    updates.sort_by(|a, b| {
        let an = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let bn = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        an.to_lowercase().cmp(&bn.to_lowercase())
    });

    // 5. Strict mode: items that weren't scanned drop to 0 (skipping ones
    //    already at 0 — no change to report).
    let mode = payload
        .mode
        .as_deref()
        .unwrap_or("strict")
        .trim()
        .to_lowercase();
    let strict = mode != "partial";
    let mut not_scanned: Vec<Value> = Vec::new();
    if strict {
        for (i, item) in items.iter().enumerate() {
            if item_scans.contains_key(&i) {
                continue;
            }
            let was = item_i64(item, "current_count");
            if was == 0 {
                continue;
            }
            not_scanned.push(json!({
                "id": item_field(item, "id"),
                "name": item_field(item, "name"),
                "id_number": item_field(item, "id_number"),
                "was": was,
                "now": 0,
                "diff": -was,
            }));
        }
        not_scanned.sort_by(|a, b| {
            let an = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let bn = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            an.to_lowercase().cmp(&bn.to_lowercase())
        });
    }

    // 6. Apply — write current_count back to disk. Only when apply=true.
    let mut applied_writes = 0i64;
    let mut write_errors: Vec<String> = Vec::new();
    if payload.apply {
        let mut targets: Vec<(String, i64)> = Vec::new();
        for (&idx, &now) in &item_scans {
            if let Some(id) = items[idx].get("id").and_then(|v| v.as_str()) {
                targets.push((id.to_string(), now));
            }
        }
        if strict {
            for (i, item) in items.iter().enumerate() {
                if item_scans.contains_key(&i) {
                    continue;
                }
                let was = item_i64(item, "current_count");
                if was == 0 {
                    continue;
                }
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    targets.push((id.to_string(), 0));
                }
            }
        }
        for (id, new_count) in &targets {
            match write_store_item_count(id, *new_count) {
                Ok(()) => applied_writes += 1,
                Err(e) => write_errors.push(format!("{}: {}", id, e)),
            }
        }
    }

    Ok(Json(json!({
        "ok": true,
        "applied": payload.apply,
        "mode": if strict { "strict" } else { "partial" },
        "lines_parsed": lines_parsed,
        "total_scans": total_scans,
        "unique_codes": unique_codes,
        "items_matched": item_scans.len(),
        "updates": updates,
        "unmatched": unmatched
            .into_iter()
            .map(|(c, n)| json!({ "code": c, "count": n }))
            .collect::<Vec<_>>(),
        "not_scanned": not_scanned,
        "applied_writes": applied_writes,
        "write_errors": write_errors,
    })))
}

fn write_store_item_count(id: &str, new_count: i64) -> Result<(), String> {
    let id = sanitize_filename(id);
    let path = store_items_dir().join(&id).join("item.json");
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut v: Value = serde_json::from_str(&content).map_err(|e| e.to_string())?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "current_count".to_string(),
            Value::Number(serde_json::Number::from(new_count)),
        );
        obj.insert(
            "updated_ms".to_string(),
            Value::Number(serde_json::Number::from(now_millis() as u64)),
        );
    }
    let pretty = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    std::fs::write(&path, pretty).map_err(|e| e.to_string())?;
    Ok(())
}

// ---------- settings (branding, screensaver, print flags, logo) ----------
//
// Single JSON at settings/settings.json plus a separate logo file
// (settings/logo.<ext>). The whole app can read /api/settings on load to
// pull the company name for titles/topbars; index.html additionally uses
// the idle / logo fields to drive its screensaver overlay.

fn settings_defaults() -> Value {
    json!({
        "company_name": "",
        "company_phone": "",
        "company_address": "",
        "screensaver_enabled": true,
        "screensaver_idle_minutes": 10,
        "screensaver_bg_color": "#000000",
        "print_show_logo": true,
        // Per-event chat notifications are now stored per-worker (see
        // worker.json → chat_notify_*), not in global settings.
        // Chat feed retention. Three fields combine into one duration so the
        // user can say e.g. "1 month" or "15 days" or "12 hours" naturally.
        // 0 in every slot means "no limit — keep everything".
        "chat_retention_months": 0,
        "chat_retention_days":   0,
        "chat_retention_hours":  0,
        // When true, messages past the retention window get PURGED from
        // localStorage on every render pass. When false they're just hidden
        // from the feed (so raising the limit later brings them back).
        "chat_retention_delete": false,
        // Chat auto-close on inactivity. Three fields combine:
        //   use_screensaver = true  → idle = screensaver_idle_minutes * pct/100
        //   use_screensaver = false → idle = fallback_minutes
        // If use_screensaver is on but screensaver_idle_minutes is 0, we
        // also fall through to fallback_minutes.
        "chat_autoclose_use_screensaver": true,
        "chat_autoclose_screensaver_pct": 30,
        "chat_autoclose_fallback_minutes": 2,
        // Notification sound for new incoming chat items. Volume 0-100.
        // We only play for items that came in from the server (poll), never
        // for messages the local user just typed themselves.
        "chat_sound_enabled": true,
        "chat_sound_volume": 60,
        // Scanner (companion phone app) config.
        //   pair_ttl_minutes — how long a pairing QR stays valid before the
        //     token expires and you have to regenerate the code.
        //   default_division — where /stocktake auto-routes scans from the
        //     phone if the scan itself carries no division hint.
        "scanner_pair_ttl_minutes": 10,
        "scanner_default_division": "",
        // Backup / restore. Engine is unimplemented — these fields exist
        // so the Settings → Backup page can already persist the user's
        // choices, and when the scheduler + download endpoints come
        // online they just read these values. `backup_last_ms` is
        // updated by the future engine, not by the settings PUT.
        "backup_auto_enabled": false,
        "backup_auto_destination": "",
        "backup_auto_schedule": "daily",
        "backup_last_ms": 0,
        // Work hours + lunch. Times are HH:MM strings so an
        // <input type="time"> binds directly. Automation was scrapped
        // in favour of a purely-passive model: raw start/end times get
        // stored on the job, and only when a job is displayed / printed
        // we subtract the lunch window if `deduct_lunch_from_elapsed`
        // is on. Retroactive, no scheduler, no state juggling.
        "work_day_start_hhmm": "09:00",
        "work_day_end_hhmm": "18:00",
        "work_lunch_start_hhmm": "13:00",
        "work_lunch_end_hhmm": "14:00",
        "deduct_lunch_from_elapsed": false,
    })
}

// Find whichever logo file lives in settings/ regardless of extension.
// We save as "logo.<ext>" (jpg/png/svg/webp) — first match wins.
fn find_settings_logo() -> Option<PathBuf> {
    let dir = settings_dir();
    if !dir.is_dir() {
        return None;
    }
    let entries = std::fs::read_dir(&dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            if stem.eq_ignore_ascii_case("logo") {
                return Some(p);
            }
        }
    }
    None
}

async fn get_settings() -> Result<Json<Value>, (StatusCode, String)> {
    let mut current = settings_defaults();
    if let Ok(txt) = std::fs::read_to_string(settings_file()) {
        if let Ok(parsed) = serde_json::from_str::<Value>(&txt) {
            // Merge over defaults so newly-added keys always have a value.
            if let (Some(cur), Some(new)) = (current.as_object_mut(), parsed.as_object()) {
                for (k, v) in new {
                    cur.insert(k.clone(), v.clone());
                }
            }
        }
    }
    if let Some(obj) = current.as_object_mut() {
        obj.insert("has_logo".to_string(), Value::Bool(find_settings_logo().is_some()));
    }
    Ok(Json(current))
}

async fn update_settings(
    Json(mut payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Read + merge so partial PUTs (e.g. just screensaver) don't wipe branding.
    let mut current = settings_defaults();
    if let Ok(txt) = std::fs::read_to_string(settings_file()) {
        if let Ok(parsed) = serde_json::from_str::<Value>(&txt) {
            if let (Some(cur), Some(new)) = (current.as_object_mut(), parsed.as_object()) {
                for (k, v) in new {
                    cur.insert(k.clone(), v.clone());
                }
            }
        }
    }
    if let (Some(cur), Some(new)) = (current.as_object_mut(), payload.as_object_mut()) {
        // has_logo is derived from disk, not stored — reject any attempt to set it.
        new.remove("has_logo");
        // Coerce numeric fields written as strings ("10" → 10). Applies to
        // every numeric setting we accept so form inputs that submit as text
        // still land in settings.json as numbers.
        for key in [
            "screensaver_idle_minutes",
            "chat_retention_months",
            "chat_retention_days",
            "chat_retention_hours",
            "chat_autoclose_screensaver_pct",
            "chat_autoclose_fallback_minutes",
            "chat_sound_volume",
            "scanner_pair_ttl_minutes",
        ] {
            if let Some(v) = new.get_mut(key) {
                if let Some(s) = v.as_str() {
                    if let Ok(n) = s.trim().parse::<i64>() {
                        *v = Value::Number(serde_json::Number::from(n));
                    }
                }
            }
        }
        for (k, v) in new.iter() {
            cur.insert(k.clone(), v.clone());
        }
    }
    let pretty = serde_json::to_string_pretty(&current)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::create_dir_all(settings_dir())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(settings_file(), pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn get_settings_logo() -> Result<Response, (StatusCode, String)> {
    let path = find_settings_logo()
        .ok_or((StatusCode::NOT_FOUND, "no logo".to_string()))?;
    let bytes = std::fs::read(&path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mime = match path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    };
    let mut resp = Response::new(bytes.into());
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(resp)
}

async fn upload_settings_logo(
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Same base64 shape as car / store item photo uploads.
    let raw_name = payload
        .get("filename")
        .and_then(|v| v.as_str())
        .unwrap_or("logo.png");
    let data_b64 = payload
        .get("data_base64")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "data_base64 required".to_string()))?;
    let b64_clean = if let Some(idx) = data_b64.find("base64,") {
        &data_b64[idx + 7..]
    } else {
        data_b64
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_clean.trim())
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    // Detect extension from the incoming filename; fall back to .png.
    let ext = std::path::Path::new(raw_name)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase())
        .filter(|s| ["png", "jpg", "jpeg", "svg", "webp", "gif"].contains(&s.as_str()))
        .unwrap_or_else(|| "png".to_string());

    std::fs::create_dir_all(settings_dir())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Drop any pre-existing logo file (may have a different extension) so
    // there's always exactly one logo file on disk.
    if let Some(old) = find_settings_logo() {
        let _ = std::fs::remove_file(old);
    }

    let target = settings_dir().join(format!("logo.{}", ext));
    std::fs::write(&target, bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn delete_settings_logo() -> Result<Json<Value>, (StatusCode, String)> {
    if let Some(p) = find_settings_logo() {
        std::fs::remove_file(&p)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}

// Embedded so the exe stays self-contained — no need to ship favicon.svg
// alongside the binary. If we later edit public/favicon.svg for local dev
// tweaks, a `cargo build --release` picks up the new bytes automatically.
static FAVICON_SVG: &[u8] = include_bytes!("../public/favicon.svg");

async fn serve_favicon() -> Response {
    let mut resp = Response::new(FAVICON_SVG.to_vec().into());
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"));
    // Favicons are safe to cache long — content only changes on redeploy.
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400"));
    resp
}

// ---------- shared workshop chat ----------
//
// One JSON file (`chat/messages.json`) holds the entire feed. Everyone reads
// from the same file so all browsers on the local network see the same
// history. Polling is client-driven: while the user has the chat open, the
// client asks for anything newer than the last message it saw; when the chat
// is closed we don't hear from that client at all.
//
// Retention: `chat_retention_delete: true` in settings.json triggers a
// physical prune on every read — messages older than the retention window
// are removed from disk. `false` = keep everything, client filters for
// display only.

fn read_chat_feed_raw() -> Vec<Value> {
    std::fs::read_to_string(chat_messages_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default()
}

fn write_chat_feed(list: &[Value]) -> Result<(), (StatusCode, String)> {
    let pretty = serde_json::to_string_pretty(list)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::create_dir_all(chat_dir())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(chat_messages_file(), pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(())
}

// Read chat_retention_* from settings, return the earliest ts we should
// keep. Returns None if retention is disabled (all-zero fields).
fn chat_retention_cutoff() -> Option<u128> {
    let s = std::fs::read_to_string(settings_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())?;
    let months = s.get("chat_retention_months").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as u128;
    let days   = s.get("chat_retention_days")  .and_then(|v| v.as_i64()).unwrap_or(0).max(0) as u128;
    let hours  = s.get("chat_retention_hours") .and_then(|v| v.as_i64()).unwrap_or(0).max(0) as u128;
    let total_hours = months * 30 * 24 + days * 24 + hours;
    if total_hours == 0 {
        return None;
    }
    let window_ms = total_hours * 3600 * 1000;
    let now = now_millis();
    Some(now.saturating_sub(window_ms))
}

// Apply retention (only if delete flag is on) and return the possibly
// pruned feed. Also physically writes back to disk when it prunes.
fn apply_chat_retention(mut feed: Vec<Value>) -> Vec<Value> {
    let should_delete = std::fs::read_to_string(settings_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|s| s.get("chat_retention_delete").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    if !should_delete {
        return feed;
    }
    let Some(cutoff) = chat_retention_cutoff() else { return feed };
    let before = feed.len();
    feed.retain(|it| {
        it.get("ts")
            .and_then(|v| v.as_u64())
            .map(|n| (n as u128) >= cutoff)
            .unwrap_or(true) // keep entries without ts (shouldn't happen)
    });
    if feed.len() != before {
        let _ = write_chat_feed(&feed);
    }
    feed
}

#[derive(serde::Deserialize)]
struct ChatFeedQuery {
    /// Only return messages with ts strictly greater than this.
    /// Client sends the max ts it already has, so polls only pull new stuff.
    since: Option<u64>,
    /// Presence identity — stable across polls for one browser session.
    /// Anything unique enough (worker_id if signed in, random session key
    /// otherwise). Used to compute the "online now" tile in the chat
    /// header. Not persisted; the map lives in-process only.
    #[serde(default)]
    client_id: String,
}

// In-memory presence table — {client_id: last-seen ms}. Rebuilds from
// scratch on server restart, which is fine for a "who's online right
// now" indicator. Entries older than PRESENCE_TTL_MS are pruned on
// every read + write, so the table can't grow without bound even
// under weird client_id churn.
const PRESENCE_TTL_MS: u64 = 10 * 60 * 1000; // 10 min

fn presence_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, u64>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, u64>>>
        = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn touch_and_count_presence(client_id: &str) -> u64 {
    let now = now_millis() as u64;
    let cutoff = now.saturating_sub(PRESENCE_TTL_MS);
    let mut map = presence_map().lock().unwrap();
    if !client_id.is_empty() {
        map.insert(client_id.to_string(), now);
    }
    // Prune expired entries so the counter reflects reality and the
    // map doesn't leak memory as browsers close.
    map.retain(|_, ts| *ts >= cutoff);
    map.len() as u64
}

async fn list_chat_feed(
    Query(params): Query<ChatFeedQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let feed = apply_chat_retention(read_chat_feed_raw());
    let since = params.since.unwrap_or(0) as u128;
    let items: Vec<&Value> = feed
        .iter()
        .filter(|it| {
            it.get("ts")
                .and_then(|v| v.as_u64())
                .map(|n| (n as u128) > since)
                .unwrap_or(false)
        })
        .collect();
    let online = touch_and_count_presence(params.client_id.trim());
    Ok(Json(json!({ "items": items, "online": online })))
}

// Append a value to the feed with a fresh, strictly-monotonic timestamp.
// Returns the value that was actually stored (with the assigned ts + id).
fn append_chat_entry(mut entry: Value) -> Result<Value, (StatusCode, String)> {
    let mut feed = read_chat_feed_raw();
    // ts monotonically increases so two rapid POSTs don't collide.
    let last_ts = feed
        .iter()
        .filter_map(|it| it.get("ts").and_then(|v| v.as_u64()))
        .max()
        .unwrap_or(0);
    let now = now_millis() as u64;
    let ts = std::cmp::max(now, last_ts + 1);
    if let Some(obj) = entry.as_object_mut() {
        obj.insert("id".to_string(), Value::String(format!("m-{}", ts)));
        obj.insert("ts".to_string(), Value::Number(serde_json::Number::from(ts)));
    }
    feed.push(entry.clone());
    write_chat_feed(&feed)?;
    Ok(entry)
}

#[derive(serde::Deserialize)]
struct ChatMessageBody {
    text: String,
    #[serde(default)]
    author: String,
}

async fn post_chat_message(
    Json(payload): Json<ChatMessageBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let text = payload.text.trim();
    if text.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "text is required".to_string()));
    }
    let entry = json!({
        "type": "msg",
        "author": payload.author.trim(),
        "text": text,
    });
    let saved = append_chat_entry(entry)?;
    Ok(Json(json!({ "ok": true, "item": saved })))
}

#[derive(serde::Deserialize)]
struct ChatEventBody {
    text: String,
    #[serde(default)]
    kind: String,
    // Fine-grained identifier used by per-worker chat notification
    // filters — one of "job_started", "job_finished", "job_reopened",
    // "stock_arrival", "low_stock". Empty for events that shouldn't
    // participate in the filter (they always show).
    #[serde(default)]
    notify_key: String,
}

async fn post_chat_event(
    Json(payload): Json<ChatEventBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let text = payload.text.trim();
    if text.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "text is required".to_string()));
    }
    // Normalise kind — anything unknown becomes "generic" so the frontend
    // always has a valid icon.
    let kind = match payload.kind.as_str() {
        "wheels" | "checkin" | "done" | "parts" | "generic" => payload.kind,
        _ => "generic".to_string(),
    };
    let entry = json!({
        "type": "event",
        "kind": kind,
        "notify_key": payload.notify_key.trim(),
        "text": text,
    });
    let saved = append_chat_entry(entry)?;
    Ok(Json(json!({ "ok": true, "item": saved })))
}

// ---------- scanner: companion phone app pairing + intake ----------
//
// Flow:
//   1. Desktop opens /settings-scanner.html and calls POST /api/scanner/pair
//      with its own origin URL. Server mints a short-lived token, stores
//      pending + server URL, and returns the token.
//   2. Page renders /api/scanner/pair/:token/qr — an SVG QR encoding a JSON
//      payload {v:1, server, token}.
//   3. Phone scans the QR, extracts server+token, POSTs /api/scanner/register
//      with the token. Server swaps the pair token for a permanent device
//      token and returns it. The device stores this and uses it in the
//      Authorization: Bearer header for every subsequent /api/scanner/scan.
//   4. Desktop polls /api/scanner/pair/:token/status until it flips from
//      "waiting" to "paired" — that's when the settings UI can show the new
//      device in the list without a manual refresh.
//
// Retention / expiry:
//   Pending tokens auto-drop on any read once their expires_ms passes.
//   Scans + devices stay until explicitly deleted.

// Not a CSPRNG — LAN pairing tokens live for ~10 min in a trusted network.
// Mixes nanosecond clock, PID, a monotonic counter and an xorshift so guessing
// one token doesn't leak the next.
static SCANNER_TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

fn scanner_new_token() -> String {
    let counter = SCANNER_TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let time_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mut state: u64 = time_ns
        .wrapping_mul(counter.wrapping_add(1))
        .wrapping_add(std::process::id() as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut out = String::with_capacity(48);
    for _ in 0..6 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push_str(&format!("{:016x}", state));
    }
    out.truncate(48);
    out
}

fn scanner_new_id(prefix: &str) -> String {
    format!("{}-{}", prefix, &scanner_new_token()[..16])
}

fn read_json_array(path: &PathBuf) -> Vec<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default()
}

fn write_json_array(path: &PathBuf, list: &[Value]) -> Result<(), (StatusCode, String)> {
    let pretty = serde_json::to_string_pretty(list)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    std::fs::write(path, pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(())
}

fn scanner_pair_ttl_ms() -> u128 {
    // Look up TTL in settings.json — falls back to 10 min if the file is
    // missing or the field isn't set.
    let mut minutes: u64 = 10;
    if let Ok(txt) = std::fs::read_to_string(settings_file()) {
        if let Ok(parsed) = serde_json::from_str::<Value>(&txt) {
            if let Some(n) = parsed.get("scanner_pair_ttl_minutes").and_then(|v| v.as_u64()) {
                if n > 0 {
                    minutes = n;
                }
            }
        }
    }
    (minutes as u128) * 60 * 1000
}

fn prune_expired_pair_tokens(list: &mut Vec<Value>) {
    let now = now_millis();
    list.retain(|t| {
        t.get("expires_ms")
            .and_then(|v| v.as_u64())
            .map(|e| (e as u128) > now)
            .unwrap_or(false)
    });
}

#[derive(serde::Deserialize)]
struct CreatePairBody {
    #[serde(default)]
    server_url: String,
}

async fn create_scanner_pair(
    Json(payload): Json<CreatePairBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let server_url = payload.server_url.trim().to_string();
    if server_url.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "server_url is required".into()));
    }
    let mut list = read_json_array(&scanner_pending_tokens_file());
    prune_expired_pair_tokens(&mut list);
    let token = scanner_new_token();
    let now = now_millis();
    let expires_ms = now + scanner_pair_ttl_ms();
    let entry = json!({
        "token": token,
        "server_url": server_url,
        "created_ms": now as u64,
        "expires_ms": expires_ms as u64,
        "consumed_device_id": Value::Null,
    });
    list.push(entry);
    write_json_array(&scanner_pending_tokens_file(), &list)?;
    Ok(Json(json!({
        "token": token,
        "expires_ms": expires_ms as u64,
        "qr_url": format!("/api/scanner/pair/{}/qr", token),
        "status_url": format!("/api/scanner/pair/{}/status", token),
    })))
}

async fn get_scanner_pair_qr(Path(token): Path<String>) -> Result<Response, (StatusCode, String)> {
    let mut list = read_json_array(&scanner_pending_tokens_file());
    prune_expired_pair_tokens(&mut list);
    let entry = list
        .iter()
        .find(|t| t.get("token").and_then(|v| v.as_str()) == Some(&token))
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "unknown or expired token".into()))?;
    let server_url = entry
        .get("server_url")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // JSON payload for the future companion APK to decode. Version tag lets
    // us evolve the format later without breaking older installs.
    let payload = json!({
        "v": 1,
        "kind": "garage-scanner-pair",
        "server": server_url,
        "token": token,
    });
    let payload_str = payload.to_string();
    let code = qrcode::QrCode::new(payload_str.as_bytes())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .dark_color(qrcode::render::svg::Color("#111111"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build();
    let mut resp = Response::new(svg.into_bytes().into());
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"));
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

async fn get_scanner_pair_status(
    Path(token): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut list = read_json_array(&scanner_pending_tokens_file());
    prune_expired_pair_tokens(&mut list);
    let Some(entry) = list
        .iter()
        .find(|t| t.get("token").and_then(|v| v.as_str()) == Some(&token))
    else {
        return Ok(Json(json!({ "status": "expired" })));
    };
    if let Some(dev_id) = entry.get("consumed_device_id").and_then(|v| v.as_str()) {
        let devices = read_json_array(&scanner_devices_file());
        let device = devices
            .iter()
            .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(dev_id))
            .cloned()
            .unwrap_or(Value::Null);
        return Ok(Json(json!({
            "status": "paired",
            "device": device,
        })));
    }
    let expires_ms = entry.get("expires_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    Ok(Json(json!({
        "status": "waiting",
        "expires_ms": expires_ms,
    })))
}

#[derive(serde::Deserialize)]
struct RegisterDeviceBody {
    pair_token: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    app_version: String,
}

async fn register_scanner_device(
    Json(payload): Json<RegisterDeviceBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut pending = read_json_array(&scanner_pending_tokens_file());
    prune_expired_pair_tokens(&mut pending);
    let idx = pending
        .iter()
        .position(|t| t.get("token").and_then(|v| v.as_str()) == Some(payload.pair_token.as_str()))
        .ok_or((StatusCode::BAD_REQUEST, "pairing token expired or unknown".into()))?;
    if pending[idx]
        .get("consumed_device_id")
        .and_then(|v| v.as_str())
        .is_some()
    {
        return Err((StatusCode::CONFLICT, "pairing token already used".into()));
    }
    let now = now_millis() as u64;
    let device_id = scanner_new_id("dev");
    let device_token = scanner_new_token();
    let name = if payload.name.trim().is_empty() {
        "Scanner".to_string()
    } else {
        payload.name.trim().to_string()
    };
    let platform = if payload.platform.trim().is_empty() {
        "unknown".to_string()
    } else {
        payload.platform.trim().to_string()
    };
    let app_version = payload.app_version.trim().to_string();
    let device = json!({
        "id": device_id,
        "name": name,
        "device_token": device_token,
        "created_ms": now,
        "last_seen_ms": now,
        "platform": platform,
        "app_version": app_version,
    });
    let mut devices = read_json_array(&scanner_devices_file());
    devices.push(device.clone());
    write_json_array(&scanner_devices_file(), &devices)?;

    pending[idx]["consumed_device_id"] = Value::String(device_id.clone());
    write_json_array(&scanner_pending_tokens_file(), &pending)?;

    Ok(Json(json!({
        "device_id": device_id,
        "device_token": device_token,
        "name": device.get("name").cloned().unwrap_or(Value::Null),
    })))
}

async fn list_scanner_devices() -> Result<Json<Value>, (StatusCode, String)> {
    let devices = read_json_array(&scanner_devices_file());
    // Strip device_token before returning to the browser — it's a bearer
    // credential; only the phone that got it at register-time needs it.
    let scrubbed: Vec<Value> = devices
        .into_iter()
        .map(|mut d| {
            if let Some(obj) = d.as_object_mut() {
                obj.remove("device_token");
            }
            d
        })
        .collect();
    Ok(Json(json!({ "items": scrubbed })))
}

async fn delete_scanner_device(
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut devices = read_json_array(&scanner_devices_file());
    let before = devices.len();
    devices.retain(|d| d.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
    if devices.len() == before {
        return Err((StatusCode::NOT_FOUND, "device not found".into()));
    }
    write_json_array(&scanner_devices_file(), &devices)?;
    Ok(Json(json!({ "ok": true })))
}

// Look up the caller's device by Authorization: Bearer <device_token>.
// Returns (index_in_devices_list, device_id) so the caller can update
// last_seen_ms after handling the request.
fn scanner_auth_device(
    headers: &HeaderMap,
    devices: &[Value],
) -> Result<(usize, String), (StatusCode, String)> {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
        .unwrap_or("")
        .trim();
    if token.is_empty() {
        return Err((StatusCode::UNAUTHORIZED, "missing bearer token".into()));
    }
    for (i, d) in devices.iter().enumerate() {
        if d.get("device_token").and_then(|v| v.as_str()) == Some(token) {
            let id = d
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            return Ok((i, id));
        }
    }
    Err((StatusCode::UNAUTHORIZED, "invalid device token".into()))
}

#[derive(serde::Deserialize)]
struct ScanBody {
    #[serde(default)]
    scans: Vec<Value>,
    // Also accept single scan-shaped bodies so a naive client can POST
    // {code, type, ...} without wrapping it.
    #[serde(default)]
    code: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    raw_text: Option<String>,
    #[serde(default)]
    scanned_ms: Option<u64>,
}

async fn post_scanner_scan(
    headers: HeaderMap,
    Json(body): Json<ScanBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut devices = read_json_array(&scanner_devices_file());
    let (idx, device_id) = scanner_auth_device(&headers, &devices)?;
    let now = now_millis() as u64;
    devices[idx]["last_seen_ms"] = Value::Number(serde_json::Number::from(now));
    write_json_array(&scanner_devices_file(), &devices)?;

    let mut incoming: Vec<Value> = body.scans.clone();
    if let Some(code) = body.code {
        if !code.trim().is_empty() {
            incoming.push(json!({
                "code": code,
                "type": body.kind.unwrap_or_default(),
                "confidence": body.confidence,
                "raw_text": body.raw_text,
                "scanned_ms": body.scanned_ms.unwrap_or(now),
            }));
        }
    }
    if incoming.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no scans in body".into()));
    }

    let mut scans = read_json_array(&scanner_scans_file());
    let mut saved: Vec<Value> = Vec::with_capacity(incoming.len());
    for item in incoming.into_iter() {
        let code = item
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if code.is_empty() {
            continue;
        }
        let kind = item
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let confidence = item.get("confidence").cloned().unwrap_or(Value::Null);
        let raw_text = item.get("raw_text").cloned().unwrap_or(Value::Null);
        let scanned_ms = item
            .get("scanned_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(now);
        let entry = json!({
            "id": scanner_new_id("scan"),
            "device_id": device_id,
            "code": code,
            "type": kind,
            "confidence": confidence,
            "raw_text": raw_text,
            "scanned_ms": scanned_ms,
            "received_ms": now,
            "dismissed": false,
        });
        scans.push(entry.clone());
        saved.push(entry);
    }
    write_json_array(&scanner_scans_file(), &scans)?;
    Ok(Json(json!({ "ok": true, "count": saved.len(), "items": saved })))
}

#[derive(serde::Deserialize)]
struct ScansQuery {
    #[serde(default)]
    since: Option<u64>,
    #[serde(default)]
    pending: Option<bool>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn list_scanner_scans(
    Query(q): Query<ScansQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut scans = read_json_array(&scanner_scans_file());
    if let Some(since) = q.since {
        scans.retain(|s| s.get("received_ms").and_then(|v| v.as_u64()).unwrap_or(0) > since);
    }
    if q.pending.unwrap_or(false) {
        scans.retain(|s| !s.get("dismissed").and_then(|v| v.as_bool()).unwrap_or(false));
    }
    // Newest first.
    scans.sort_by(|a, b| {
        let av = a.get("received_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let bv = b.get("received_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        bv.cmp(&av)
    });
    if let Some(limit) = q.limit {
        scans.truncate(limit);
    }
    Ok(Json(json!({ "items": scans })))
}
