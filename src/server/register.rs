use super::*;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetDefinition {
    dir: Option<String>,
    script: Option<String>,
    steps: Option<StepsDef>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
}

impl From<TargetDefinition> for StoredTarget {
    fn from(value: TargetDefinition) -> Self {
        Self {
            dir: value.dir,
            script: value.script,
            steps: value.steps,
            env: value.env,
            ..Self::default()
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NestedRegisterRequest {
    name: String,
    target: TargetDefinition,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FlatRegisterRequest {
    name: String,
    dir: Option<String>,
    script: Option<String>,
    steps: Option<StepsDef>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
}

fn parse_register_request(body: &[u8]) -> Result<(String, StoredTarget), String> {
    match serde_json::from_slice::<NestedRegisterRequest>(body) {
        Ok(request) => Ok((request.name, request.target.into())),
        Err(nested_error) => match serde_json::from_slice::<FlatRegisterRequest>(body) {
            Ok(request) => Ok((
                request.name,
                TargetDefinition {
                    dir: request.dir,
                    script: request.script,
                    steps: request.steps,
                    env: request.env,
                }
                .into(),
            )),
            Err(flat_error) => Err(format!(
                "nested form: {nested_error}; flat form: {flat_error}"
            )),
        },
    }
}

pub(crate) async fn handle_register(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }

    let (name, target) = match parse_register_request(&body) {
        Ok(request) => request,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("JSON parse error: {e}\n")).into_response()
        }
    };

    if let Err(e) = validate_target_name(&name) {
        return (StatusCode::BAD_REQUEST, format!("{e}\n")).into_response();
    }

    if let Err(e) = validate_definition(&target) {
        return (StatusCode::BAD_REQUEST, format!("{e}\n")).into_response();
    }

    #[cfg(windows)]
    let entry_for_toast = target.clone();
    let result = match s.store.register(name.clone(), target, s.auto_approve).await {
        Ok(result) => result,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("保存エラー: {e:#}\n"),
            )
                .into_response()
        }
    };

    let hash_line = format!("hash: {}\n", result.hash);

    if result.unchanged {
        return (
            StatusCode::OK,
            format!("'{}' は変更なし\n{hash_line}", name),
        )
            .into_response();
    }

    if s.auto_approve {
        return (
            StatusCode::OK,
            format!("'{}' を登録しました\n{hash_line}", name),
        )
            .into_response();
    }

    let msg = if result.reapproval {
        format!(
            "'{}' の設定が変更されました。Windows で clipwire approve {} を実行してください",
            name, name
        )
    } else {
        format!(
            "'{}' を承認待ちに追加しました。Windows で clipwire approve {} を実行してください",
            name, name
        )
    };
    #[cfg(windows)]
    if result.redisplay_required {
        win_clip::show_register_toast(
            name.clone(),
            entry_for_toast,
            s.config_dir.clone(),
            result.reapproval,
        );
    }
    #[cfg(not(windows))]
    eprintln!("{msg}");

    (StatusCode::OK, format!("{msg}\n{hash_line}")).into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckRequest {
    targets: std::collections::BTreeMap<String, TargetDefinition>,
}

pub(crate) async fn handle_targets_check(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    let request: CheckRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("JSON parse error: {error}\n"),
            )
                .into_response()
        }
    };
    let targets = request
        .targets
        .into_iter()
        .map(|(name, target)| (name, target.into()))
        .collect();
    match state.store.check_targets(targets).await {
        Ok(statuses) => axum::Json(serde_json::json!({ "targets": statuses })).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("読み込みエラー: {error:#}\n"),
        )
            .into_response(),
    }
}
