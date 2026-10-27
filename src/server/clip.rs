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
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let content = match parse_clip_body(content_type, &body) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("{e}\n")).into_response(),
    };
    let (tx, rx) = oneshot::channel();
    if s.clip_tx
        .send(ClipRequest::SetClip { content, reply: tx })
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

/// `application/json` で送られるバンドル。PNG は base64。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipBundle {
    text: Option<String>,
    png: Option<String>,
    svg: Option<String>,
}

fn checked_png(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .map_err(|e| format!("invalid PNG: {e}"))?;
    Ok(bytes)
}

fn checked_svg(svg: String) -> Result<String, String> {
    if looks_like_svg(&svg) {
        Ok(svg)
    } else {
        Err("body is not an SVG document".into())
    }
}

/// Content-Type に従って本文を検証し、書き込み内容へ変換する。
/// 未指定・text/* は従来どおり UTF-8 テキスト。
/// 単体の SVG は、非対応アプリ向けにソースをテキストとしても載せる。
pub(crate) fn parse_clip_body(content_type: &str, body: &[u8]) -> Result<ClipContent, String> {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match mime.as_str() {
        "image/png" => Ok(ClipContent {
            png: Some(checked_png(body.to_vec())?),
            ..Default::default()
        }),
        "image/svg+xml" => {
            let svg =
                String::from_utf8(body.to_vec()).map_err(|_| "SVG must be UTF-8".to_string())?;
            let svg = checked_svg(svg)?;
            Ok(ClipContent {
                text: Some(svg.clone()),
                svg: Some(svg),
                ..Default::default()
            })
        }
        "application/json" => {
            use base64::Engine as _;
            let bundle: ClipBundle =
                serde_json::from_slice(body).map_err(|e| format!("invalid JSON: {e}"))?;
            let png = bundle
                .png
                .map(|b| {
                    base64::engine::general_purpose::STANDARD
                        .decode(b)
                        .map_err(|e| format!("invalid base64 in png: {e}"))
                        .and_then(checked_png)
                })
                .transpose()?;
            let content = ClipContent {
                text: bundle.text,
                png,
                svg: bundle.svg.map(checked_svg).transpose()?,
            };
            if content == ClipContent::default() {
                return Err("bundle has no text, png or svg".into());
            }
            Ok(content)
        }
        m if m.is_empty() || m.starts_with("text/") => String::from_utf8(body.to_vec())
            .map(|t| ClipContent {
                text: Some(t),
                ..Default::default()
            })
            .map_err(|_| "Body must be UTF-8".to_string()),
        other => Err(format!("unsupported Content-Type: {other}")),
    }
}

/// 先頭の BOM・XML 宣言・コメント・DOCTYPE を読み飛ばし、最初の要素が `<svg` か判定する。
pub(crate) fn looks_like_svg(src: &str) -> bool {
    let mut rest = src.trim_start_matches('\u{feff}').trim_start();
    loop {
        let skip = if rest.starts_with("<?") {
            rest.find("?>").map(|i| i + 2)
        } else if rest.starts_with("<!--") {
            rest.find("-->").map(|i| i + 3)
        } else if rest.starts_with("<!") {
            rest.find('>').map(|i| i + 1)
        } else {
            None
        };
        match skip {
            Some(n) => rest = rest[n..].trim_start(),
            None => break,
        }
    }
    rest.get(..4)
        .is_some_and(|h| h.eq_ignore_ascii_case("<svg"))
        && rest[4..]
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/')
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipFormat {
    UnicodeText,
    Png,
    Dib,
    Svg,
}

/// 書き込み内容を (クリップボード形式, データ) の列へ展開する。
/// - テキスト: CF_UNICODETEXT
/// - PNG: "PNG"(アルファ保持)+ CF_DIB(互換用)
/// - SVG: "image/svg+xml"
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn clip_formats(content: &ClipContent) -> anyhow::Result<Vec<(ClipFormat, Vec<u8>)>> {
    let mut out = Vec::new();
    if let Some(svg) = &content.svg {
        out.push((ClipFormat::Svg, svg.clone().into_bytes()));
    }
    if let Some(png) = &content.png {
        out.push((ClipFormat::Png, png.clone()));
        out.push((ClipFormat::Dib, png_to_dib(png)?));
    }
    if let Some(text) = &content.text {
        let bytes = text
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(|w| w.to_le_bytes())
            .collect();
        out.push((ClipFormat::UnicodeText, bytes));
    }
    Ok(out)
}

#[cfg_attr(not(windows), allow(dead_code))]
/// PNG を CF_DIB 用の DIB(BITMAPINFOHEADER + ピクセル)へ変換する。
pub(crate) fn png_to_dib(png: &[u8]) -> anyhow::Result<Vec<u8>> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)?;
    let mut bmp = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)?;
    // BITMAPFILEHEADER(14 バイト)を除くと DIB になる
    Ok(bmp.split_off(14))
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
#[serde(deny_unknown_fields)]
pub(crate) struct OpenBody {
    name: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 3, image::Rgba([10, 20, 30, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn text_only(t: &str) -> ClipContent {
        ClipContent {
            text: Some(t.into()),
            ..Default::default()
        }
    }

    #[test]
    fn text_and_default_content_types() {
        assert_eq!(parse_clip_body("", b"hi"), Ok(text_only("hi")));
        assert_eq!(
            parse_clip_body("text/plain; charset=utf-8", b"hi"),
            Ok(text_only("hi"))
        );
        assert!(parse_clip_body("text/plain", &[0xff, 0xfe]).is_err());
        assert!(parse_clip_body("application/pdf", b"x").is_err());
    }

    #[test]
    fn png_is_validated() {
        let png = tiny_png();
        assert_eq!(
            parse_clip_body("image/png", &png),
            Ok(ClipContent {
                png: Some(png.clone()),
                ..Default::default()
            })
        );
        assert!(parse_clip_body("image/png", b"not a png").is_err());
    }

    #[test]
    fn svg_is_validated_and_gets_text_fallback() {
        let svg = "<?xml version=\"1.0\"?>\n<!-- c -->\n<!DOCTYPE svg>\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        assert_eq!(
            parse_clip_body("Image/SVG+xml", svg.as_bytes()),
            Ok(ClipContent {
                text: Some(svg.into()),
                svg: Some(svg.into()),
                ..Default::default()
            })
        );
        assert!(parse_clip_body("image/svg+xml", b"<html></html>").is_err());
        assert!(parse_clip_body("image/svg+xml", b"<svgx/>").is_err());
        assert!(parse_clip_body("image/svg+xml", &[0xff]).is_err());
    }

    #[test]
    fn json_bundle_carries_all_representations() {
        use base64::Engine as _;
        let png = tiny_png();
        let body = serde_json::json!({
            "text": "alt",
            "svg": "<svg/>",
            "png": base64::engine::general_purpose::STANDARD.encode(&png),
        })
        .to_string();
        assert_eq!(
            parse_clip_body("application/json", body.as_bytes()),
            Ok(ClipContent {
                text: Some("alt".into()),
                png: Some(png),
                svg: Some("<svg/>".into()),
            })
        );
    }

    #[test]
    fn json_bundle_rejects_bad_input() {
        for body in [
            r#"{}"#,
            r#"{"png":"!!!"}"#,
            r#"{"png":"aGVsbG8="}"#,
            r#"{"svg":"<html/>"}"#,
            r#"{"other":"x"}"#,
            "not json",
        ] {
            assert!(
                parse_clip_body("application/json", body.as_bytes()).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn formats_cover_all_representations_in_one_set() {
        let content = ClipContent {
            text: Some("a".into()),
            png: Some(tiny_png()),
            svg: Some("<svg/>".into()),
        };
        let kinds: Vec<_> = clip_formats(&content)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            kinds,
            [
                ClipFormat::Svg,
                ClipFormat::Png,
                ClipFormat::Dib,
                ClipFormat::UnicodeText
            ]
        );
        let text = &clip_formats(&text_only("é")).unwrap()[0].1;
        assert_eq!(text, &[0xe9, 0, 0, 0]);
    }

    #[test]
    fn png_converts_to_dib() {
        let dib = png_to_dib(&tiny_png()).unwrap();
        let header = u32::from_le_bytes(dib[0..4].try_into().unwrap());
        // BITMAPINFOHEADER / V4 / V5
        assert!([40, 108, 124].contains(&header), "header size {header}");
        assert_eq!(i32::from_le_bytes(dib[4..8].try_into().unwrap()), 2);
        assert_eq!(i32::from_le_bytes(dib[8..12].try_into().unwrap()).abs(), 3);
    }
}
