use axum::extract::{Json, Query};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use regex::Regex;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

#[derive(serde::Deserialize)]
struct StepAQuery {
    plate: String,
}

#[derive(serde::Deserialize)]
struct StepBQuery {
    plate: String,
    token: String,
}

/// Base directory for public assets and database: project root when running
/// from target/release or target/debug, otherwise the executable's directory.
fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .and_then(|exe_dir| {
            let name = exe_dir.file_name().and_then(|n| n.to_str())?;
            if (name == "release" || name == "debug")
                && exe_dir.parent().and_then(|p| p.file_name().and_then(|n| n.to_str())) == Some("target")
            {
                exe_dir.join("..").join("..").canonicalize().ok()
            } else {
                Some(exe_dir)
            }
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

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

    // One-time migration: copy saved documents from target/release/database (or target/debug) into project root database
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let old_db = exe_dir.join("database");
            let new_db = base.join("database");
            if old_db.is_dir() && new_db != old_db {
                let _ = std::fs::create_dir_all(&new_db);
                if let Ok(entries) = std::fs::read_dir(&old_db) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|e| e.to_str()) == Some("json") {
                            if let Some(name) = path.file_name() {
                                let dest = new_db.join(name);
                                let _ = std::fs::copy(&path, &dest);
                            }
                        }
                    }
                }
            }
        }
    }

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(|| async { Redirect::temporary("/job.html") }))
        .route("/api/step-a", get(step_a))
        .route("/api/step-b", get(step_b))
        .route("/api/save", post(save_doc))
        .route("/api/list", get(list_docs))
        .route("/api/load", get(load_doc))
        .fallback_service(ServeDir::new(public_dir))
        .layer(cors);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("Failed to bind to 0.0.0.0:3000");

    println!("Server running at http://0.0.0.0:3000");
    axum::serve(listener, app).await.expect("server error");
}

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

#[derive(serde::Deserialize)]
struct LoadQuery {
    file: String,
}

#[derive(Serialize)]
struct ListItem {
    name: String,
    modified_ms: u128,
    date_in: String,
    reg_no: String,
    make: String,
    model: String,
}

fn database_dir() -> PathBuf {
    PathBuf::from("database")
}

fn sanitize_filename(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect();
    if cleaned.is_empty() {
        "job".to_string()
    } else {
        cleaned
    }
}

async fn save_doc(Json(payload): Json<Value>) -> Result<Json<Value>, (StatusCode, String)> {
    let dir = database_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let reg_no = payload
        .get("vehicle")
        .and_then(|v| v.get("reg_no"))
        .and_then(|v| v.as_str())
        .unwrap_or("job");

    let suggested = format!("{}-{}", reg_no, now_millis());
    let file_name = payload
        .get("file_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or(suggested);

    let mut file_name = sanitize_filename(&file_name);
    if !file_name.to_lowercase().ends_with(".json") {
        file_name.push_str(".json");
    }

    let path = dir.join(&file_name);
    let pretty = serde_json::to_string_pretty(&payload)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(&path, pretty)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "file_name": file_name
    })))
}

async fn list_docs() -> Result<Json<Value>, (StatusCode, String)> {
    let dir = database_dir();
    if !dir.exists() {
        return Ok(Json(serde_json::json!({ "items": [] })));
    }
    let mut items: Vec<ListItem> = Vec::new();
    let entries =
        std::fs::read_dir(&dir).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let meta =
            std::fs::metadata(&path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let modified = meta
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let json: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
        let date_in = json
            .get("date_in")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let vehicle = json.get("vehicle").and_then(|v| v.as_object());
        let reg_no = vehicle
            .and_then(|v| v.get("reg_no"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let make = vehicle
            .and_then(|v| v.get("make"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let model = vehicle
            .and_then(|v| v.get("model"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown.json")
            .to_string();
        items.push(ListItem {
            name,
            modified_ms: modified,
            date_in,
            reg_no,
            make,
            model,
        });
    }
    items.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms));
    Ok(Json(serde_json::json!({ "items": items })))
}

async fn load_doc(Query(params): Query<LoadQuery>) -> Result<Json<Value>, (StatusCode, String)> {
    let name = sanitize_filename(&params.file);
    let file_name = if name.to_lowercase().ends_with(".json") {
        name
    } else {
        format!("{name}.json")
    };
    let path = database_dir().join(&file_name);
    let content =
        std::fs::read_to_string(&path).map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let json: Value =
        serde_json::from_str(&content).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(json))
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
    let token = re
        .captures(&html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| {
            (
                StatusCode::BAD_GATEWAY,
                "csrf token not found".to_string(),
            )
        })?;

    Ok(token)
}

async fn proxy_json(client: &Client, url: &str, csrf: &str) -> Result<Response, (StatusCode, String)> {
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

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
