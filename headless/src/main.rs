use actix_multipart::Multipart;
use actix_web::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use actix_web::web::Bytes;
use actix_web::{delete, get, post, put, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use constant_time_eq::constant_time_eq;
use futures::{StreamExt, TryStreamExt};
use hmac::{Hmac, Mac};
use grammers_client::types::{Media, PasswordToken, Peer};
use grammers_client::{Client, InputMessage};
use grammers_mtsender::{ConnectionParams, InvocationError, SenderPool};
use grammers_session::storages::SqliteSession;
use grammers_session::types::{PeerAuth, PeerInfo, UpdateState, UpdatesState};
use grammers_session::Session;
use grammers_tl_types as tl;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};
use tokio_util::io::StreamReader;

const TELEGRAM_MAX_FILE_SIZE: u64 = 2_000_000_000;
const MAX_MULTIPART_METADATA_BYTES: usize = 128;
const MAX_UPLOAD_FILENAME_CHARS: usize = 255;
const MAX_DIRECT_CHUNK_BYTES: u64 = 64 * 1024 * 1024;
const CDN_ALIGNMENT: u64 = 524_288;
const DOWNLOAD_CHUNK_SIZE: i32 = 65_536;
const PLAYBACK_SEGMENT_SIZE: u64 = 64 * 1024 * 1024;
const PLAYBACK_ORIGIN_DERIVATION_CONTEXT: &[u8] = b"drama-playback-origin-v1";

#[derive(Clone)]
struct AppState {
    client: Client,
    api_key: Arc<String>,
    playback_origin_key: Arc<String>,
    playback_registry_path: Arc<PathBuf>,
    playback_registry: Arc<RwLock<HashMap<String, PlaybackAsset>>>,
    peer_cache: Arc<RwLock<HashMap<i64, Peer>>>,
}

struct TelegramConnection {
    client: Client,
    session: Arc<SqliteSession>,
}

#[derive(Serialize)]
struct HealthResponse<'a> {
    status: &'a str,
    version: &'a str,
    telegram: &'a str,
}

#[derive(Serialize)]
struct ApiError {
    error: ApiErrorDetail,
}

#[derive(Serialize)]
struct ApiErrorDetail {
    code: String,
    message: String,
}

#[derive(Serialize, Clone)]
struct ApiFile {
    id: i64,
    folder_id: Option<i64>,
    name: String,
    size: u64,
    mime_type: Option<String>,
    created_at: String,
}

#[derive(Serialize)]
struct FilesResponse {
    data: Vec<ApiFile>,
    files: Vec<ApiFile>,
    total: usize,
}

#[derive(Deserialize)]
struct FilesQuery {
    folder_id: Option<i64>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct FolderQuery {
    folder_id: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct PlaybackAssetPart {
    message_id: i32,
    folder_id: Option<i64>,
    size_bytes: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct PlaybackAsset {
    size_bytes: u64,
    mime_type: String,
    etag: String,
    parts: Vec<PlaybackAssetPart>,
}

#[derive(Deserialize)]
struct PlaybackAssetInput {
    size_bytes: u64,
    mime_type: String,
    etag: String,
    parts: Vec<PlaybackAssetPart>,
}

#[derive(Serialize)]
struct PlaybackAssetResponse {
    asset_id: String,
    size_bytes: u64,
    segment_size: u64,
    segment_count: u64,
}

fn json_error(code: impl Into<String>, message: impl Into<String>, status: u16) -> HttpResponse {
    let body = ApiError {
        error: ApiErrorDetail {
            code: code.into(),
            message: message.into(),
        },
    };
    HttpResponse::build(
        actix_web::http::StatusCode::from_u16(status)
            .unwrap_or(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR),
    )
    .json(body)
}

fn require_auth(req: &HttpRequest, state: &AppState) -> Result<(), HttpResponse> {
    let provided = req
        .headers()
        .get("X-API-Key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if provided.len() == state.api_key.len()
        && constant_time_eq(provided.as_bytes(), state.api_key.as_bytes())
    {
        Ok(())
    } else {
        Err(json_error("UNAUTHORIZED", "Invalid API key", 401))
    }
}

fn derive_playback_origin_key(api_key: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(api_key.as_bytes())
        .expect("HMAC accepts arbitrary key sizes");
    mac.update(PLAYBACK_ORIGIN_DERIVATION_CONTEXT);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn require_playback_origin(req: &HttpRequest, state: &AppState) -> Result<(), HttpResponse> {
    let provided = req
        .headers()
        .get("X-Playback-Origin-Key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if provided.len() == state.playback_origin_key.len()
        && constant_time_eq(provided.as_bytes(), state.playback_origin_key.as_bytes())
    {
        Ok(())
    } else {
        Err(json_error("UNAUTHORIZED", "Invalid playback origin key", 401))
    }
}

fn valid_asset_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_playback_asset(input: PlaybackAssetInput) -> Result<PlaybackAsset, String> {
    if input.size_bytes == 0 {
        return Err("Playback asset must be non-empty".to_string());
    }
    if input.mime_type.trim().to_ascii_lowercase() != "video/mp4" {
        return Err("Playback asset must use video/mp4".to_string());
    }
    if input.etag.trim().is_empty() || input.etag.len() > 160 {
        return Err("Playback asset ETag is invalid".to_string());
    }
    if input.parts.is_empty() {
        return Err("Playback asset has no Telegram parts".to_string());
    }
    let mut total = 0u64;
    for part in &input.parts {
        if part.message_id <= 0 || part.size_bytes == 0 {
            return Err("Playback asset contains an invalid Telegram part".to_string());
        }
        total = total
            .checked_add(part.size_bytes)
            .ok_or_else(|| "Playback asset size overflows".to_string())?;
    }
    if total != input.size_bytes {
        return Err("Playback asset part sizes do not match the declared size".to_string());
    }
    Ok(PlaybackAsset {
        size_bytes: input.size_bytes,
        mime_type: "video/mp4".to_string(),
        etag: input.etag.trim().to_string(),
        parts: input.parts,
    })
}

fn playback_registry_path() -> PathBuf {
    env::var("TELEGRAM_PLAYBACK_REGISTRY_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/playback-registry.json"))
}

async fn load_playback_registry(path: &Path) -> Result<HashMap<String, PlaybackAsset>, String> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(error) => Err(error.to_string()),
    }
}

async fn persist_playback_registry(
    path: &Path,
    registry: &HashMap<String, PlaybackAsset>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| error.to_string())?;
    }
    let payload = serde_json::to_vec(registry).map_err(|error| error.to_string())?;
    let temp_path = path.with_extension("json.tmp");
    tokio::fs::write(&temp_path, payload)
        .await
        .map_err(|error| error.to_string())?;
    tokio::fs::rename(&temp_path, path)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn playback_segment_bounds(size_bytes: u64, segment_index: u64) -> Option<(u64, u64)> {
    let start = segment_index.checked_mul(PLAYBACK_SEGMENT_SIZE)?;
    if start >= size_bytes {
        return None;
    }
    let end = start
        .saturating_add(PLAYBACK_SEGMENT_SIZE - 1)
        .min(size_bytes - 1);
    Some((start, end))
}

fn media_size(media: &Media) -> u64 {
    let raw = match media {
        Media::Document(document) => document.size(),
        Media::Photo(photo) => photo.size(),
        _ => 0,
    };
    u64::try_from(raw).unwrap_or(0)
}

fn sanitise_upload_filename(value: &str) -> String {
    let basename = value.rsplit(['/', '\\']).next().unwrap_or(value).trim();
    let cleaned: String = basename
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_UPLOAD_FILENAME_CHARS)
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "file".to_string()
    } else {
        cleaned
    }
}

fn required_content_length(req: &HttpRequest) -> Result<u64, HttpResponse> {
    let raw = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| json_error("CONTENT_LENGTH_REQUIRED", "Content-Length is required", 411))?;
    let size = raw
        .parse::<u64>()
        .map_err(|_| json_error("INVALID_CONTENT_LENGTH", "Content-Length is invalid", 400))?;
    if size == 0 {
        return Err(json_error("FILE_REQUIRED", "A non-empty chunk is required", 400));
    }
    if size > MAX_DIRECT_CHUNK_BYTES {
        return Err(json_error(
            "CHUNK_TOO_LARGE",
            format!("Direct upload chunks must be at most {MAX_DIRECT_CHUNK_BYTES} bytes"),
            413,
        ));
    }
    Ok(size)
}

fn direct_chunk_filename(req: &HttpRequest) -> String {
    req.headers()
        .get("X-File-Name")
        .and_then(|value| value.to_str().ok())
        .map(sanitise_upload_filename)
        .unwrap_or_else(|| "video.part".to_string())
}

fn api_file_from_message(
    message: &grammers_client::types::Message,
    folder_id: Option<i64>,
) -> Option<ApiFile> {
    let media = message.media()?;
    let (name, mime_type) = match &media {
        Media::Document(document) => (
            document.name().to_string(),
            document.mime_type().map(ToOwned::to_owned),
        ),
        Media::Photo(_) => ("Photo.jpg".to_string(), Some("image/jpeg".to_string())),
        _ => ("Unknown".to_string(), None),
    };
    Some(ApiFile {
        id: i64::from(message.id()),
        folder_id,
        name,
        size: media_size(&media),
        mime_type,
        created_at: message.date().to_string(),
    })
}

async fn resolve_peer(
    client: &Client,
    folder_id: Option<i64>,
    peer_cache: &Arc<RwLock<HashMap<i64, Peer>>>,
) -> Result<Peer, String> {
    let Some(folder_id) = folder_id else {
        return client
            .get_me()
            .await
            .map(Peer::User)
            .map_err(|error| error.to_string());
    };

    if let Some(peer) = peer_cache.read().await.get(&folder_id).cloned() {
        return Ok(peer);
    }

    let mut cache = peer_cache.write().await;
    if let Some(peer) = cache.get(&folder_id).cloned() {
        return Ok(peer);
    }

    let mut dialogs = client.iter_dialogs();
    while let Some(dialog) = dialogs.next().await.map_err(|error| error.to_string())? {
        let peer_id = match &dialog.peer {
            Peer::Channel(channel) => Some(channel.raw.id),
            Peer::User(user) => Some(user.raw.id()),
            _ => None,
        };
        if let Some(peer_id) = peer_id {
            cache.insert(peer_id, dialog.peer.clone());
            if peer_id == folder_id {
                return Ok(dialog.peer);
            }
        }
    }

    Err(format!("Folder/Chat {folder_id} not found"))
}

#[get("/api/v1/health")]
async fn health(state: web::Data<AppState>) -> impl Responder {
    match state.client.get_me().await {
        Ok(_) => HttpResponse::Ok().json(HealthResponse {
            status: "ok",
            version: env!("CARGO_PKG_VERSION"),
            telegram: "connected",
        }),
        Err(error) => json_error(
            "NOT_CONNECTED",
            format!("Telegram connection is unavailable: {error}"),
            503,
        ),
    }
}

#[get("/api/v1/files")]
async fn list_files(
    req: HttpRequest,
    query: web::Query<FilesQuery>,
    state: web::Data<AppState>,
) -> impl Responder {
    if let Err(response) = require_auth(&req, state.get_ref()) {
        return response;
    }
    let peer = match resolve_peer(&state.client, query.folder_id, &state.peer_cache).await {
        Ok(peer) => peer,
        Err(error) => return json_error("PEER_ERROR", error, 400),
    };
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let mut messages = state.client.iter_messages(&peer).limit(limit);
    let mut files = Vec::new();
    while let Some(message) = messages.next().await.ok().flatten() {
        if let Some(file) = api_file_from_message(&message, query.folder_id) {
            files.push(file);
        }
    }
    HttpResponse::Ok().json(FilesResponse {
        data: files.clone(),
        files: files.clone(),
        total: files.len(),
    })
}

#[get("/api/v1/files/{message_id}")]
async fn get_file(
    req: HttpRequest,
    path: web::Path<i64>,
    query: web::Query<FolderQuery>,
    state: web::Data<AppState>,
) -> impl Responder {
    if let Err(response) = require_auth(&req, state.get_ref()) {
        return response;
    }
    let peer = match resolve_peer(&state.client, query.folder_id, &state.peer_cache).await {
        Ok(peer) => peer,
        Err(error) => return json_error("PEER_ERROR", error, 400),
    };
    let message_id = match i32::try_from(path.into_inner()) {
        Ok(value) if value > 0 => value,
        _ => return json_error("INVALID_MESSAGE_ID", "Invalid message id", 400),
    };
    match state.client.get_messages_by_id(peer, &[message_id]).await {
        Ok(messages) => messages
            .first()
            .and_then(|message| message.as_ref())
            .and_then(|message| api_file_from_message(message, query.folder_id))
            .map(|file| HttpResponse::Ok().json(file))
            .unwrap_or_else(|| json_error("NOT_FOUND", "File not found", 404)),
        Err(error) => json_error("FETCH_ERROR", error.to_string(), 502),
    }
}

#[post("/api/v1/files")]
async fn upload_file(
    req: HttpRequest,
    mut payload: Multipart,
    state: web::Data<AppState>,
) -> impl Responder {
    if let Err(response) = require_auth(&req, state.get_ref()) {
        return response;
    }

    let temp = match tempfile::NamedTempFile::new() {
        Ok(file) => file,
        Err(error) => return json_error("TEMP_FILE_CREATE_FAILED", error.to_string(), 500),
    };
    let mut output = match temp.reopen() {
        Ok(file) => tokio::fs::File::from_std(file),
        Err(error) => return json_error("TEMP_FILE_OPEN_FAILED", error.to_string(), 500),
    };

    let mut filename = "file".to_string();
    let mut folder_id: Option<i64> = None;
    let mut file_seen = false;
    let mut folder_seen = false;
    let mut file_size = 0_u64;

    loop {
        let mut field = match payload.try_next().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return json_error("INVALID_MULTIPART", error.to_string(), 400),
        };
        let disposition = field.content_disposition();
        let name = disposition.and_then(|value| value.get_name()).unwrap_or("");

        if name == "file" {
            if file_seen {
                return json_error("MULTIPLE_FILES", "Upload exactly one file", 400);
            }
            file_seen = true;
            if let Some(value) = disposition.and_then(|value| value.get_filename()) {
                filename = sanitise_upload_filename(value);
            }
            while let Some(chunk) = field.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => return json_error("READ_ERROR", error.to_string(), 400),
                };
                file_size = match file_size.checked_add(chunk.len() as u64) {
                    Some(size) if size <= TELEGRAM_MAX_FILE_SIZE => size,
                    _ => {
                        return json_error(
                            "FILE_TOO_LARGE",
                            "File exceeds Telegram's 2,000,000,000-byte upload limit",
                            413,
                        )
                    }
                };
                if let Err(error) = output.write_all(&chunk).await {
                    return json_error("WRITE_ERROR", error.to_string(), 500);
                }
            }
        } else if name == "folder_id" {
            if folder_seen {
                return json_error("DUPLICATE_FOLDER_ID", "folder_id may be provided once", 400);
            }
            folder_seen = true;
            let mut bytes = Vec::new();
            while let Some(chunk) = field.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => return json_error("READ_ERROR", error.to_string(), 400),
                };
                if bytes.len().saturating_add(chunk.len()) > MAX_MULTIPART_METADATA_BYTES {
                    return json_error("INVALID_FOLDER_ID", "folder_id is too long", 400);
                }
                bytes.extend_from_slice(&chunk);
            }
            let raw = String::from_utf8_lossy(&bytes).trim().to_string();
            if !raw.is_empty() && raw != "null" && raw != "none" {
                folder_id = match raw.parse::<i64>() {
                    Ok(value) => Some(value),
                    Err(_) => return json_error("INVALID_FOLDER_ID", "folder_id must be an integer", 400),
                };
            }
        } else {
            return json_error(
                "UNKNOWN_MULTIPART_FIELD",
                "Only file and folder_id fields are accepted",
                400,
            );
        }
    }

    if !file_seen || file_size == 0 {
        return json_error("FILE_REQUIRED", "A non-empty multipart file is required", 400);
    }
    if let Err(error) = output.flush().await {
        return json_error("WRITE_ERROR", error.to_string(), 500);
    }
    drop(output);

    let peer = match resolve_peer(&state.client, folder_id, &state.peer_cache).await {
        Ok(peer) => peer,
        Err(error) => return json_error("PEER_ERROR", error, 400),
    };
    let mut source = match tokio::fs::File::open(temp.path()).await {
        Ok(file) => file,
        Err(error) => return json_error("OPEN_ERROR", error.to_string(), 500),
    };
    let uploaded = match state
        .client
        .upload_stream(&mut source, file_size as usize, filename.clone())
        .await
    {
        Ok(uploaded) => uploaded,
        Err(error) => return json_error("UPLOAD_FAILED", error.to_string(), 502),
    };
    let message = InputMessage::new().text("").file(uploaded);
    match state.client.send_message(&peer, message).await {
        Ok(message) => match api_file_from_message(&message, folder_id) {
            Some(file) => HttpResponse::Ok().json(file),
            None => json_error("UPLOAD_INCOMPLETE", "Telegram returned no media message", 502),
        },
        Err(error) => json_error("SEND_FAILED", error.to_string(), 502),
    }
}

#[post("/api/v1/files/chunks")]
async fn upload_chunk(
    req: HttpRequest,
    query: web::Query<FolderQuery>,
    payload: web::Payload,
    state: web::Data<AppState>,
) -> impl Responder {
    if let Err(response) = require_auth(&req, state.get_ref()) {
        return response;
    }
    let file_size = match required_content_length(&req) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let filename = direct_chunk_filename(&req);
    let folder_id = query.folder_id;
    let peer = match resolve_peer(&state.client, folder_id, &state.peer_cache).await {
        Ok(peer) => peer,
        Err(error) => return json_error("PEER_ERROR", error, 400),
    };

    let stream = payload.map(|item| {
        item.map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))
    });
    let mut reader = StreamReader::new(stream);
    let uploaded = match state
        .client
        .upload_stream(&mut reader, file_size as usize, filename.clone())
        .await
    {
        Ok(uploaded) => uploaded,
        Err(error) => return json_error("UPLOAD_FAILED", error.to_string(), 502),
    };
    let message = InputMessage::new().text("").file(uploaded);
    match state.client.send_message(&peer, message).await {
        Ok(message) => match api_file_from_message(&message, folder_id) {
            Some(file) if file.size == file_size => HttpResponse::Ok().json(file),
            Some(file) => {
                let _ = state.client.delete_messages(&peer, &[message.id()]).await;
                json_error(
                    "SIZE_MISMATCH",
                    format!("Telegram stored {} bytes, expected {file_size}", file.size),
                    502,
                )
            }
            None => json_error("UPLOAD_INCOMPLETE", "Telegram returned no media message", 502),
        },
        Err(error) => json_error("SEND_FAILED", error.to_string(), 502),
    }
}

#[put("/api/v1/playback-assets/{asset_id}")]
async fn put_playback_asset(
    req: HttpRequest,
    path: web::Path<String>,
    body: web::Json<PlaybackAssetInput>,
    state: web::Data<AppState>,
) -> impl Responder {
    if let Err(response) = require_auth(&req, state.get_ref()) {
        return response;
    }
    let asset_id = path.into_inner();
    if !valid_asset_id(&asset_id) {
        return json_error("INVALID_ASSET_ID", "Playback asset id is invalid", 400);
    }
    let asset = match validate_playback_asset(body.into_inner()) {
        Ok(asset) => asset,
        Err(error) => return json_error("INVALID_PLAYBACK_ASSET", error, 422),
    };
    let response = PlaybackAssetResponse {
        asset_id: asset_id.clone(),
        size_bytes: asset.size_bytes,
        segment_size: PLAYBACK_SEGMENT_SIZE,
        segment_count: asset.size_bytes.div_ceil(PLAYBACK_SEGMENT_SIZE),
    };
    let mut registry = state.playback_registry.write().await;
    registry.insert(asset_id, asset);
    if let Err(error) = persist_playback_registry(&state.playback_registry_path, &registry).await {
        return json_error("REGISTRY_WRITE_FAILED", error, 500);
    }
    HttpResponse::Ok().json(response)
}

#[delete("/api/v1/playback-assets/{asset_id}")]
async fn delete_playback_asset(
    req: HttpRequest,
    path: web::Path<String>,
    state: web::Data<AppState>,
) -> impl Responder {
    if let Err(response) = require_auth(&req, state.get_ref()) {
        return response;
    }
    let asset_id = path.into_inner();
    if !valid_asset_id(&asset_id) {
        return json_error("INVALID_ASSET_ID", "Playback asset id is invalid", 400);
    }
    let mut registry = state.playback_registry.write().await;
    registry.remove(&asset_id);
    if let Err(error) = persist_playback_registry(&state.playback_registry_path, &registry).await {
        return json_error("REGISTRY_WRITE_FAILED", error, 500);
    }
    HttpResponse::Ok().json(serde_json::json!({"success": true}))
}

#[delete("/api/v1/files/{message_id}")]
async fn delete_file(
    req: HttpRequest,
    path: web::Path<i64>,
    query: web::Query<FolderQuery>,
    state: web::Data<AppState>,
) -> impl Responder {
    if let Err(response) = require_auth(&req, state.get_ref()) {
        return response;
    }
    let message_id = match i32::try_from(path.into_inner()) {
        Ok(value) if value > 0 => value,
        _ => return json_error("INVALID_MESSAGE_ID", "Invalid message id", 400),
    };
    let peer = match resolve_peer(&state.client, query.folder_id, &state.peer_cache).await {
        Ok(peer) => peer,
        Err(error) => return json_error("PEER_ERROR", error, 400),
    };
    match state.client.delete_messages(&peer, &[message_id]).await {
        Ok(_) => HttpResponse::Ok().json(serde_json::json!({"success": true})),
        Err(error) => json_error("DELETE_FAILED", error.to_string(), 502),
    }
}

fn parse_range_header(value: &str, total_size: u64) -> Option<(u64, u64)> {
    if total_size == 0 || !value.starts_with("bytes=") || value.contains(',') {
        return None;
    }
    let range = value.trim_start_matches("bytes=");
    let parts: Vec<&str> = range.splitn(2, '-').collect();
    if parts.len() != 2 || parts[0].trim().is_empty() {
        return None;
    }
    let start = parts[0].trim().parse::<u64>().ok()?;
    if start >= total_size {
        return None;
    }
    let end = if parts[1].trim().is_empty() {
        total_size - 1
    } else {
        parts[1]
            .trim()
            .parse::<u64>()
            .ok()?
            .min(total_size - 1)
    };
    (start <= end).then_some((start, end))
}

fn build_media_response(
    client: &Client,
    media: &Media,
    req: &HttpRequest,
    mime: &str,
    filename: &str,
) -> HttpResponse {
    let size = media_size(media);
    if size == 0 {
        return json_error("EMPTY_MEDIA", "Telegram media is empty", 422);
    }

    let requested_range = req
        .headers()
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_range_header(value, size));
    let is_range = req.headers().contains_key(RANGE);
    if is_range && requested_range.is_none() {
        return HttpResponse::RangeNotSatisfiable()
            .insert_header((CONTENT_RANGE, format!("bytes */{size}")))
            .finish();
    }
    let (start_byte, end_byte) = requested_range.unwrap_or((0, size - 1));
    let content_length = end_byte - start_byte + 1;

    let mut download = client.iter_download(media).chunk_size(DOWNLOAD_CHUNK_SIZE);
    let mut bytes_to_skip = 0usize;
    if start_byte > 0 {
        let aligned_start = (start_byte / CDN_ALIGNMENT) * CDN_ALIGNMENT;
        let chunk_index = (aligned_start / DOWNLOAD_CHUNK_SIZE as u64) as i32;
        if chunk_index > 0 {
            download = download.skip_chunks(chunk_index);
        }
        bytes_to_skip = (start_byte - aligned_start) as usize;
    }

    let stream = async_stream::stream! {
        let mut skipped = 0usize;
        let mut yielded = 0u64;
        while let Some(chunk) = download.next().await.transpose() {
            match chunk {
                Ok(mut data) => {
                    if skipped < bytes_to_skip {
                        let to_skip = bytes_to_skip - skipped;
                        if data.len() <= to_skip {
                            skipped += data.len();
                            continue;
                        }
                        data = data[to_skip..].to_vec();
                        skipped = bytes_to_skip;
                    }
                    let remaining = content_length.saturating_sub(yielded);
                    if remaining == 0 {
                        break;
                    }
                    let take = remaining.min(data.len() as u64) as usize;
                    if take > 0 {
                        yielded += take as u64;
                        yield Ok::<Bytes, actix_web::Error>(Bytes::copy_from_slice(&data[..take]));
                    }
                    if yielded >= content_length {
                        break;
                    }
                }
                Err(error) => {
                    log::error!("Telegram stream failed: {error}");
                    break;
                }
            }
        }
    };

    let mut response = if requested_range.is_some() {
        let mut response = HttpResponse::PartialContent();
        response.insert_header((CONTENT_RANGE, format!("bytes {start_byte}-{end_byte}/{size}")));
        response
    } else {
        HttpResponse::Ok()
    };
    response
        .insert_header((CONTENT_TYPE, mime.to_string()))
        .insert_header(("Accept-Ranges", "bytes"))
        .insert_header((CONTENT_LENGTH, content_length.to_string()))
        .insert_header((
            CONTENT_DISPOSITION,
            format!("inline; filename=\"{}\"", sanitise_upload_filename(filename)),
        ))
        .streaming(stream)
}

fn build_playback_segment_response(
    state: &AppState,
    asset: PlaybackAsset,
    segment_index: u64,
) -> HttpResponse {
    let Some((segment_start, segment_end)) = playback_segment_bounds(asset.size_bytes, segment_index) else {
        return HttpResponse::RangeNotSatisfiable()
            .insert_header((CONTENT_RANGE, format!("bytes */{}", asset.size_bytes)))
            .finish();
    };
    let content_length = segment_end - segment_start + 1;
    let client = state.client.clone();
    let peer_cache = state.peer_cache.clone();
    let parts = asset.parts.clone();

    let stream = async_stream::stream! {
        let mut logical_cursor = 0u64;
        let mut emitted = 0u64;
        for part in parts {
            let part_start = logical_cursor;
            let part_end = part_start + part.size_bytes - 1;
            logical_cursor += part.size_bytes;
            if part_end < segment_start {
                continue;
            }
            if part_start > segment_end {
                break;
            }
            let local_start = segment_start.max(part_start) - part_start;
            let local_end = segment_end.min(part_end) - part_start;
            let local_length = local_end - local_start + 1;
            let peer = match resolve_peer(&client, part.folder_id, &peer_cache).await {
                Ok(peer) => peer,
                Err(error) => {
                    log::error!("Playback peer resolution failed: {error}");
                    yield Err::<Bytes, actix_web::Error>(actix_web::error::ErrorBadGateway("Playback origin unavailable"));
                    return;
                }
            };
            let messages = match client.get_messages_by_id(peer, &[part.message_id]).await {
                Ok(messages) => messages,
                Err(error) => {
                    log::error!("Playback message fetch failed: {error}");
                    yield Err::<Bytes, actix_web::Error>(actix_web::error::ErrorBadGateway("Playback origin unavailable"));
                    return;
                }
            };
            let Some(message) = messages.first().and_then(|message| message.as_ref()) else {
                yield Err::<Bytes, actix_web::Error>(actix_web::error::ErrorNotFound("Playback part missing"));
                return;
            };
            let Some(media) = message.media() else {
                yield Err::<Bytes, actix_web::Error>(actix_web::error::ErrorNotFound("Playback media missing"));
                return;
            };
            if media_size(&media) != part.size_bytes {
                yield Err::<Bytes, actix_web::Error>(actix_web::error::ErrorBadGateway("Playback part size mismatch"));
                return;
            }

            let aligned_start = (local_start / CDN_ALIGNMENT) * CDN_ALIGNMENT;
            let chunk_index = (aligned_start / DOWNLOAD_CHUNK_SIZE as u64) as i32;
            let mut download = client.iter_download(&media).chunk_size(DOWNLOAD_CHUNK_SIZE);
            if chunk_index > 0 {
                download = download.skip_chunks(chunk_index);
            }
            let bytes_to_skip = (local_start - aligned_start) as usize;
            let mut skipped = 0usize;
            let mut part_emitted = 0u64;
            while let Some(next) = download.next().await.transpose() {
                let mut data = match next {
                    Ok(data) => data,
                    Err(error) => {
                        log::error!("Playback Telegram stream failed: {error}");
                        yield Err::<Bytes, actix_web::Error>(actix_web::error::ErrorBadGateway("Playback Telegram stream failed"));
                        return;
                    }
                };
                if skipped < bytes_to_skip {
                    let remaining_skip = bytes_to_skip - skipped;
                    if data.len() <= remaining_skip {
                        skipped += data.len();
                        continue;
                    }
                    data = data[remaining_skip..].to_vec();
                    skipped = bytes_to_skip;
                }
                let remaining = local_length.saturating_sub(part_emitted);
                if remaining == 0 {
                    break;
                }
                let take = remaining.min(data.len() as u64) as usize;
                if take > 0 {
                    part_emitted += take as u64;
                    emitted += take as u64;
                    yield Ok::<Bytes, actix_web::Error>(Bytes::copy_from_slice(&data[..take]));
                }
                if part_emitted >= local_length {
                    break;
                }
            }
            if part_emitted != local_length {
                yield Err::<Bytes, actix_web::Error>(actix_web::error::ErrorBadGateway("Playback segment ended early"));
                return;
            }
        }
        if emitted != content_length {
            yield Err::<Bytes, actix_web::Error>(actix_web::error::ErrorBadGateway("Playback segment is incomplete"));
        }
    };

    HttpResponse::Ok()
        .insert_header((CONTENT_TYPE, asset.mime_type))
        .insert_header((CONTENT_LENGTH, content_length.to_string()))
        .insert_header(("Accept-Ranges", "bytes"))
        .insert_header(("Cache-Control", "public, max-age=31536000, immutable"))
        .insert_header(("ETag", format!("\"{}-{}\"", asset.etag, segment_index)))
        .insert_header(("X-Content-Type-Options", "nosniff"))
        .streaming(stream)
}

#[get("/api/v1/playback/{asset_id}/segments/{segment_index}")]
async fn playback_segment(
    req: HttpRequest,
    path: web::Path<(String, u64)>,
    state: web::Data<AppState>,
) -> impl Responder {
    if let Err(response) = require_playback_origin(&req, state.get_ref()) {
        return response;
    }
    let (asset_id, segment_index) = path.into_inner();
    if !valid_asset_id(&asset_id) {
        return json_error("INVALID_ASSET_ID", "Playback asset id is invalid", 400);
    }
    let asset = state.playback_registry.read().await.get(&asset_id).cloned();
    let Some(asset) = asset else {
        return json_error("NOT_FOUND", "Playback asset not found", 404);
    };
    build_playback_segment_response(state.get_ref(), asset, segment_index)
}

#[get("/api/v1/files/{message_id}/download")]
async fn download_file(
    req: HttpRequest,
    path: web::Path<i64>,
    query: web::Query<FolderQuery>,
    state: web::Data<AppState>,
) -> impl Responder {
    if let Err(response) = require_auth(&req, state.get_ref()) {
        return response;
    }
    let message_id = match i32::try_from(path.into_inner()) {
        Ok(value) if value > 0 => value,
        _ => return json_error("INVALID_MESSAGE_ID", "Invalid message id", 400),
    };
    let peer = match resolve_peer(&state.client, query.folder_id, &state.peer_cache).await {
        Ok(peer) => peer,
        Err(error) => return json_error("PEER_ERROR", error, 400),
    };
    match state.client.get_messages_by_id(peer, &[message_id]).await {
        Ok(messages) => {
            let Some(message) = messages.first().and_then(|message| message.as_ref()) else {
                return json_error("NOT_FOUND", "File not found", 404);
            };
            let Some(media) = message.media() else {
                return json_error("NOT_FOUND", "Message has no media", 404);
            };
            let (mime, filename) = match &media {
                Media::Document(document) => (
                    document.mime_type().unwrap_or("application/octet-stream").to_string(),
                    document.name().to_string(),
                ),
                Media::Photo(_) => ("image/jpeg".to_string(), "Photo.jpg".to_string()),
                _ => ("application/octet-stream".to_string(), "download".to_string()),
            };
            build_media_response(&state.client, &media, &req, &mime, &filename)
        }
        Err(error) => json_error("FETCH_ERROR", error.to_string(), 502),
    }
}

fn env_required(name: &str) -> Result<String, String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is required"))
}

fn load_api_key() -> Result<String, String> {
    if let Ok(path) = env::var("TELEGRAM_DRIVE_API_KEY_FILE") {
        let value = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
        let value = value.trim().to_string();
        if !value.is_empty() {
            return Ok(value);
        }
    }
    env_required("TELEGRAM_DRIVE_API_KEY")
}

fn session_path() -> PathBuf {
    env::var("TELEGRAM_SESSION_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/telegram.session"))
}

fn api_id() -> Result<i32, String> {
    env_required("TELEGRAM_API_ID")?
        .parse::<i32>()
        .map_err(|_| "TELEGRAM_API_ID must be an integer".to_string())
}

async fn connect(api_id: i32, path: &Path) -> Result<TelegramConnection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let path = path
        .to_str()
        .ok_or_else(|| "Session path is not valid UTF-8".to_string())?;
    let session = Arc::new(SqliteSession::open(path).map_err(|error| error.to_string())?);
    let pool = SenderPool::with_configuration(session.clone(), api_id, ConnectionParams::default());
    let client = Client::new(&pool);
    let SenderPool { runner, .. } = pool;
    tokio::spawn(async move {
        runner.run().await;
    });
    Ok(TelegramConnection { client, session })
}

fn prompt_line(label: &str) -> Result<String, String> {
    print!("{label}");
    io::stdout().flush().map_err(|error| error.to_string())?;
    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .map_err(|error| error.to_string())?;
    Ok(value.trim().to_string())
}

async fn send_login_code(
    connection: &TelegramConnection,
    phone: &str,
    api_id: i32,
    api_hash: &str,
) -> Result<tl::enums::auth::SentCode, String> {
    let request = tl::functions::auth::SendCode {
        phone_number: phone.to_string(),
        api_id,
        api_hash: api_hash.to_string(),
        settings: tl::types::CodeSettings {
            allow_flashcall: false,
            current_number: false,
            allow_app_hash: false,
            allow_missed_call: true,
            allow_firebase: false,
            unknown_number: false,
            logout_tokens: None,
            token: None,
            app_sandbox: None,
        }
        .into(),
    };
    let mut migrated = false;
    loop {
        match timeout(Duration::from_secs(30), connection.client.invoke(&request)).await {
            Ok(Ok(sent_code)) => return Ok(sent_code),
            Ok(Err(InvocationError::Rpc(error))) if error.code == 303 && !migrated => {
                let dc_id = error
                    .value
                    .and_then(|value| i32::try_from(value).ok())
                    .ok_or_else(|| "Telegram requested an invalid data-center migration".to_string())?;
                connection.session.set_home_dc_id(dc_id);
                migrated = true;
            }
            Ok(Err(error)) => return Err(error.to_string()),
            Err(_) => return Err("Telegram timed out while requesting the login code".to_string()),
        }
    }
}

async fn complete_raw_login(
    connection: &TelegramConnection,
    authorization: tl::types::auth::Authorization,
) -> Result<(), String> {
    match &authorization.user {
        tl::enums::User::User(user) => {
            connection.session.cache_peer(&PeerInfo::User {
                id: user.id,
                auth: Some(user.access_hash.map(PeerAuth::from_hash).unwrap_or_default()),
                bot: Some(user.bot),
                is_self: Some(true),
            });
        }
        tl::enums::User::Empty(user) => {
            connection.session.cache_peer(&PeerInfo::User {
                id: user.id,
                auth: Some(PeerAuth::default()),
                bot: Some(false),
                is_self: Some(true),
            });
        }
    }
    if let Ok(Ok(tl::enums::updates::State::State(update))) = timeout(
        Duration::from_secs(15),
        connection.client.invoke(&tl::functions::updates::GetState {}),
    )
    .await
    {
        connection.session.set_update_state(UpdateState::All(UpdatesState {
            pts: update.pts,
            qts: update.qts,
            date: update.date,
            seq: update.seq,
            channels: Vec::new(),
        }));
    }
    Ok(())
}

async fn login() -> Result<(), String> {
    let api_id = api_id()?;
    let api_hash = match env::var("TELEGRAM_API_HASH") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => rpassword::prompt_password("Telegram API hash: ").map_err(|error| error.to_string())?,
    };
    if api_hash.is_empty() {
        return Err("Telegram API hash is required".to_string());
    }
    let connection = connect(api_id, &session_path()).await?;
    if connection.client.is_authorized().await.map_err(|error| error.to_string())? {
        println!("Telegram session is already authorized.");
        return Ok(());
    }

    let phone = match env::var("TELEGRAM_PHONE") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => prompt_line("Telegram phone number (international format, e.g. +84...): ")?,
    };
    if !phone.starts_with('+') || phone.len() < 8 {
        return Err("Phone number must use international +country format".to_string());
    }

    let sent_code = send_login_code(&connection, &phone, api_id, &api_hash).await?;
    let phone_code_hash = match sent_code {
        tl::enums::auth::SentCode::Code(code) => code.phone_code_hash,
        tl::enums::auth::SentCode::Success(success) => match success.authorization {
            tl::enums::auth::Authorization::Authorization(authorization) => {
                complete_raw_login(&connection, authorization).await?;
                println!("Telegram session authorized.");
                return Ok(());
            }
            tl::enums::auth::Authorization::SignUpRequired(_) => {
                return Err("Use an existing Telegram account; sign-up is not supported here".to_string())
            }
        },
        tl::enums::auth::SentCode::PaymentRequired(_) => {
            return Err("Telegram did not provide a usable phone-code login flow".to_string())
        }
    };

    let code = prompt_line("Telegram login code: ")?;
    let request = tl::functions::auth::SignIn {
        phone_number: phone,
        phone_code_hash,
        phone_code: Some(code),
        email_verification: None,
    };
    match timeout(Duration::from_secs(30), connection.client.invoke(&request)).await {
        Err(_) => Err("Telegram timed out while verifying the login code".to_string()),
        Ok(Ok(tl::enums::auth::Authorization::Authorization(authorization))) => {
            complete_raw_login(&connection, authorization).await?;
            println!("Telegram session authorized and saved.");
            Ok(())
        }
        Ok(Ok(tl::enums::auth::Authorization::SignUpRequired(_))) => {
            Err("Use an existing Telegram account; sign-up is not supported here".to_string())
        }
        Ok(Err(error)) if error.is("SESSION_PASSWORD_NEEDED") => {
            let password = connection
                .client
                .invoke(&tl::functions::account::GetPassword {})
                .await
                .map_err(|error| error.to_string())?;
            let password: tl::types::account::Password = password.into();
            let token = PasswordToken::new(password);
            let value = rpassword::prompt_password("Telegram 2FA password: ")
                .map_err(|error| error.to_string())?;
            connection
                .client
                .check_password(token, &value)
                .await
                .map_err(|error| error.to_string())?;
            println!("Telegram session authorized and saved.");
            Ok(())
        }
        Ok(Err(error)) => Err(error.to_string()),
    }
}

async fn serve() -> Result<(), String> {
    let api_id = api_id()?;
    let api_key = load_api_key()?;
    if api_key.len() < 24 {
        return Err("TELEGRAM_DRIVE_API_KEY must be at least 24 characters".to_string());
    }
    let connection = connect(api_id, &session_path()).await?;
    if !connection
        .client
        .is_authorized()
        .await
        .map_err(|error| error.to_string())?
    {
        return Err("Telegram session is not authorized. Run the `login` command first.".to_string());
    }
    let bind = env::var("TELEGRAM_DRIVE_BIND").unwrap_or_else(|_| "0.0.0.0:8550".to_string());
    let registry_path = playback_registry_path();
    let registry = load_playback_registry(&registry_path).await?;
    let playback_origin_key = derive_playback_origin_key(&api_key);
    let state = AppState {
        client: connection.client,
        api_key: Arc::new(api_key),
        playback_origin_key: Arc::new(playback_origin_key),
        playback_registry_path: Arc::new(registry_path),
        playback_registry: Arc::new(RwLock::new(registry)),
        peer_cache: Arc::new(RwLock::new(HashMap::new())),
    };
    log::info!("Telegram Drive headless listening on {bind}");
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            .service(health)
            .service(list_files)
            .service(get_file)
            .service(upload_file)
            .service(upload_chunk)
            .service(put_playback_asset)
            .service(delete_playback_asset)
            .service(playback_segment)
            .service(delete_file)
            .service(download_file)
    })
    .bind(&bind)
    .map_err(|error| error.to_string())?
    .run()
    .await
    .map_err(|error| error.to_string())
}

#[actix_web::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let command = env::args().nth(1).unwrap_or_else(|| "serve".to_string());
    let result = match command.as_str() {
        "login" => login().await,
        "serve" => serve().await,
        "--help" | "-h" | "help" => {
            println!("telegram-drive-headless [login|serve]");
            println!("  login  bootstrap/update the persistent Telegram session");
            println!("  serve  run the private REST API for DMCMS Drama");
            Ok(())
        }
        _ => Err(format!("Unknown command: {command}")),
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parser_accepts_normal_ranges() {
        assert_eq!(parse_range_header("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range_header("bytes=100-", 1000), Some((100, 999)));
        assert_eq!(parse_range_header("bytes=900-2000", 1000), Some((900, 999)));
    }

    #[test]
    fn range_parser_rejects_invalid_or_multi_ranges() {
        assert_eq!(parse_range_header("bytes=-100", 1000), None);
        assert_eq!(parse_range_header("bytes=1000-", 1000), None);
        assert_eq!(parse_range_header("bytes=0-1,3-4", 1000), None);
    }

    #[test]
    fn upload_filename_is_reduced_to_safe_basename() {
        assert_eq!(sanitise_upload_filename("../../episode-1.mp4"), "episode-1.mp4");
        assert_eq!(sanitise_upload_filename(".."), "file");
    }

    #[test]
    fn playback_origin_key_is_deterministic_and_scoped() {
        let first = derive_playback_origin_key("abcdefghijklmnopqrstuvwxyz123456");
        let second = derive_playback_origin_key("abcdefghijklmnopqrstuvwxyz123456");
        let other = derive_playback_origin_key("abcdefghijklmnopqrstuvwxyz654321");
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn playback_asset_validation_requires_exact_part_size_sum() {
        let valid = validate_playback_asset(PlaybackAssetInput {
            size_bytes: 10,
            mime_type: "video/mp4".to_string(),
            etag: "version-1".to_string(),
            parts: vec![
                PlaybackAssetPart { message_id: 1, folder_id: None, size_bytes: 6 },
                PlaybackAssetPart { message_id: 2, folder_id: Some(9), size_bytes: 4 },
            ],
        })
        .unwrap();
        assert_eq!(valid.size_bytes, 10);
        assert_eq!(valid.parts.len(), 2);

        let invalid = validate_playback_asset(PlaybackAssetInput {
            size_bytes: 11,
            mime_type: "video/mp4".to_string(),
            etag: "version-1".to_string(),
            parts: valid.parts,
        });
        assert!(invalid.is_err());
    }

    #[test]
    fn playback_segments_are_fixed_and_bounded() {
        assert_eq!(playback_segment_bounds(PLAYBACK_SEGMENT_SIZE * 2 + 7, 0), Some((0, PLAYBACK_SEGMENT_SIZE - 1)));
        assert_eq!(
            playback_segment_bounds(PLAYBACK_SEGMENT_SIZE * 2 + 7, 2),
            Some((PLAYBACK_SEGMENT_SIZE * 2, PLAYBACK_SEGMENT_SIZE * 2 + 6))
        );
        assert_eq!(playback_segment_bounds(PLAYBACK_SEGMENT_SIZE * 2 + 7, 3), None);
    }

    #[test]
    fn direct_chunk_requires_bounded_content_length() {
        let valid = actix_web::test::TestRequest::default()
            .insert_header((CONTENT_LENGTH, "33554432"))
            .to_http_request();
        assert_eq!(required_content_length(&valid).unwrap(), 33_554_432);

        let too_large = actix_web::test::TestRequest::default()
            .insert_header((CONTENT_LENGTH, (MAX_DIRECT_CHUNK_BYTES + 1).to_string()))
            .to_http_request();
        assert_eq!(
            required_content_length(&too_large).unwrap_err().status().as_u16(),
            413
        );
    }

    #[test]
    fn direct_chunk_filename_is_sanitized_from_header() {
        let req = actix_web::test::TestRequest::default()
            .insert_header(("X-File-Name", "../../episode.mp4.part000001"))
            .to_http_request();
        assert_eq!(direct_chunk_filename(&req), "episode.mp4.part000001");
    }
}
