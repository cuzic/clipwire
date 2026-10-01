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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteId {
    Health,
    Root,
    Clip,
    File,
    VFile,
    Open,
    Exec,
    Register,
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RouteSpec {
    pub(crate) id: RouteId,
    pub(crate) path: &'static str,
    pub(crate) class: RouteClass,
    pub(crate) registered_mutation: RegisteredMutation,
}

pub(crate) const ROUTES: &[RouteSpec] = &[
    RouteSpec {
        id: RouteId::Health,
        path: "/health",
        class: RouteClass::Common,
        registered_mutation: RegisteredMutation::Never,
    },
    RouteSpec {
        id: RouteId::Root,
        path: "/",
        class: RouteClass::Clipboard,
        registered_mutation: RegisteredMutation::Never,
    },
    RouteSpec {
        id: RouteId::Clip,
        path: "/clip",
        class: RouteClass::Clipboard,
        registered_mutation: RegisteredMutation::Never,
    },
    RouteSpec {
        id: RouteId::File,
        path: "/file",
        class: RouteClass::Clipboard,
        registered_mutation: RegisteredMutation::Never,
    },
    RouteSpec {
        id: RouteId::VFile,
        path: "/vfile",
        class: RouteClass::Clipboard,
        registered_mutation: RegisteredMutation::Never,
    },
    RouteSpec {
        id: RouteId::Open,
        path: "/open",
        class: RouteClass::Clipboard,
        registered_mutation: RegisteredMutation::Never,
    },
    RouteSpec {
        id: RouteId::Exec,
        path: "/exec",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::Never,
    },
    RouteSpec {
        id: RouteId::Register,
        path: "/register",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::AutoApproveOnly,
    },
    RouteSpec {
        id: RouteId::TargetsCheck,
        path: "/targets/check",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::Never,
    },
    RouteSpec {
        id: RouteId::Jobs,
        path: "/jobs",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::Never,
    },
    RouteSpec {
        id: RouteId::Job,
        path: "/jobs/:id",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::Never,
    },
    RouteSpec {
        id: RouteId::JobLog,
        path: "/jobs/:id/log",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::Never,
    },
    RouteSpec {
        id: RouteId::JobKill,
        path: "/jobs/:id/kill",
        class: RouteClass::Protected,
        registered_mutation: RegisteredMutation::Never,
    },
];

pub(crate) fn build_router(state: AppState) -> Router {
    let mut common = Router::new();
    let mut clipboard = Router::new();
    let mut protected = Router::new();

    for route in ROUTES {
        let router = match route.id {
            RouteId::Health => Router::new().route(route.path, get(handle_health)),
            RouteId::Root => Router::new().route(route.path, get(handle_clip)),
            RouteId::Clip => {
                Router::new().route(route.path, get(handle_clip).post(handle_clip_post))
            }
            RouteId::File => Router::new().route(route.path, get(handle_file)),
            RouteId::VFile => Router::new().route(route.path, get(handle_vfile)),
            RouteId::Open => Router::new().route(route.path, get(handle_open)),
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
        }
    }

    // Axum layers are entered in reverse addition order. Authentication stays
    // innermost so malformed requests are rejected before credentials matter.
    protected = protected
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .route_layer(middleware::from_fn(require_json_content_type));

    common
        .merge(clipboard)
        .merge(protected)
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
