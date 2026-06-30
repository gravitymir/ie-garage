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
fn parts_dir() -> PathBuf {
    PathBuf::from("parts")
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
fn autofill_dir() -> PathBuf {
    PathBuf::from("autofill")
}
fn autofill_rules_file() -> PathBuf {
    autofill_dir().join("rules.json")
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
    let _ = std::fs::create_dir_all(parts_dir());
    let _ = std::fs::create_dir_all(workers_dir());
    let _ = std::fs::create_dir_all(store_items_dir());
    // Seed empty divisions file if missing
    if !store_divisions_file().exists() {
        let _ = std::fs::write(store_divisions_file(), "[]");
    }
    let _ = std::fs::create_dir_all(autofill_dir());
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
        // parts
        .route("/api/parts", get(list_parts).post(create_part))
        .route(
            "/api/parts/:id",
            get(get_part).put(update_part).delete(delete_part),
        )
        .route("/api/parts/:id/images", post(upload_part_image))
        .route(
            "/api/parts/:id/images/:filename",
            delete(delete_part_image),
        )
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
        // serve car + part + store images
        .nest_service("/cars-files", ServeDir::new("cars"))
        .nest_service("/parts-files", ServeDir::new("parts"))
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
        items.push(JobItem {
            name,
            saved_ms,
            date_in,
            time_in,
            time_out,
            work_summary,
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
            });
        }
    }
    items.sort_by(|a, b| b.last_active_ms.cmp(&a.last_active_ms));
    Ok(Json(json!({ "items": items })))
}

// ---------- parts catalog ----------

#[derive(Serialize)]
struct PartSummary {
    id: String,
    name: String,
    number: String,
    description: String,
    notes: String,
    images: Vec<String>,
    created_ms: u128,
    updated_ms: u128,
}

fn read_part_dir(dir: &std::path::Path) -> Option<PartSummary> {
    let id = dir.file_name()?.to_str()?.to_string();
    let part_json: Value = std::fs::read_to_string(dir.join("part.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())?;
    let name = part_json
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let number = part_json
        .get("number")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = part_json
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let notes = part_json
        .get("notes")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let created_ms = part_json
        .get("created_ms")
        .and_then(|v| v.as_u64())
        .map(|n| n as u128)
        .unwrap_or(0);
    let updated_ms = part_json
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
    Some(PartSummary {
        id,
        name,
        number,
        description,
        notes,
        images,
        created_ms,
        updated_ms,
    })
}

async fn list_parts() -> Result<Json<Value>, (StatusCode, String)> {
    let dir = parts_dir();
    if !dir.exists() {
        return Ok(Json(json!({ "items": [] })));
    }
    let mut items: Vec<PartSummary> = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .flatten()
    {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        if let Some(summary) = read_part_dir(&p) {
            items.push(summary);
        }
    }
    items.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
    Ok(Json(json!({ "items": items })))
}

async fn get_part(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = parts_dir().join(&id);
    let summary = read_part_dir(&dir)
        .ok_or((StatusCode::NOT_FOUND, "part not found".to_string()))?;
    Ok(Json(serde_json::to_value(summary).unwrap_or(Value::Null)))
}

async fn create_part(
    Json(mut payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = format!("p-{}", now_millis());
    let dir = parts_dir().join(&id);
    std::fs::create_dir_all(dir.join("images"))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
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
    std::fs::write(dir.join("part.json"), pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true, "id": id })))
}

async fn update_part(
    Path(id): Path<String>,
    Json(mut payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = parts_dir().join(&id);
    if !dir.exists() {
        return Err((StatusCode::NOT_FOUND, "part not found".to_string()));
    }
    // Preserve created_ms from existing file
    let existing: Value = std::fs::read_to_string(dir.join("part.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let created_ms = existing
        .get("created_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| now_millis() as u64);
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
    std::fs::write(dir.join("part.json"), pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true, "id": id })))
}

async fn delete_part(Path(id): Path<String>) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = parts_dir().join(&id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn upload_part_image(
    Path(id): Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let dir = parts_dir().join(&id).join("images");
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

    // Strip data URL prefix if present
    let b64_clean = if let Some(idx) = data_b64.find("base64,") {
        &data_b64[idx + 7..]
    } else {
        data_b64
    };

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_clean.trim())
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    // Make filename unique with timestamp prefix
    let safe = sanitize_filename(raw_name);
    let safe = if safe.is_empty() { "image.png".to_string() } else { safe };
    let file_name = format!("{}-{}", now_millis(), safe);

    std::fs::write(dir.join(&file_name), bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Bump part updated_ms
    let part_json_path = parts_dir().join(&id).join("part.json");
    if let Ok(content) = std::fs::read_to_string(&part_json_path) {
        if let Ok(mut v) = serde_json::from_str::<Value>(&content) {
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "updated_ms".to_string(),
                    Value::Number(serde_json::Number::from(now_millis() as u64)),
                );
                let _ = std::fs::write(
                    &part_json_path,
                    serde_json::to_string_pretty(&v).unwrap_or(content),
                );
            }
        }
    }

    Ok(Json(json!({ "ok": true, "filename": file_name })))
}

async fn delete_part_image(
    Path((id, filename)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = sanitize_filename(&id);
    let filename = sanitize_filename(&filename);
    let path = parts_dir().join(&id).join("images").join(&filename);
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}

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
            }
            d
        })
        .collect();
    Ok(Json(json!({ "items": enriched })))
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
    let new_name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if new_name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name is required".to_string()));
    }
    let mut list = read_divisions();
    let mut found = false;
    for d in list.iter_mut() {
        if d.get("id").and_then(|v| v.as_str()) == Some(&id) {
            if let Some(obj) = d.as_object_mut() {
                obj.insert("name".to_string(), Value::String(new_name.clone()));
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
