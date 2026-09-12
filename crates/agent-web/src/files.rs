//! The authenticated workspace file flow (M5, §6.3b / ADR-017).
//!
//! `POST /api/files?name=…` lands an upload in the sandbox workspace (the one
//! writable folder); `GET /api/files/{path}` serves workspace files with MIME
//! detection and path sanitization (no traversal, no symlink escape — the
//! shared rules live in `agent_core::sandbox::workspace`). Images render
//! inline; everything else downloads as an attachment.
//!
//! The routes sit under `/api`, so they inherit the `X-Auth-Token` check when
//! one is configured. The UI fetches with the header and turns responses into
//! blob URLs because an `<img>` tag cannot carry a custom header.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_core::{ApiErrorKind, inline_mime, mime_hint, resolve_workspace_file, safe_file_name};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::error;
use crate::routes::AppState;

/// Workspace access for the file routes: the sandbox workspace plus the
/// configured upload cap.
#[derive(Clone, Debug)]
pub struct Files {
    pub workspace: PathBuf,
    pub max_upload_bytes: usize,
}

#[derive(Debug, Default, Deserialize)]
pub struct UploadQuery {
    /// Target file name (a single path component).
    #[serde(default)]
    pub name: Option<String>,
}

/// Upload one file into the workspace. The body is the raw file content (the
/// UI sets `?name=`); the size cap is enforced by the route's body limit and
/// re-checked here.
pub async fn upload(
    State(state): State<AppState>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    let Some(files) = state.files.as_ref() else {
        return error::json_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "the file flow is not configured",
        );
    };
    let Some(name) = query.name.as_deref() else {
        return error::json_error(
            StatusCode::BAD_REQUEST,
            "config",
            "upload needs a ?name= query parameter",
        );
    };
    let name = match safe_file_name(name) {
        Ok(name) => name,
        Err(err) => return error::json_error(StatusCode::BAD_REQUEST, "config", err.message),
    };
    if body.is_empty() {
        return error::json_error(
            StatusCode::BAD_REQUEST,
            "config",
            "the uploaded file is empty",
        );
    }
    if body.len() > files.max_upload_bytes {
        return error::json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "config",
            format!(
                "file is larger than the {} MiB upload cap",
                files.max_upload_bytes / (1024 * 1024)
            ),
        );
    }
    let target = files.workspace.join(&name);
    // Atomic tmp+rename: a dropped upload never leaves a partial file, and the
    // hidden temp name is ignored by the artifact scan (M5).
    static UPLOAD_SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = files.workspace.join(format!(
        ".kaeru-upload-{}-{}.tmp",
        std::process::id(),
        UPLOAD_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let size = body.len();
    if let Err(err) = tokio::fs::write(&tmp, &body).await {
        return error::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("cannot store the upload: {err}"),
        );
    }
    if let Err(err) = tokio::fs::rename(&tmp, &target).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return error::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("cannot store the upload: {err}"),
        );
    }
    (
        StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "path": name,
            "size": size,
        })),
    )
        .into_response()
}

/// Serve a workspace file: authenticated, MIME-aware, path-sanitized.
pub async fn serve(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    let Some(files) = state.files.as_ref() else {
        return error::json_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "the file flow is not configured",
        );
    };
    let resolved = match resolve_workspace_file(&files.workspace, &path) {
        Ok(resolved) => resolved,
        Err(err) => {
            let status = match err.kind {
                ApiErrorKind::Forbidden => StatusCode::FORBIDDEN,
                ApiErrorKind::NotFound => StatusCode::NOT_FOUND,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            return error::json_error(status, err.kind.as_str(), err.message);
        }
    };
    let metadata = match tokio::fs::metadata(&resolved).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => {
            return error::json_error(StatusCode::NOT_FOUND, "not_found", "not a regular file");
        }
        Err(err) => {
            return error::json_error(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("cannot read {path}: {err}"),
            );
        }
    };
    let file = match tokio::fs::File::open(&resolved).await {
        Ok(file) => file,
        Err(err) => {
            return error::json_error(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("cannot read {path}: {err}"),
            );
        }
    };

    let name = resolved
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    // Only raster images render inline; everything else downloads, so a
    // hostile HTML/SVG artifact can never execute on our origin.
    let (mime, disposition) = match inline_mime(&resolved) {
        Some(mime) => (mime, "inline"),
        None => (
            mime_hint(&resolved).unwrap_or("application/octet-stream"),
            "attachment",
        ),
    };
    let mut response = Body::from_stream(stream_file(file)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static(mime));
    headers.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&format!(
            "{disposition}; filename=\"{}\"",
            escape_header_value(name)
        ))
        .unwrap_or(header::HeaderValue::from_static("attachment")),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(&metadata.len().to_string())
            .unwrap_or(header::HeaderValue::from_static("0")),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    response
}

/// Stream a file in bounded chunks instead of buffering it in memory.
fn stream_file(
    file: tokio::fs::File,
) -> impl tokio_stream::Stream<Item = Result<Bytes, std::io::Error>> {
    use tokio::io::AsyncReadExt as _;
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        let mut file = file;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match file.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => {
                    let chunk = Bytes::copy_from_slice(&buffer[..read]);
                    if tx.send(Ok(chunk)).await.is_err() {
                        break; // client went away
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    break;
                }
            }
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

/// Keep a file name safe inside a quoted header value (no quotes/backslashes/
/// control characters; our names cannot contain them anyway).
fn escape_header_value(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            '"' | '\\' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_values_are_sanitized() {
        assert_eq!(escape_header_value("plot.png"), "plot.png");
        assert_eq!(escape_header_value("a\"b\\c"), "a_b_c");
    }
}
