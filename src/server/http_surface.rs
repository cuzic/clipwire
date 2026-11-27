use super::*;
use axum::{
    extract::{Request, State},
    middleware::Next,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteClass {
    Common,
    Clipboard,
    Protected,
    /// 保護ルートと同じ認証。本文は `application/octet-stream`(ADR-0012 §2)
    Upload,
}

impl RouteClass {
    pub(crate) const fn auth_label(self) -> &'static str {
        match self {
            Self::Common => "none",
            Self::Clipboard => "token-if-configured",
            Self::Protected | Self::Upload => "protected",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteId {
    Health,
    Info,
    Root,
    Clip,
    File,
    VFile,
    Open,
    Files,
    Exec,
    Register,
    Unregister,
    Targets,
    TargetsCheck,
    Jobs,
    Job,
    JobLog,
    JobKill,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegisteredMutation {
    Never,
    AutoApproveOnly,
    /// 削除だけ(権限を減らす操作。ADR-0012 §5)
    RemoveOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RouteSpec {
    pub(crate) id: RouteId,
    pub(crate) methods: &'static str,
    pub(crate) path: &'static str,
    pub(crate) class: RouteClass,
    pub(crate) registered_mutation: RegisteredMutation,
    pub(crate) summary: &'static str,
}

pub(crate) const ROUTES: &[RouteSpec] = &[
    RouteSpec {
        id: RouteId::Health,
        methods: "GET",
        path: "/health",
        class: RouteClass::Common,
        registered_mutation: RegisteredMutation::Never,
        summary: "生存確認 (Accept: application/json で version)",
    },
    RouteSpec {
        id: RouteId::Info,
        methods: "GET",
        path: "/info",
        class: RouteClass::Common,
        registered_mutation: RegisteredMutation::Never,
        summary: "app 名・version・features・エンドポイント一覧",
    },
    RouteSpec {
        id: RouteId::Root,
        methods: "GET",
        path: "/",
        class: RouteClass::Clipboard,
        registered_mutation: RegisteredMutation::Never,
        summary: "クリップボードの内容 (GET /clip と同じ)",
    },
    RouteSpec {
        id: RouteId::Clip,
        methods: "GET, POST",
        path: "/clip",
        class: RouteClass::Clipboard,
        registered_mutation: RegisteredMutation::Never,
        summary: "クリップボードの取得 / 書き込み (text, image/png, image/svg+xml, JSON バンドル)",
    },
    RouteSpec {
        id: RouteId::File,
        methods: "GET",
        path: "/file",
        class: RouteClass::Clipboard,
        registered_mutation: RegisteredMutation::Never,
        summary: "直前の GET /clip で返したファイルの取得 (?path=)",
    },
    RouteSpec {
        id: RouteId::VFile,
        methods: "GET",
        path: "/vfile",
        class: RouteClass::Clipboard,
        registered_mutation: RegisteredMutation::Never,
        summary: "仮想ファイルの取得 (?i=)",
    },
    RouteSpec {
        id: RouteId::Open,
        methods: "POST",
        path: "/open",
        class: RouteClass::Clipboard,
        registered_mutation: RegisteredMutation::Never,
        summary: "既定ブラウザで既知のサービスを開く ({\"name\"})",
    },
    RouteSpec {
        id: RouteId::Files,
        methods: "POST",
        path: "/files",
        class: RouteClass::Upload,
        registered_mutation: RegisteredMutation::Never,
        summary:
            "ファイルを保存先 (既定 Downloads) に保存 (application/octet-stream, ?name=&clip=1)",
    },
    RouteSpec {
        id: RouteId::Exec,
        methods: "POST",
        path: "/exec",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::Never,
        summary: "登録済みターゲットを実行",
    },
    RouteSpec {
        id: RouteId::Register,
        methods: "POST",
        path: "/register",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::AutoApproveOnly,
        summary: "ターゲットを登録 (承認待ち、または自動承認)",
    },
    RouteSpec {
        id: RouteId::Unregister,
        methods: "DELETE",
        path: "/register/:name",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::RemoveOnly,
        summary: "ターゲットの登録を削除 (registered と pending)",
    },
    RouteSpec {
        id: RouteId::Targets,
        methods: "GET",
        path: "/targets",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::Never,
        summary: "登録済み・承認待ちターゲットの一覧 (本文なし)",
    },
    RouteSpec {
        id: RouteId::TargetsCheck,
        methods: "POST",
        path: "/targets/check",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::Never,
        summary: "ローカル定義と登録状態の比較",
    },
    RouteSpec {
        id: RouteId::Jobs,
        methods: "GET",
        path: "/jobs",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::Never,
        summary: "ジョブ一覧",
    },
    RouteSpec {
        id: RouteId::Job,
        methods: "GET",
        path: "/jobs/:id",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::Never,
        summary: "ジョブの状態",
    },
    RouteSpec {
        id: RouteId::JobLog,
        methods: "GET",
        path: "/jobs/:id/log",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::Never,
        summary: "ジョブのログ (?follow=1 で NDJSON)",
    },
    RouteSpec {
        id: RouteId::JobKill,
        methods: "POST",
        path: "/jobs/:id/kill",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::Never,
        summary: "ジョブの終了",
    },
];

pub(crate) fn build_router(state: AppState) -> Router {
    let mut common = Router::new();
    let mut clipboard = Router::new();
    let mut protected = Router::new();
    let mut upload = Router::new();

    for route in ROUTES {
        let router = match route.id {
            RouteId::Health => Router::new().route(route.path, get(handle_health)),
            RouteId::Info => Router::new().route(route.path, get(handle_info)),
            RouteId::Files => Router::new().route(
                route.path,
                post(super::files::handle_files_post)
                    .layer(axum::extract::DefaultBodyLimit::disable()),
            ),
            RouteId::Unregister => Router::new().route(
                route.path,
                axum::routing::delete(super::register::handle_unregister),
            ),
            RouteId::Targets => {
                Router::new().route(route.path, get(super::register::handle_targets_list))
            }
            RouteId::Root => Router::new().route(route.path, get(handle_clip)),
            RouteId::Clip => {
                Router::new().route(route.path, get(handle_clip).post(handle_clip_post))
            }
            RouteId::File => Router::new().route(route.path, get(handle_file)),
            RouteId::VFile => Router::new().route(route.path, get(handle_vfile)),
            RouteId::Open => Router::new()
                .route(route.path, post(handle_open_post))
                .route_layer(middleware::from_fn(require_json_content_type)),
            RouteId::Exec => Router::new().route(route.path, post(super::exec::handle_exec_http)),
            RouteId::Register => {
                Router::new().route(route.path, post(super::register::handle_register_http))
            }
            RouteId::TargetsCheck => Router::new().route(
                route.path,
                post(handle_targets_check)
                    .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024)),
            ),
            RouteId::Jobs => Router::new().route(route.path, get(super::jobs::list)),
            RouteId::Job => Router::new().route(route.path, get(super::jobs::get_job)),
            RouteId::JobLog => Router::new().route(route.path, get(super::jobs::log)),
            RouteId::JobKill => Router::new().route(route.path, post(super::jobs::kill)),
        };
        match route.class {
            RouteClass::Common => common = common.merge(router),
            RouteClass::Clipboard => clipboard = clipboard.merge(router),
            RouteClass::Protected => protected = protected.merge(router),
            RouteClass::Upload => upload = upload.merge(router),
        }
    }

    // Axum layers are entered in reverse addition order. Authentication stays
    // innermost so malformed requests are rejected before credentials matter.
    protected = protected
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .route_layer(middleware::from_fn(require_json_content_type));
    upload = upload
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .route_layer(middleware::from_fn(require_octet_stream_content_type));

    common
        .merge(clipboard)
        .merge(protected)
        .merge(upload)
        .fallback(handle_not_found)
        .with_state(state.clone())
        .layer(middleware::from_fn(reject_origin))
        .layer(middleware::from_fn_with_state(state.clone(), require_host))
}

async fn require_host(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Err(message) = check_host_header(&state.host_policy, request.headers()) {
        if state.host_policy.mode == HostCheckMode::Enforce {
            return (StatusCode::MISDIRECTED_REQUEST, message).into_response();
        }
    }
    next.run(request).await
}

pub(crate) fn check_host_header(policy: &HostPolicy, headers: &HeaderMap) -> Result<(), String> {
    let raw = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    let normalized = raw.and_then(|value| {
        value
            .parse::<axum::http::uri::Authority>()
            .ok()
            .and_then(|authority| normalize_host(authority.host()))
    });
    if normalized
        .as_ref()
        .is_some_and(|host| policy.allowed.contains(host))
    {
        return Ok(());
    }

    let shown = raw.unwrap_or("<missing>");
    warn!(host = %shown, "Host header is not in the allowlist");
    Err(format!(
        "Host is not allowed. Allowed hosts: {}. Add a name with --allow-host <host>.\n",
        policy
            .allowed
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn normalize_host(host: &str) -> Option<String> {
    let host = host.trim().trim_end_matches('.');
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

async fn reject_origin(request: Request, next: Next) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return (StatusCode::FORBIDDEN, "Origin header is not allowed\n").into_response();
    }
    next.run(request).await
}

async fn require_json_content_type(request: Request, next: Next) -> Response {
    if request.method() != axum::http::Method::POST {
        return next.run(request).await;
    }
    let is_json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "application/json required\n",
        )
            .into_response();
    }
    next.run(request).await
}

async fn require_octet_stream_content_type(request: Request, next: Next) -> Response {
    let ok = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| {
            value
                .trim()
                .eq_ignore_ascii_case("application/octet-stream")
        });
    if !ok {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "application/octet-stream required\n",
        )
            .into_response();
    }
    next.run(request).await
}

/// `GET /info` の `features`(人間とエージェント向けの説明。互換ネゴシエーションではない)。
pub(crate) const FEATURES: &[&str] = &[
    "clip",
    "clip-png",
    "clip-svg",
    "files",
    "files-clip",
    "open",
    "exec",
    "exec-stream",
    "jobs",
    "targets",
    "unregister",
];

pub(crate) fn endpoints_json() -> serde_json::Value {
    ROUTES
        .iter()
        .map(|route| {
            serde_json::json!({
                "method": route.methods,
                "path": route.path,
                "auth": route.class.auth_label(),
                "summary": route.summary,
            })
        })
        .collect()
}

pub(crate) fn endpoints_text() -> String {
    let width = ROUTES.iter().map(|r| r.methods.len()).max().unwrap_or(0);
    ROUTES
        .iter()
        .map(|route| {
            format!(
                "  {:<width$}  {:<16} [{}] {}\n",
                route.methods,
                route.path,
                route.class.auth_label(),
                route.summary
            )
        })
        .collect()
}

async fn handle_info(State(state): State<AppState>) -> Response {
    axum::Json(serde_json::json!({
        "app": "clipwire",
        "version": env!("CARGO_PKG_VERSION"),
        "features": FEATURES,
        "endpoints": endpoints_json(),
        "files": { "max_bytes": state.files.max_bytes },
    }))
    .into_response()
}

async fn handle_not_found(uri: axum::http::Uri) -> Response {
    (
        StatusCode::NOT_FOUND,
        format!(
            "clipwire {}: {} はありません。使えるエンドポイント:\n{}詳細は GET /info (JSON)。\n",
            env!("CARGO_PKG_VERSION"),
            uri.path(),
            endpoints_text()
        ),
    )
        .into_response()
}

async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if state.token.is_none() {
        if state.allow_no_token {
            return next.run(request).await;
        }
        return (
            StatusCode::FORBIDDEN,
            "Protected routes require authentication; restart with --token-file <path> or explicitly opt in with --allow-no-token\n",
        )
            .into_response();
    }
    if !check_auth(&state.token, request.headers()) {
        return unauthorized();
    }
    next.run(request).await
}
