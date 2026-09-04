use actix_multipart::Multipart;
use actix_web::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use actix_web::web::Bytes;
use actix_web::{delete, get, post, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use constant_time_eq::constant_time_eq;
use futures::{StreamExt, TryStreamExt};
use grammers_client::types::{Media, PasswordToken, Peer};
use grammers_client::{Client, InputMessage};
use grammers_mtsender::{ConnectionParams, InvocationError, SenderPool};
use grammers_session::storages::SqliteSession;
use grammers_session::types::{PeerAuth, PeerInfo, UpdateState, UpdatesState};
use grammers_session::Session;
use grammers_tl_types as tl;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};

const TELEGRAM_MAX_FILE_SIZE: u64 = 2_000_000_000;
const MAX_MULTIPART_METADATA_BYTES: usize = 128;
const MAX_UPLOAD_FILENAME_CHARS: usize = 255;
const CDN_ALIGNMENT: u64 = 524_288;
const DOWNLOAD_CHUNK_SIZE: i32 = 65_536;

#[derive(Clone)]
struct AppState {
    client: Client,
    api_key: Arc<String>,
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
    let state = AppState {
        client: connection.client,
        api_key: Arc::new(api_key),
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
}
