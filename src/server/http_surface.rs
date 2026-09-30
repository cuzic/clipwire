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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RouteSpec {
    pub(crate) id: RouteId,
    pub(crate) path: &'static str,
    pub(crate) class: RouteClass,
}

pub(crate) const ROUTES: &[RouteSpec] = &[
    RouteSpec {
        id: RouteId::Health,
        path: "/health",
        class: RouteClass::Common,
    },
    RouteSpec {
        id: RouteId::Root,
        path: "/",
        class: RouteClass::Clipboard,
    },
    RouteSpec {
        id: RouteId::Clip,
        path: "/clip",
        class: RouteClass::Clipboard,
    },
    RouteSpec {
        id: RouteId::File,
        path: "/file",
        class: RouteClass::Clipboard,
    },
    RouteSpec {
        id: RouteId::VFile,
        path: "/vfile",
        class: RouteClass::Clipboard,
    },
    RouteSpec {
        id: RouteId::Open,
        path: "/open",
        class: RouteClass::Clipboard,
    },
    RouteSpec {
        id: RouteId::Exec,
        path: "/exec",
        class: RouteClass::Protected,
    },
    RouteSpec {
        id: RouteId::Register,
        path: "/register",
        class: RouteClass::Protected,
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
            RouteId::Exec => Router::new().route(route.path, post(handle_exec)),
            RouteId::Register => Router::new().route(route.path, post(handle_register)),
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
        .with_state(state)
        .layer(middleware::from_fn(reject_origin))
        .layer(middleware::from_fn(require_host))
}

async fn require_host(request: Request, next: Next) -> Response {
    let valid = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.trim().is_empty());
    if !valid {
        return (StatusCode::MISDIRECTED_REQUEST, "Host header is required\n").into_response();
    }
    next.run(request).await
}

async fn reject_origin(request: Request, next: Next) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return (StatusCode::FORBIDDEN, "Origin header is not allowed\n").into_response();
    }
    next.run(request).await
}

async fn require_json_content_type(request: Request, next: Next) -> Response {
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
    if !check_auth(&state.token, request.headers()) {
        return unauthorized();
    }
    next.run(request).await
}
