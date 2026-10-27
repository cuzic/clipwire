use super::*;
use crate::exec_rhai::exec_rhai_with_deadline_output;
use crate::jobs::{JobControl, JobStatus};
use crate::runner::{resolve_timeout, valid_timeout, JobSpec, OrderedOutput, Runner};
use std::sync::{atomic::Ordering, Arc};
use std::time::{Duration, Instant};

pub(crate) async fn handle_exec_http(
    connect: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    state: State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    handle_exec_with_ip(state, headers, body, connect.map(|v| v.0.ip().to_string())).await
}

#[cfg(test)]
pub(crate) async fn handle_exec(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    handle_exec_with_ip(State(state), headers, body, None).await
}

#[derive(serde::Deserialize)]
struct ExecRequest {
    name: String,
    timeout: Option<String>,
    #[serde(default)]
    detach: bool,
}

struct Prepared {
    request: ExecRequest,
    payload: ExecPayload,
    dir: Option<String>,
    timeout: Duration,
    def_hash: String,
    job_id: String,
}

async fn handle_exec_with_ip(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
    requester_ip: Option<String>,
) -> Response {
    let prepared = match prepare(&state, &body, requester_ip.clone()) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let detached = prepared.request.detach;
    let streaming = super::stream::accepts_ndjson(&headers) && !detached;
    let job_id = prepared.job_id.clone();
    if detached {
        state.jobs.set_detached(&job_id);
    }
    let control = Arc::new(JobControl::default());
    state.jobs.register_control(&job_id, Arc::clone(&control));
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(run_job(
        state.clone(),
        prepared,
        requester_ip,
        control,
        reply_tx,
    ));

    if detached {
        return (
            StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({ "id": job_id })),
        )
            .into_response();
    }
    if streaming {
        return super::stream::follow_response(state, job_id, 0);
    }
    match reply_rx.await {
        Ok(Ok((output, code))) => exec_response(output, code),
        Ok(Err(message)) => (StatusCode::INTERNAL_SERVER_ERROR, message).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "ジョブ応答が失われました\n",
        )
            .into_response(),
    }
}

fn prepare(
    state: &AppState,
    body: &[u8],
    requester_ip: Option<String>,
) -> Result<Prepared, Box<Response>> {
    let request: ExecRequest = serde_json::from_slice(body).map_err(|error| {
        Box::new(
            (
                StatusCode::BAD_REQUEST,
                format!("JSON parse error: {error}\n"),
            )
                .into_response(),
        )
    })?;
    validate_target_name(&request.name).map_err(|error| {
        Box::new((StatusCode::BAD_REQUEST, format!("{error}\n")).into_response())
    })?;
    let stored = match state.store.verified_target(&request.name) {
        Ok(Some(target)) => target,
        Ok(None) => {
            let pending = load_target_map_or_warn(&state.config_dir.join("pending.toml"));
            if pending.contains_key(&request.name) {
                return Err(Box::new(
                    (
                        StatusCode::CONFLICT,
                        format!(
                        "'{}' は承認待ちです。Windows で clipwire approve {} を実行してください\n",
                        request.name, request.name
                    ),
                    )
                        .into_response(),
                ));
            }
            return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
        }
        Err(error) => {
            return Err(Box::new(
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("承認検証エラー: {error:#}\n"),
                )
                    .into_response(),
            ))
        }
    };
    let definition_text = stored.timeout.clone().unwrap_or_else(|| "30m".into());
    let definition_timeout = match stored.timeout.as_deref().map(humantime::parse_duration) {
        Some(Ok(value)) if valid_timeout(value) => Some(value),
        Some(_) => {
            return Err(Box::new(
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "登録済み timeout が不正です\n",
                )
                    .into_response(),
            ))
        }
        None => None,
    };
    let requested_timeout = match request.timeout.as_deref().map(humantime::parse_duration) {
        Some(Ok(value)) if valid_timeout(value) => Some(value),
        Some(_) => {
            return Err(Box::new(
                (StatusCode::BAD_REQUEST, "timeout が不正です\n").into_response(),
            ))
        }
        None => None,
    };
    let timeout = resolve_timeout(definition_timeout, requested_timeout).map_err(|_| {
        Box::new(
            (
                StatusCode::BAD_REQUEST,
                format!("要求 timeout はサーバー側の定義値 {definition_text} を超えています\n"),
            )
                .into_response(),
        )
    })?;
    let def_hash = stored
        .hash
        .clone()
        .unwrap_or_else(|| crate::config::definition_hash(&crate::config::canonical_json(&stored)));
    let concurrency = stored
        .concurrency
        .unwrap_or(crate::config::Concurrency::Reject);
    let (dir, payload) = stored.into_exec().map_err(|error| {
        Box::new((StatusCode::INTERNAL_SERVER_ERROR, format!("{error}\n")).into_response())
    })?;
    let job_id = state
        .jobs
        .start(&request.name, &def_hash, requester_ip, concurrency)
        .map_err(|crate::jobs::Conflict(id)| {
            Box::new(
                (
                    StatusCode::CONFLICT,
                    format!("ターゲット '{}' はジョブ {id} で実行中です\n", request.name),
                )
                    .into_response(),
            )
        })?;
    for lost in state.jobs.take_lost_events() {
        let mut event = crate::audit::AuditEvent::new(crate::audit::AuditEventKind::Lost)
            .target(&lost.target, &lost.def_hash);
        event.job_id = Some(lost.id);
        state.audit.record(event);
    }
    Ok(Prepared {
        request,
        payload,
        dir,
        timeout,
        def_hash,
        job_id,
    })
}

async fn run_job(
    state: AppState,
    prepared: Prepared,
    requester_ip: Option<String>,
    control: Arc<JobControl>,
    reply: tokio::sync::oneshot::Sender<Result<(Vec<u8>, i32), String>>,
) {
    let Prepared {
        request,
        payload,
        dir,
        timeout,
        def_hash,
        job_id,
    } = prepared;
    let started = Instant::now();
    let mut start = crate::audit::AuditEvent::new(crate::audit::AuditEventKind::Start)
        .target(&request.name, &def_hash)
        .requester(requester_ip.clone());
    start.job_id = Some(job_id.clone());
    start.args = Some(request.timeout.into_iter().collect());
    state.audit.record(start);

    let cancel = control.cancelled();
    let deadline_cancel = Arc::clone(&cancel);
    let deadline = tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        deadline_cancel.store(true, Ordering::Release);
    });
    let registry = state.jobs.clone();
    let running_id = job_id.clone();
    let output_pair = OrderedOutput::new_with_log(&state.jobs.log_path(&job_id));
    let result = tokio::task::spawn_blocking(move || match payload {
        ExecPayload::Steps { steps, env } => {
            let (output, relay) = output_pair.map_err(|e| e.to_string())?;
            let runner =
                Runner::with_output(std::env::temp_dir().join("clipwire-runner"), output.clone());
            let mut exit_code = 0;
            for args in steps.into_argv() {
                if cancel.load(Ordering::Acquire) {
                    exit_code = 124;
                    break;
                }
                if args.is_empty() {
                    continue;
                }
                let mut spec = JobSpec::new(&args[0], "http");
                spec.args = args[1..].iter().map(Into::into).collect();
                spec.cwd = dir.as_ref().map(Into::into);
                spec.env = env.iter().map(|(k, v)| (k.into(), v.into())).collect();
                let mut job = runner.spawn(spec).map_err(|e| e.to_string())?;
                if let Some(error) = job.spawn_error() {
                    return Err(format!("実行エラー: {error}"));
                }
                registry.set_child(
                    &running_id,
                    job.child_identity().map_err(|e| e.to_string())?,
                );
                job.wait_cancelable(&cancel).map_err(|e| e.to_string())?;
                registry.set_child(&running_id, None);
                exit_code = job.exit_code().unwrap_or(-1);
                if exit_code != 0 {
                    break;
                }
            }
            drop(runner);
            output
                .finish(relay)
                .map(|bytes| (bytes, exit_code))
                .map_err(|e| e.to_string())
        }
        ExecPayload::Script { script } => {
            let callback_registry = registry.clone();
            let callback_id = running_id.clone();
            let child_changed =
                Arc::new(move |child| callback_registry.set_child(&callback_id, child));
            let (output, relay) = output_pair.map_err(|e| e.to_string())?;
            exec_rhai_with_deadline_output(
                &script,
                dir.as_deref(),
                cancel,
                child_changed,
                output,
                relay,
            )
            .map_err(|e| e.to_string())
        }
    })
    .await;
    deadline.abort();
    let result = match result {
        Ok(value) => value,
        Err(error) => Err(format!("thread panic: {error}")),
    };
    let killed = control.was_killed();
    let (output, code) = match &result {
        Ok((output, code)) => (output.clone(), *code),
        Err(message) => (format!("{message}\n").into_bytes(), -1),
    };
    if let Err(error) = state.jobs.write_log(&job_id, &output) {
        tracing::error!(job_id = %job_id, "ジョブログの保存に失敗しました: {error}");
    }
    let final_state = if killed {
        JobStatus::Killed
    } else if code == 0 {
        JobStatus::Succeeded
    } else if code == 124 {
        JobStatus::Timeout
    } else {
        JobStatus::Failed
    };
    state.jobs.finish(&job_id, final_state, Some(code));
    if code == 124 && !killed {
        record_event(
            &state,
            crate::audit::AuditEventKind::Timeout,
            (&job_id, &request.name, &def_hash),
            requester_ip.clone(),
            code,
            started.elapsed(),
        );
    }
    record_event(
        &state,
        crate::audit::AuditEventKind::End,
        (&job_id, &request.name, &def_hash),
        requester_ip,
        code,
        started.elapsed(),
    );
    let _ = reply.send(result);
}

fn record_event(
    state: &AppState,
    kind: crate::audit::AuditEventKind,
    job: (&str, &str, &str),
    requester: Option<String>,
    code: i32,
    elapsed: Duration,
) {
    let (id, target, hash) = job;
    let mut event = crate::audit::AuditEvent::new(kind)
        .target(target, hash)
        .requester(requester);
    event.job_id = Some(id.to_string());
    event.exit_code = Some(code);
    event.duration_ms = Some(crate::audit::duration_millis(elapsed));
    state.audit.record(event);
}

pub(crate) fn exec_response(body: Vec<u8>, exit_code: i32) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("X-Exit-Code", exit_code.to_string())
        .header(
            "X-Job-State",
            if exit_code == 124 {
                "timeout"
            } else {
                "completed"
            },
        )
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}
