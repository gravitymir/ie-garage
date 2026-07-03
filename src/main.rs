use axum::extract::{DefaultBodyLimit, Json, Path, Query};
use axum::http::{header, HeaderValue, StatusCode};
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
fn autofill_dir() -> PathBuf {
    PathBuf::from("autofill")
}
fn autofill_rules_file() -> PathBuf {
    autofill_dir().join("rules.json")
}
fn settings_dir() -> PathBuf {
    PathBuf::from("settings")
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
    let _ = std::fs::create_dir_all(autofill_dir());
    let _ = std::fs::create_dir_all(settings_dir());
    let _ = std::fs::create_dir_all(chat_dir());
    if !chat_messages_file().exists() {
        let _ = std::fs::write(chat_messages_file(), "[]");
    }
    if !autofill_rules_file().exists() {
        let _ = std::fs::write(autofill_rules_file(), "[]");
    }

    // One-time migration from flat database/*.json to cars/{plate}/jobs/*.json
    migrate_legacy_database();
    // Backfill fuel_type="unknown" on existing cars that don't have one.
    backfill_fuel_type();

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

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3333")
        .await
        .expect("Failed to bind to 0.0.0.0:3333");

    println!("Server running at http://localhost:3333");
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
    photo: Option<String>,
    photo_updated_ms: u128,
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
        let car_json: Value = std::fs::read_to_string(path.join("car.json"))
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
        if jobs_dir.is_dir() {
            if let Ok(job_entries) = std::fs::read_dir(&jobs_dir) {
                for je in job_entries.flatten() {
                    let jp = je.path();
                    if jp.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    job_count += 1;
                    if let Ok(meta) = std::fs::metadata(&jp) {
                        if let Ok(modified) = meta.modified() {
                            let ms = modified
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis();
                            if ms > last_job_ms {
                                last_job_ms = ms;
                            }
                        }
                    }
                }
            }
        }

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
        });
    }
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
        let displacement = extract_displacement(&variant_name);
        // Engine summary is built once per car (computed once, used for every
        // job of that car below).
        let engine_summary = build_engine_summary(&car_path);

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
    let mut json: Value = serde_json::from_str(&content)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    // Never expose the password to the client. Replace with a boolean marker.
    if let Some(obj) = json.as_object_mut() {
        let has_pw = obj
            .get("password")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        obj.remove("password");
        obj.insert("has_password".to_string(), Value::Bool(has_pw));
    }
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
        // Password: empty / missing means "keep existing"; non-empty replaces.
        // (Never echoed back by get_worker, so the form sends empty unless user retyped it.)
        let incoming = obj
            .get("password")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        match incoming {
            Some(s) if !s.is_empty() => {
                obj.insert("password".to_string(), Value::String(s));
            }
            _ => {
                if let Some(existing_pw) = existing.get("password") {
                    obj.insert("password".to_string(), existing_pw.clone());
                } else {
                    obj.remove("password");
                }
            }
        }
        // Strip the UI-only marker if the frontend sent it back.
        obj.remove("has_password");
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
        // Chat notifications — per-event toggles. Defaults to "everything on"
        // so new installs get the full workshop feed; users can mute noise
        // via Settings → Chat.
        "chat_notify_job_started": true,
        // "job_finished" historically covered Work done — keep the key so
        // pre-existing settings.json files still gate that event.
        "chat_notify_job_finished": true,
        // Split off from finished so QC-returned jobs and delivered ones can
        // be muted independently. Default true so a fresh install sees the
        // full flow.
        "chat_notify_job_reopened": true,
        "chat_notify_job_closed": true,
        "chat_notify_stock_arrival": true,
        "chat_notify_low_stock": true,
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
    Ok(Json(json!({ "items": items })))
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
        "text": text,
    });
    let saved = append_chat_entry(entry)?;
    Ok(Json(json!({ "ok": true, "item": saved })))
}
