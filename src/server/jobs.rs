use super::*;

#[derive(Deserialize)]
pub(crate) struct JobsQuery {
    state: Option<crate::jobs::JobStatus>,
}

#[derive(Deserialize)]
pub(crate) struct LogQuery {
    #[serde(default)]
    pub(crate) offset: usize,
}

pub(crate) async fn list(
    State(state): State<AppState>,
    Query(query): Query<JobsQuery>,
) -> Response {
    axum::Json(state.jobs.list(query.state)).into_response()
}

pub(crate) async fn get_job(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    state.jobs.get(&id).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |job| axum::Json(job).into_response(),
    )
}

pub(crate) async fn log(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(query): Query<LogQuery>,
) -> Response {
    match state.jobs.read_log(&id, query.offset) {
        Ok(Some(bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(bytes))
            .unwrap(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{error}\n")).into_response(),
    }
}

pub(crate) async fn kill(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> Response {
    if serde_json::from_slice::<serde_json::Value>(&body).is_err() {
        return (StatusCode::BAD_REQUEST, "JSON parse error\n").into_response();
    }
    let Some(meta) = state.jobs.get(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match state.jobs.request_kill(&id) {
        crate::jobs::KillDecision::Accepted => {
            let mut event = crate::audit::AuditEvent::new(crate::audit::AuditEventKind::Kill)
                .target(&meta.target, &meta.def_hash);
            event.job_id = Some(id);
            state.audit.record(event);
            StatusCode::ACCEPTED.into_response()
        }
        crate::jobs::KillDecision::Missing => StatusCode::NOT_FOUND.into_response(),
        crate::jobs::KillDecision::Finished => {
            (StatusCode::CONFLICT, "ジョブはすでに終了しています\n").into_response()
        }
    }
}
