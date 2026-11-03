use super::*;

#[derive(serde::Deserialize)]
pub(crate) struct WriteQuery {
    #[serde(default)]
    overwrite: Option<String>,
}

pub(crate) async fn handle_write(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WriteQuery>,
    body: axum::body::Bytes,
) -> Response {
    if !state.enable_write {
        return (
            StatusCode::FORBIDDEN,
            "/write は無効です(--enable-write で有効化してください)\n",
        )
            .into_response();
    }
    let raw_path = match headers
        .get("X-Clipwire-Path")
        .and_then(|value| value.to_str().ok())
    {
        Some(value) => value,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "X-Clipwire-Path ヘッダが必要です\n",
            )
                .into_response();
        }
    };
    let parsed = match crate::write_path::decode_and_validate_header_path(raw_path) {
        Ok(parsed) => parsed,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("パスが不正です: {error:?}\n"),
            )
                .into_response();
        }
    };

    let absolute = crate::write_path::to_absolute_string(&parsed);
    let requested_path = request_path(&absolute);
    let Some(parent) = requested_path.parent() else {
        return (StatusCode::BAD_REQUEST, "パスにファイル名がありません\n").into_response();
    };
    let Some(file_name) = requested_path.file_name() else {
        return (StatusCode::BAD_REQUEST, "パスにファイル名がありません\n").into_response();
    };
    let canonical_parent = match std::fs::canonicalize(parent) {
        Ok(path) => path,
        Err(_) => {
            return (StatusCode::CONFLICT, "親ディレクトリが存在しません\n").into_response();
        }
    };
    let canonical_parsed = match super::parse_os_path(&canonical_parent) {
        Ok(parsed) => parsed,
        Err(_) => {
            return (StatusCode::FORBIDDEN, "書き込み先が許可されていません\n").into_response();
        }
    };
    if !state
        .write_roots
        .iter()
        .any(|root| crate::write_path::is_under(&canonical_parsed, root))
    {
        return (StatusCode::FORBIDDEN, "このパスは許可ルートの外です\n").into_response();
    }
    if state
        .write_denylist
        .iter()
        .any(|deny| super::write_path_denied(&canonical_parsed, deny))
    {
        return (
            StatusCode::FORBIDDEN,
            "このパスは拒否対象ディレクトリと重なっています\n",
        )
            .into_response();
    }

    // The parent can theoretically be swapped after canonicalization; ADR-0014
    // accepts this residual TOCTOU risk for callers already running as the user.
    let final_path = canonical_parent.join(file_name);
    let overwrite = query.overwrite.as_deref() == Some("1");
    use sha2::Digest;
    let sha256 = hex::encode(sha2::Sha256::digest(&body));
    let bytes_len = body.len() as u64;

    let mut random = [0_u8; 8];
    if let Err(error) = getrandom::fill(&mut random) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("一時ファイル名の生成に失敗しました: {error}\n"),
        )
            .into_response();
    }
    let temp_path = canonical_parent.join(format!(".clipwire-write-{}.tmp", hex::encode(random)));
    let write_result = {
        let temp_path = temp_path.clone();
        let body = body.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            file.write_all(&body)
        })
        .await
    };
    if let Err(error) = write_result.unwrap_or_else(|e| Err(std::io::Error::other(e.to_string()))) {
        let _ = std::fs::remove_file(&temp_path);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("一時ファイルの書き込みに失敗しました: {error}\n"),
        )
            .into_response();
    }

    let mut event = crate::audit::AuditEvent::new(crate::audit::AuditEventKind::Write);
    event.path = Some(final_path.display().to_string());
    event.bytes = Some(bytes_len);
    event.sha256 = Some(sha256.clone());
    event.overwrote = Some(overwrite);
    if let Err(error) = state.audit.try_record(event) {
        let _ = std::fs::remove_file(&temp_path);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("監査記録に失敗しました。書き込みを中止しました: {error}\n"),
        )
            .into_response();
    }

    let move_result = {
        let temp_path = temp_path.clone();
        let final_path = final_path.clone();
        tokio::task::spawn_blocking(move || {
            if overwrite {
                os::move_replace(&temp_path, &final_path)
            } else {
                os::move_no_replace(&temp_path, &final_path)
            }
        })
        .await
    };
    match move_result.unwrap_or_else(|e| Err(std::io::Error::other(e.to_string()))) {
        Ok(()) => (
            if overwrite {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            },
            axum::Json(serde_json::json!({"bytes": bytes_len, "sha256": sha256})),
        )
            .into_response(),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&temp_path);
            (
                StatusCode::CONFLICT,
                axum::Json(serde_json::json!({"error": "already_exists"})),
            )
                .into_response()
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temp_path);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("書き込みに失敗しました: {error}\n"),
            )
                .into_response()
        }
    }
}

#[cfg(windows)]
fn request_path(absolute: &str) -> PathBuf {
    PathBuf::from(absolute)
}

#[cfg(not(windows))]
fn request_path(absolute: &str) -> PathBuf {
    // Linux-only development adapter: C:\tmp\x maps to /tmp/x.
    let rest = absolute.get(3..).unwrap_or_default();
    PathBuf::from(format!("/{}", rest.replace('\\', "/")))
}

#[cfg(windows)]
mod os {
    use std::{io, os::windows::ffi::OsStrExt, path::Path};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, WIN32_ERROR};
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVE_FILE_FLAGS,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn move_file(from: &Path, to: &Path, flags: MOVE_FILE_FLAGS) -> io::Result<()> {
        let from_w = wide(from);
        let to_w = wide(to);
        if unsafe { MoveFileExW(PCWSTR(from_w.as_ptr()), PCWSTR(to_w.as_ptr()), flags) }.is_ok() {
            return Ok(());
        }
        let code = unsafe { GetLastError() };
        let kind = match code {
            WIN32_ERROR(183) | WIN32_ERROR(80) => io::ErrorKind::AlreadyExists,
            _ => io::ErrorKind::Other,
        };
        Err(io::Error::new(
            kind,
            format!("MoveFileExW failed: GetLastError={}", code.0),
        ))
    }

    pub(crate) fn move_no_replace(from: &Path, to: &Path) -> io::Result<()> {
        move_file(from, to, MOVE_FILE_FLAGS(0))
    }

    pub(crate) fn move_replace(from: &Path, to: &Path) -> io::Result<()> {
        move_file(from, to, MOVEFILE_REPLACE_EXISTING)
    }
}

#[cfg(not(windows))]
mod os {
    use std::{io, path::Path};

    pub(crate) fn move_no_replace(from: &Path, to: &Path) -> io::Result<()> {
        if to.exists() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        std::fs::rename(from, to)
    }

    pub(crate) fn move_replace(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
}
