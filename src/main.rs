/**
 * Rust Transcription Starter - Backend Server
 *
 * This is a simple HTTP server that provides a transcription API endpoint
 * powered by Deepgram's Speech-to-Text service. It's designed to be easily
 * modified and extended for your own projects.
 *
 * Key Features:
 * - Single API endpoint: POST /api/transcription
 * - Accepts file uploads (multipart/form-data)
 * - CORS enabled for frontend communication
 * - JWT session auth
 * - Pure API server (frontend served separately)
 */

use axum::{
    Router,
    extract::{DefaultBodyLimit, Multipart},
    http::{HeaderMap, Method, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
};
use chrono::Utc;
use deepgram::{
    common::{
        audio_source::AudioSource,
        options::{Language, Model, Options},
    },
    Deepgram, DeepgramError,
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::{env, net::SocketAddr, sync::Arc};
use tower_http::cors::{Any, CorsLayer};

// ============================================================================
// SECTION 1: CONFIGURATION - Customize these values for your needs
// ============================================================================

/**
 * Default transcription model to use when none is specified.
 * Options: "nova-3", "nova-2", "nova", "enhanced", "base"
 * See: https://developers.deepgram.com/docs/models-languages-overview
 */
const DEFAULT_MODEL: &str = "nova-3";

/// Server configuration, overridable via environment variables.
struct Config {
    port: u16,
    host: String,
}

/// Reads PORT and HOST from the environment with sensible defaults.
fn load_config() -> Config {
    let port = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8081);
    let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    Config { port, host }
}

// ============================================================================
// SECTION 2: SESSION AUTH - JWT tokens for production security
// ============================================================================

/// JWT expiry duration in seconds (1 hour).
const JWT_EXPIRY_SECS: i64 = 3600;

/// JWT claims structure for session tokens.
#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    iat: i64,
    exp: i64,
}

/// Shared application state passed to all handlers.
struct AppState {
    session_secret: String,
    api_key: String,
    http_client: reqwest::Client,
}

/// Initialises the session secret from env or generates a random one.
fn init_session_secret() -> String {
    if let Ok(secret) = env::var("SESSION_SECRET") {
        if !secret.is_empty() {
            return secret;
        }
    }
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Middleware that validates a JWT Bearer token on protected routes.
/// Returns 401 JSON error if the token is missing or invalid.
async fn require_session(
    headers: HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let state = request
        .extensions()
        .get::<Arc<AppState>>()
        .cloned();

    let state = match state {
        Some(s) => s,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Internal server error"})),
            )
                .into_response();
        }
    };

    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if auth_header.is_empty() || !auth_header.starts_with("Bearer ") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": {
                    "type": "AuthenticationError",
                    "code": "MISSING_TOKEN",
                    "message": "Authorization header with Bearer token is required"
                }
            })),
        )
            .into_response();
    }

    let token_str = &auth_header[7..];
    let decoding_key = DecodingKey::from_secret(state.session_secret.as_bytes());
    let mut validation = Validation::default();
    validation.required_spec_claims.clear();
    validation.validate_exp = true;

    match decode::<Claims>(token_str, &decoding_key, &validation) {
        Ok(_) => next.run(request).await,
        Err(err) => {
            let msg = if err.to_string().contains("expired") {
                "Session expired, please refresh the page"
            } else {
                "Invalid session token"
            };
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": {
                        "type": "AuthenticationError",
                        "code": "INVALID_TOKEN",
                        "message": msg
                    }
                })),
            )
                .into_response()
        }
    }
}

// ============================================================================
// SECTION 3: API KEY LOADING - Load Deepgram API key from .env
// ============================================================================

/// Reads the Deepgram API key from the environment.
/// Exits with a helpful error message if not found.
fn load_api_key() -> String {
    match env::var("DEEPGRAM_API_KEY") {
        Ok(key) if !key.is_empty() => key,
        _ => {
            eprintln!("\n  ERROR: Deepgram API key not found!\n");
            eprintln!("Please set your API key using one of these methods:\n");
            eprintln!("1. Create a .env file (recommended):");
            eprintln!("   DEEPGRAM_API_KEY=your_api_key_here\n");
            eprintln!("2. Environment variable:");
            eprintln!("   export DEEPGRAM_API_KEY=your_api_key_here\n");
            eprintln!("Get your API key at: https://console.deepgram.com\n");
            std::process::exit(1);
        }
    }
}

// ============================================================================
// SECTION 4: SETUP - Initialize configuration and middleware
// ============================================================================

/// Middleware that injects shared AppState into request extensions so that
/// the require_session middleware can access it.
async fn inject_state(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    request.extensions_mut().insert(state);
    next.run(request).await
}

/// Builds the CORS layer. Wildcard origin is safe because same-origin is
/// enforced via Vite proxy / Caddy in production.
fn build_cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any)
}

// ============================================================================
// SECTION 5: HELPER FUNCTIONS - JSON response utilities
// ============================================================================

/// Builds a structured error envelope suitable for the frontend to display.
fn format_error_response(
    err_msg: &str,
    status_code: u16,
    code: Option<&str>,
) -> serde_json::Value {
    let err_type = if status_code == 400 {
        "ValidationError"
    } else {
        "TranscriptionError"
    };

    let error_code = match code {
        Some(c) => c.to_string(),
        None => {
            if status_code == 400 {
                "MISSING_INPUT".to_string()
            } else {
                "TRANSCRIPTION_FAILED".to_string()
            }
        }
    };

    serde_json::json!({
        "error": {
            "type": err_type,
            "code": error_code,
            "message": err_msg,
            "details": {
                "originalError": err_msg
            }
        }
    })
}

/// Returns the first non-empty string from a slice, or a default value.
fn first_non_empty(vals: &[&str], default: &str) -> String {
    for v in vals {
        if !v.is_empty() {
            return v.to_string();
        }
    }
    default.to_string()
}

// ============================================================================
// SECTION 6: DEEPGRAM API CLIENT - Direct HTTP calls to Deepgram REST API
// ============================================================================

/// Sends audio bytes to Deepgram via the official `deepgram` crate and returns
/// the response as a JSON value (matching the raw `/v1/listen` shape the
/// frontend already consumes).
///
/// The incoming `params` are the same key/value pairs that were previously
/// appended to the request URL; here they are mapped onto the SDK's typed
/// [`Options`] builder. The audio is uploaded as a raw buffer, exactly as
/// before.
async fn call_deepgram_transcription(
    api_key: &str,
    audio_data: Vec<u8>,
    params: &[(String, String)],
) -> Result<serde_json::Value, String> {
    let dg = Deepgram::new(api_key)
        .map_err(|e| format!("Failed to initialize Deepgram client: {}", e))?;

    let lookup = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    };
    let as_bool = |key: &str| lookup(key).map(|v| v == "true");

    let mut builder = Options::builder();
    if let Some(model) = lookup("model") {
        builder = builder.model(Model::from(model.to_string()));
    }
    if let Some(language) = lookup("language") {
        builder = builder.language(Language::from(language.to_string()));
    }
    if let Some(v) = as_bool("smart_format") {
        builder = builder.smart_format(v);
    }
    if let Some(v) = as_bool("punctuate") {
        builder = builder.punctuate(v);
    }
    if let Some(v) = as_bool("diarize") {
        builder = builder.diarize(v);
    }
    if let Some(v) = as_bool("paragraphs") {
        builder = builder.paragraphs(v);
    }
    if let Some(v) = as_bool("utterances") {
        builder = builder.utterances(v);
    }
    if let Some(v) = as_bool("filler_words") {
        builder = builder.filler_words(v);
    }
    let options = builder.build();

    let source = AudioSource::from_buffer_with_mime_type(audio_data, "application/octet-stream");

    let response = dg
        .transcription()
        .prerecorded(source, &options)
        .await
        .map_err(|e| match &e {
            DeepgramError::DeepgramApiError { body, .. } => {
                format!("Deepgram API returned an error: {}", body)
            }
            other => format!("Deepgram API request failed: {}", other),
        })?;

    serde_json::to_value(&response)
        .map_err(|e| format!("Failed to serialize Deepgram response: {}", e))
}

// ============================================================================
// SECTION 7: RESPONSE FORMATTING - Shape Deepgram responses for the frontend
// ============================================================================

/// Extracts the relevant fields from the raw Deepgram API response and
/// returns a simplified structure the frontend expects.
fn format_transcription_response(
    dg_response: &serde_json::Value,
    model_name: &str,
) -> Result<serde_json::Value, String> {
    // Navigate: results -> channels[0] -> alternatives[0]
    let results = dg_response
        .get("results")
        .ok_or("No transcription results returned from Deepgram")?;
    let channels = results
        .get("channels")
        .and_then(|c| c.as_array())
        .ok_or("No transcription results returned from Deepgram")?;
    let channel = channels
        .first()
        .ok_or("No transcription results returned from Deepgram")?;
    let alternatives = channel
        .get("alternatives")
        .and_then(|a| a.as_array())
        .ok_or("No transcription results returned from Deepgram")?;
    let alt = alternatives
        .first()
        .ok_or("No transcription results returned from Deepgram")?;

    // Build metadata from top-level metadata field
    let mut metadata = serde_json::json!({
        "model_name": model_name,
    });
    if let Some(meta) = dg_response.get("metadata") {
        if let Some(v) = meta.get("model_uuid") {
            metadata["model_uuid"] = v.clone();
        }
        if let Some(v) = meta.get("request_id") {
            metadata["request_id"] = v.clone();
        }
    }

    let mut response = serde_json::json!({
        "transcript": alt.get("transcript").cloned().unwrap_or(serde_json::Value::String(String::new())),
        "words": alt.get("words").cloned().unwrap_or(serde_json::json!([])),
        "metadata": metadata,
    });

    // Add optional duration if present
    if let Some(meta) = dg_response.get("metadata") {
        if let Some(dur) = meta.get("duration") {
            response["duration"] = dur.clone();
        }
    }

    Ok(response)
}

// ============================================================================
// SECTION 8: SESSION ROUTES - Auth endpoints (unprotected)
// ============================================================================

/// Issues a signed JWT for session authentication.
/// GET /api/session
async fn handle_session(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    let now = Utc::now().timestamp();
    let claims = Claims {
        iat: now,
        exp: now + JWT_EXPIRY_SECS,
    };

    let encoding_key = EncodingKey::from_secret(state.session_secret.as_bytes());
    match encode(&Header::default(), &claims, &encoding_key) {
        Ok(token) => (StatusCode::OK, Json(serde_json::json!({"token": token}))).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Failed to generate session token"})),
        )
            .into_response(),
    }
}

// ============================================================================
// SECTION 9: API ROUTES - Define your API endpoints here
// ============================================================================

/// Processes audio file uploads and sends them to the Deepgram API for
/// prerecorded transcription.
///
/// POST /api/transcription
///
/// Accepts multipart/form-data with a "file" field.
/// Query params: model, language, smart_format, diarize, punctuate,
///               paragraphs, utterances, filler_words
///
/// Protected by JWT session auth (require_session middleware).
async fn handle_transcription(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    query: axum::extract::Query<std::collections::HashMap<String, String>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    // Read uploaded file from multipart form
    let mut audio_data: Option<Vec<u8>> = None;
    let mut form_fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            match field.bytes().await {
                Ok(bytes) => audio_data = Some(bytes.to_vec()),
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(format_error_response(
                            &format!("Failed to read uploaded file: {}", e),
                            400,
                            Some("MISSING_INPUT"),
                        )),
                    )
                        .into_response();
                }
            }
        } else if let Ok(text) = field.text().await {
            form_fields.insert(name, text);
        }
    }

    // If no file was uploaded, check for a URL field
    let audio_data = if let Some(data) = audio_data.filter(|d| !d.is_empty()) {
        data
    } else if let Some(url) = form_fields.remove("url").filter(|u| !u.is_empty()) {
        // Validate URL format
        if reqwest::Url::parse(&url).is_err() {
            return (
                StatusCode::BAD_REQUEST,
                Json(format_error_response(
                    "Invalid URL format",
                    400,
                    Some("INVALID_URL"),
                )),
            )
                .into_response();
        }
        // Download audio from URL
        match state.http_client.get(&url).timeout(std::time::Duration::from_secs(30)).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.bytes().await {
                    Ok(bytes) => bytes.to_vec(),
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(format_error_response(
                                &format!("Failed to read audio from URL: {}", e),
                                400,
                                Some("URL_FETCH_FAILED"),
                            )),
                        )
                            .into_response();
                    }
                }
            }
            Ok(resp) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(format_error_response(
                        &format!("Failed to fetch audio from URL: HTTP {}", resp.status()),
                        400,
                        Some("URL_FETCH_FAILED"),
                    )),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(format_error_response(
                        &format!("Failed to fetch audio from URL: {}", e),
                        400,
                        Some("URL_FETCH_FAILED"),
                    )),
                )
                    .into_response();
            }
        }
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(format_error_response(
                "Either file or url must be provided",
                400,
                Some("MISSING_INPUT"),
            )),
        )
            .into_response();
    };

    // Build query parameters from request query string and form fields
    let model = first_non_empty(
        &[
            query.get("model").map(|s| s.as_str()).unwrap_or(""),
            form_fields.get("model").map(|s| s.as_str()).unwrap_or(""),
        ],
        DEFAULT_MODEL,
    );

    let language = first_non_empty(
        &[
            query.get("language").map(|s| s.as_str()).unwrap_or(""),
            form_fields.get("language").map(|s| s.as_str()).unwrap_or(""),
        ],
        "en",
    );

    let smart_format = first_non_empty(
        &[
            query.get("smart_format").map(|s| s.as_str()).unwrap_or(""),
            form_fields
                .get("smart_format")
                .map(|s| s.as_str())
                .unwrap_or(""),
        ],
        "true",
    );

    let mut params = vec![
        ("model".to_string(), model.clone()),
        ("language".to_string(), language),
        ("smart_format".to_string(), smart_format),
    ];

    // Optional boolean feature flags
    for key in &["diarize", "punctuate", "paragraphs", "utterances", "filler_words"] {
        let val = first_non_empty(
            &[
                query.get(*key).map(|s| s.as_str()).unwrap_or(""),
                form_fields.get(*key).map(|s| s.as_str()).unwrap_or(""),
            ],
            "",
        );
        if !val.is_empty() {
            params.push((key.to_string(), val));
        }
    }

    // Call Deepgram REST API
    let dg_response =
        match call_deepgram_transcription(&state.api_key, audio_data, &params).await {
            Ok(resp) => resp,
            Err(e) => {
                eprintln!("Transcription error: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(format_error_response(
                        "An error occurred during transcription",
                        500,
                        Some("TRANSCRIPTION_FAILED"),
                    )),
                )
                    .into_response();
            }
        };

    // Format and return response
    match format_transcription_response(&dg_response, &model) {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(e) => {
            eprintln!("Response formatting error: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(format_error_response(
                    &e,
                    500,
                    Some("TRANSCRIPTION_FAILED"),
                )),
            )
                .into_response()
        }
    }
}

/// Reads and returns the [meta] section from deepgram.toml.
/// GET /api/metadata
async fn handle_metadata() -> impl IntoResponse {
    /// Helper struct for parsing deepgram.toml.
    #[derive(Deserialize)]
    struct DeepgramToml {
        meta: Option<toml::Value>,
    }

    let content = match std::fs::read_to_string("deepgram.toml") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error reading deepgram.toml: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "INTERNAL_SERVER_ERROR",
                    "message": "Failed to read metadata from deepgram.toml"
                })),
            )
                .into_response();
        }
    };

    let toml_data: DeepgramToml = match toml::from_str(&content) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error parsing deepgram.toml: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "INTERNAL_SERVER_ERROR",
                    "message": "Failed to read metadata from deepgram.toml"
                })),
            )
                .into_response();
        }
    };

    match toml_data.meta {
        Some(meta) => {
            // Convert TOML Value to JSON Value
            let json_str = serde_json::to_string(&meta).unwrap_or_default();
            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).unwrap_or(serde_json::json!({}));
            (StatusCode::OK, Json(json_val)).into_response()
        }
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "INTERNAL_SERVER_ERROR",
                "message": "Missing [meta] section in deepgram.toml"
            })),
        )
            .into_response(),
    }
}

/// Simple health-check endpoint.
/// GET /health
async fn handle_health() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

// ============================================================================
// SECTION 10: SERVER START
// ============================================================================

#[tokio::main]
async fn main() {
    // Load .env file (ignore error if not present)
    let _ = dotenvy::dotenv();

    // Initialize components
    let cfg = load_config();
    let session_secret = init_session_secret();
    let api_key = load_api_key();

    let state = Arc::new(AppState {
        session_secret,
        api_key,
        http_client: reqwest::Client::new(),
    });

    // Build protected routes (with JWT auth middleware)
    // 50 MB body limit for file uploads (default 2 MB is too small for audio)
    let protected = Router::new()
        .route("/api/transcription", post(handle_transcription))
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
        .layer(middleware::from_fn(require_session));

    // Build unprotected routes
    let app = Router::new()
        .route("/api/session", get(handle_session))
        .route("/api/metadata", get(handle_metadata))
        .route("/health", get(handle_health))
        .merge(protected)
        .layer(middleware::from_fn_with_state(state.clone(), inject_state))
        .layer(build_cors_layer())
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port)
        .parse()
        .expect("Invalid address");

    let separator = "=".repeat(70);
    println!("\n{}", separator);
    println!("  Backend API running at http://localhost:{}", cfg.port);
    println!("  GET  /api/session");
    println!("  POST /api/transcription (auth required)");
    println!("  GET  /api/metadata");
    println!("  GET  /health");
    println!("{}\n", separator);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind address");
    axum::serve(listener, app)
        .await
        .expect("Server failed");
}
