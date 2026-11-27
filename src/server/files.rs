//! `POST /files`: Linux 側から Windows の保存先(既定 Downloads)へファイルを送る(ADR-0012)。

use super::*;
use sha2::Digest as _;
use tokio::io::AsyncWriteExt as _;

pub(crate) const DEFAULT_FILES_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// UTF-16 単位で数える(NTFS の上限 255 は UTF-16 単位。` (999)` の分を残す)。
const MAX_NAME_UNITS: usize = 200;
const MAX_SUFFIX: u32 = 999;
/// 本文がこの時間届かなければ打ち切る(止まったクライアントが書きかけを残さないように)。
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// `POST /files` の設定(`serve --files-*`)。
#[derive(Clone, Debug)]
pub(crate) struct FilesConfig {
    pub(crate) dir: PathBuf,
    pub(crate) max_bytes: u64,
    pub(crate) mark_of_the_web: bool,
}

impl FilesConfig {
    pub(crate) fn new(dir: Option<PathBuf>, max_bytes: u64, mark_of_the_web: bool) -> Self {
        Self {
            dir: dir.unwrap_or_else(default_files_dir),
            max_bytes,
            mark_of_the_web,
        }
    }
}

/// Windows では Known Folder の Downloads。取れなければ `~/Downloads`。
pub(crate) fn default_files_dir() -> PathBuf {
    dirs_next::download_dir()
        .or_else(|| dirs_next::home_dir().map(|home| home.join("Downloads")))
        .unwrap_or_else(|| PathBuf::from("Downloads"))
}

/// Explorer がフォルダを表示するだけで解釈しうる種類(ADR-0012 §1)。
const SHELL_INTERPRETED_EXTENSIONS: &[&str] = &[
    "lnk",
    "url",
    "scf",
    "pif",
    "library-ms",
    "searchconnector-ms",
];

fn is_reserved_device_name(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let mut chars = stem.chars();
    let prefix: String = chars.by_ref().take(3).collect();
    let rest: String = chars.collect();
    (prefix == "COM" || prefix == "LPT")
        && rest.chars().count() == 1
        && rest
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, '¹' | '²' | '³'))
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// `name` が Explorer の解釈する種類か。NTFS は大文字化して比べるため、
/// `ſ`(→ S)や `ı`(→ I)のように小文字化では畳めない文字も考慮して両方向で比べる。
fn is_shell_interpreted(name: &str) -> bool {
    [name.to_lowercase(), name.to_uppercase().to_lowercase()]
        .iter()
        .any(|folded| {
            folded == "desktop.ini"
                || folded
                    .rsplit_once('.')
                    .is_some_and(|(_, e)| SHELL_INTERPRETED_EXTENSIONS.contains(&e))
        })
}

/// 保存するファイル名を検証する。違反なら理由を返す。
pub(crate) fn validate_upload_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("ファイル名が空です".into());
    }
    if name.encode_utf16().count() > MAX_NAME_UNITS {
        return Err(format!(
            "ファイル名が長すぎます(UTF-16 で {MAX_NAME_UNITS} 単位まで)"
        ));
    }
    if let Some(c) = name.chars().find(|c| c.is_control()) {
        return Err(format!("制御文字 U+{:04X} は使えません", c as u32));
    }
    if let Some(c) = name.chars().find(|c| is_bidi_control(*c)) {
        return Err(format!(
            "双方向テキストの制御文字 U+{:04X} は使えません",
            c as u32
        ));
    }
    if let Some(c) = name.chars().find(|c| r#"\/:*?"<>|"#.contains(*c)) {
        return Err(format!("文字 {c:?} は使えません"));
    }
    if name.starts_with('.') {
        return Err("先頭の '.' は使えません".into());
    }
    if name.contains("..") {
        return Err("'..' は使えません".into());
    }
    if name.starts_with(' ') || name.ends_with(' ') || name.ends_with('.') {
        return Err("先頭の空白、末尾の空白・'.' は使えません".into());
    }
    if is_reserved_device_name(name) {
        return Err("Windows の予約デバイス名は使えません".into());
    }
    if is_shell_interpreted(name) {
        return Err("Explorer が表示時に解釈する種類のファイル(.lnk, .url, .scf, .pif, .library-ms, .searchConnector-ms, desktop.ini)は受け付けません".into());
    }
    Ok(())
}

/// `n == 0` ならそのまま、それ以外は最後の `.` の前に ` (n)` を入れる。
pub(crate) fn numbered_name(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

/// CF_HDROP 用の DROPFILES 構造体 + UTF-16 のパス列(各 NUL 終端 + 最後に NUL)。
pub(crate) fn dropfiles_bytes(paths: &[String]) -> Vec<u8> {
    const HEADER_LEN: u32 = 20;
    let mut out = Vec::new();
    out.extend_from_slice(&HEADER_LEN.to_le_bytes()); // pFiles
    out.extend_from_slice(&0_i32.to_le_bytes()); // pt.x
    out.extend_from_slice(&0_i32.to_le_bytes()); // pt.y
    out.extend_from_slice(&0_i32.to_le_bytes()); // fNC
    out.extend_from_slice(&1_i32.to_le_bytes()); // fWide
    for path in paths {
        for unit in path.encode_utf16().chain(std::iter::once(0)) {
            out.extend_from_slice(&unit.to_le_bytes());
        }
    }
    out.extend_from_slice(&0_u16.to_le_bytes());
    out
}

pub(crate) fn zone_identifier_contents() -> &'static str {
    "[ZoneTransfer]\r\nZoneId=3\r\n"
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FilesQuery {
    name: String,
    #[serde(default)]
    clip: Option<String>,
}

fn wants_clip(value: Option<&str>) -> Result<bool, String> {
    match value {
        None | Some("0") | Some("false") => Ok(false),
        Some("1") | Some("true") | Some("") => Ok(true),
        Some(other) => Err(format!("clip は 1 か 0 を指定してください: {other:?}")),
    }
}

fn text_error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (status, format!("{message}\n")).into_response()
}

async fn create_unique(
    dir: &Path,
    name: &str,
) -> Result<(String, PathBuf, tokio::fs::File), Box<Response>> {
    for n in 0..=MAX_SUFFIX {
        let candidate = numbered_name(name, n);
        let path = dir.join(&candidate);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(file) => return Ok((candidate, path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(Box::new(text_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("保存できません: {}: {error}", path.display()),
                )))
            }
        }
    }
    Err(Box::new(text_error(
        StatusCode::CONFLICT,
        format!("同名のファイルが多すぎます: {name} ((1)〜({MAX_SUFFIX}) まで使用済み)"),
    )))
}

enum WriteError {
    TooLarge,
    Idle,
    Body(axum::Error),
    Io(io::Error),
}

async fn write_body(
    file: &mut tokio::fs::File,
    body: Body,
    max_bytes: u64,
) -> Result<(u64, String), WriteError> {
    let mut stream = body.into_data_stream();
    let mut hasher = sha2::Sha256::new();
    let mut total: u64 = 0;
    loop {
        let chunk = tokio::time::timeout(
            BODY_IDLE_TIMEOUT,
            std::future::poll_fn(|cx| {
                futures_core::Stream::poll_next(std::pin::Pin::new(&mut stream), cx)
            }),
        )
        .await
        .map_err(|_| WriteError::Idle)?;
        let Some(chunk) = chunk else { break };
        let chunk = chunk.map_err(WriteError::Body)?;
        total = total.saturating_add(chunk.len() as u64);
        if total > max_bytes {
            return Err(WriteError::TooLarge);
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(WriteError::Io)?;
    }
    file.flush().await.map_err(WriteError::Io)?;
    file.sync_all().await.map_err(WriteError::Io)?;
    Ok((total, hex::encode(hasher.finalize())))
}

/// 書きかけのファイル。成功して `keep` するまで、どの経路で抜けても(future が
/// 途中で捨てられても)消す。
struct PartialFile<'a> {
    path: &'a Path,
    keep: bool,
}

impl Drop for PartialFile<'_> {
    fn drop(&mut self) {
        if !self.keep {
            if let Err(error) = std::fs::remove_file(self.path) {
                warn!(
                    "書きかけのファイルを消せません({}): {error}",
                    self.path.display()
                );
            }
        }
    }
}

pub(crate) async fn handle_files_post(
    connect: Option<axum::extract::ConnectInfo<SocketAddr>>,
    State(s): State<AppState>,
    query: Result<Query<FilesQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(error) => {
            return text_error(
                StatusCode::BAD_REQUEST,
                format!("クエリが不正です(?name=<ファイル名>[&clip=1]): {error}"),
            )
        }
    };
    if let Err(message) = validate_upload_name(&query.name) {
        return text_error(
            StatusCode::BAD_REQUEST,
            format!("{message}: {:?}", query.name),
        );
    }
    let clip = match wants_clip(query.clip.as_deref()) {
        Ok(clip) => clip,
        Err(message) => return text_error(StatusCode::BAD_REQUEST, message),
    };
    let max = s.files.max_bytes;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|len| len > max) {
        return text_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("サイズの上限 {max} バイトを超えています"),
        );
    }
    if !s.files.dir.is_dir() {
        return text_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("保存先がありません: {}", s.files.dir.display()),
        );
    }

    let (saved_name, path, mut file) = match create_unique(&s.files.dir, &query.name).await {
        Ok(created) => created,
        Err(response) => return *response,
    };
    let mut partial = PartialFile {
        path: &path,
        keep: false,
    };
    // 本文を書く前に付ける(受信中や、書き終えてから付けるまでの間も MotW なしにしない)。
    // 本体を開いたままでも、Rust は共有モード READ|WRITE|DELETE で開くので ADS を書ける。
    if s.files.mark_of_the_web {
        write_mark_of_the_web(&path);
    }
    let written = write_body(&mut file, body, max).await;
    drop(file);
    let (bytes, sha256) = match written {
        Ok(written) => {
            partial.keep = true;
            written
        }
        Err(error) => {
            drop(partial);
            return match error {
                WriteError::TooLarge => text_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("サイズの上限 {max} バイトを超えています"),
                ),
                WriteError::Idle => text_error(
                    StatusCode::REQUEST_TIMEOUT,
                    format!(
                        "本文が {} 秒届かないため打ち切りました",
                        BODY_IDLE_TIMEOUT.as_secs()
                    ),
                ),
                WriteError::Body(error) => text_error(
                    StatusCode::BAD_REQUEST,
                    format!("受信が途中で切れました: {error}"),
                ),
                WriteError::Io(error) => text_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("書き込みに失敗しました: {error}"),
                ),
            };
        }
    };
    drop(partial);

    let path_str = path.to_string_lossy().into_owned();
    let mut clip_error = None;
    if clip {
        let (tx, rx) = oneshot::channel();
        let content = ClipContent {
            files: vec![path_str.clone()],
            ..Default::default()
        };
        clip_error = if s
            .clip_tx
            .send(ClipRequest::SetClip { content, reply: tx })
            .is_err()
        {
            Some("クリップボードのスレッドが停止しています".to_string())
        } else {
            match rx.await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(format!("{error:#}")),
                Err(_) => Some("クリップボードのスレッドが応答しません".to_string()),
            }
        };
    }

    let mut event = crate::audit::AuditEvent::new(crate::audit::AuditEventKind::FileReceive)
        .requester(connect.map(|value| value.0.ip().to_string()));
    event.file = Some(saved_name.clone());
    event.bytes = Some(bytes);
    event.sha256 = Some(sha256.clone());
    event.clip = Some(clip && clip_error.is_none());
    s.audit.record(event);
    info!(file = %path_str, bytes, "file received");

    #[cfg(windows)]
    win_clip::show_balloon(&format!(
        "ファイルを受信しました: {saved_name} ({})",
        human_size(bytes)
    ));
    #[cfg(not(windows))]
    let _ = human_size(bytes);

    let mut body = serde_json::json!({
        "name": saved_name,
        "path": path_str,
        "bytes": bytes,
        "sha256": sha256,
        "clipboard": clip && clip_error.is_none(),
    });
    if let Some(error) = clip_error {
        body["clip_error"] = error.into();
    }
    (StatusCode::CREATED, axum::Json(body)).into_response()
}

#[cfg(windows)]
fn write_mark_of_the_web(path: &Path) {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    if let Err(error) = std::fs::write(&stream, zone_identifier_contents()) {
        warn!("Zone.Identifier を書けません({}): {error}", path.display());
    }
}

#[cfg(not(windows))]
fn write_mark_of_the_web(_path: &Path) {
    let _ = zone_identifier_contents();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ac_t9_1_1_name_validation_rules() {
        for ok in [
            "slides.pptx",
            "報告書 2026.docx",
            "a.b.c",
            "noext",
            "COM10.txt",
            "console.log",
            "x.lnk.txt",
        ] {
            assert_eq!(validate_upload_name(ok), Ok(()), "{ok}");
        }
        let long = "a".repeat(201);
        let long_astral = "😀".repeat(101);
        for bad in [
            "",
            long.as_str(),
            long_astral.as_str(),
            "..",
            "a..b",
            "../x",
            "a/b",
            "a\\b",
            "C:x",
            "a\u{0}b",
            "a\nb",
            "a\u{7f}",
            "a\u{85}",
            ".hidden",
            " lead",
            "trail ",
            "trail.",
            "a*b",
            "a?b",
            "a\"b",
            "a<b",
            "a>b",
            "a|b",
            "CON",
            "nul.txt",
            "Com1.log",
            "LPT9",
            "COM¹",
            "aux .txt",
            "invoice\u{202E}fdp.exe",
            "a\u{2066}b",
            "x.lnk",
            "X.URL",
            "a.scf",
            "a.library-ms",
            "a.searchConnector-ms",
            "Desktop.ini",
            "a.pif",
            "x.\u{17F}cf",
            "de\u{17F}ktop.\u{131}ni",
            "a\u{061C}b",
        ] {
            assert!(validate_upload_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ac_t9_1_2_numbered_names() {
        assert_eq!(numbered_name("a.pptx", 0), "a.pptx");
        assert_eq!(numbered_name("a.pptx", 1), "a (1).pptx");
        assert_eq!(numbered_name("a.tar.gz", 2), "a.tar (2).gz");
        assert_eq!(numbered_name("noext", 3), "noext (3)");
    }

    #[test]
    fn ac_t9_1_6_dropfiles_layout() {
        let bytes = dropfiles_bytes(&["C:\\a".to_string()]);
        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 20);
        assert_eq!(i32::from_le_bytes(bytes[16..20].try_into().unwrap()), 1);
        let units: Vec<u16> = bytes[20..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let mut expected: Vec<u16> = "C:\\a".encode_utf16().collect();
        expected.extend([0, 0]);
        assert_eq!(units, expected);
    }

    #[test]
    fn clip_flag_values() {
        assert_eq!(wants_clip(None), Ok(false));
        assert_eq!(wants_clip(Some("1")), Ok(true));
        assert_eq!(wants_clip(Some("0")), Ok(false));
        assert!(wants_clip(Some("yes")).is_err());
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(100 * 1024), "100.0 KiB");
    }
}
