use super::*;

pub(crate) async fn handle_clip(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }

    let (tx, rx) = oneshot::channel();
    if s.clip_tx.send(ClipRequest::GetClip { reply: tx }).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let Ok(kind) = rx.await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    match kind {
        ClipKind::Image(data) => {
            s.last_clip.lock().unwrap().files.clear();
            clip_resp("image", "image/png", data)
        }
        ClipKind::Files(paths) => {
            let j = serde_json::to_vec(&paths).unwrap();
            s.last_clip.lock().unwrap().files = paths;
            clip_resp("files", "application/json", j)
        }
        ClipKind::VFiles(names) => {
            let j = serde_json::to_vec(&names).unwrap();
            s.last_clip.lock().unwrap().vfiles = names;
            clip_resp("vfiles", "application/json", j)
        }
        ClipKind::Audio(data) => clip_resp("audio", "audio/wav", data),
        ClipKind::Html(html) => clip_resp("html", "text/html; charset=utf-8", html.into_bytes()),
        ClipKind::Url(url) => clip_resp("url", "text/plain; charset=utf-8", url.into_bytes()),
        ClipKind::Rtf(rtf) => clip_resp("rtf", "text/rtf", rtf.into_bytes()),
        ClipKind::Text(text) => clip_resp("text", "text/plain; charset=utf-8", text.into_bytes()),
        ClipKind::Empty => {
            #[cfg(windows)]
            win_clip::show_balloon("クリップボードが空です");
            clip_resp("empty", "text/plain; charset=utf-8", b"".to_vec())
        }
    }
}

pub(crate) fn clip_resp(kind: &str, ct: &str, body: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("X-Clip-Kind", kind)
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from(body))
        .unwrap()
}

pub(crate) async fn handle_clip_post(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }
    let text = match String::from_utf8(body.to_vec()) {
        Ok(t) => t,
        Err(_) => return (StatusCode::BAD_REQUEST, "Body must be UTF-8\n").into_response(),
    };
    let (tx, rx) = oneshot::channel();
    if s.clip_tx
        .send(ClipRequest::SetClip { text, reply: tx })
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match rx.await {
        Ok(Ok(())) => (StatusCode::NO_CONTENT, "").into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct FileQuery {
    path: String,
}
#[derive(Deserialize)]
pub(crate) struct VFileQuery {
    i: usize,
}
#[derive(Deserialize)]
pub(crate) struct OpenQuery {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OpenBody {
    name: String,
}

pub(crate) async fn handle_open(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<OpenQuery>,
) -> Response {
    open_target(&s, &headers, &q.name)
}

pub(crate) async fn handle_open_post(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }
    let body: OpenBody = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, format!("invalid JSON: {error}\n")).into_response()
        }
    };
    open_target(&s, &headers, &body.name)
}

fn open_target(s: &AppState, headers: &HeaderMap, name: &str) -> Response {
    if !check_auth(&s.token, headers) {
        return unauthorized();
    }
    let url = match name {
        "chatgpt" => "https://chatgpt.com",
        "claude" => "https://claude.ai",
        "tailscale" => "https://login.tailscale.com/admin",
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown target: {other}\n"),
            )
                .into_response()
        }
    };
    #[cfg(windows)]
    if let Err(e) = std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .spawn()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("ブラウザを開けませんでした: {e}\n"),
        )
            .into_response();
    }
    (StatusCode::OK, format!("{url}\n")).into_response()
}

pub(crate) async fn handle_file(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<FileQuery>,
) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }
    let req_path = match std::path::Path::new(&q.path).canonicalize() {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let allowed = s.last_clip.lock().unwrap().files.iter().any(|f| {
        std::path::Path::new(f)
            .canonicalize()
            .map(|p| p == req_path)
            .unwrap_or(false)
    });
    if !allowed {
        return StatusCode::FORBIDDEN.into_response();
    }
    let (tx, rx) = oneshot::channel();
    if s.clip_tx
        .send(ClipRequest::GetFile {
            path: q.path,
            reply: tx,
        })
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match rx.await {
        Ok(Some(data)) => {
            let mime = mime_for_ext(req_path.extension().and_then(|e| e.to_str()).unwrap_or(""));
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .body(Body::from(data))
                .unwrap()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(crate) async fn handle_vfile(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<VFileQuery>,
) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }
    let (tx, rx) = oneshot::channel();
    if s.clip_tx
        .send(ClipRequest::GetVFile {
            index: q.i,
            reply: tx,
        })
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match rx.await {
        Ok(Some(data)) => {
            let fname = s
                .last_clip
                .lock()
                .unwrap()
                .vfiles
                .get(q.i)
                .cloned()
                .unwrap_or_else(|| format!("file_{}", q.i));
            let mime = mime_for_ext(
                std::path::Path::new(&fname)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or(""),
            );
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header(
                    "Content-Disposition",
                    format!("attachment; filename=\"{fname}\""),
                )
                .body(Body::from(data))
                .unwrap()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(crate) fn mime_for_ext(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "zip" => "application/zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}
