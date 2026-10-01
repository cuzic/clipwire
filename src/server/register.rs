use super::*;

pub(crate) async fn handle_register(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !check_auth(&s.token, &headers) {
        return unauthorized();
    }

    #[derive(serde::Deserialize)]
    struct Req {
        name: String,
        #[serde(flatten)]
        target: StoredTarget,
    }

    let req: Req = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("JSON parse error: {e}\n")).into_response()
        }
    };

    if let Err(e) = validate_target_name(&req.name) {
        return (StatusCode::BAD_REQUEST, format!("{e}\n")).into_response();
    }

    if let Err(e) = validate_definition(&req.target) {
        return (StatusCode::BAD_REQUEST, format!("{e}\n")).into_response();
    }

    #[cfg(windows)]
    let entry_for_toast = req.target.clone();
    let result = match s
        .store
        .register(req.name.clone(), req.target, s.auto_approve)
        .await
    {
        Ok(result) => result,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("保存エラー: {e:#}\n"),
            )
                .into_response()
        }
    };

    if s.auto_approve {
        return (StatusCode::OK, format!("'{}' を登録しました\n", req.name)).into_response();
    }

    let msg = if result.reapproval {
        format!(
            "'{}' の設定が変更されました。Windows で clipwire approve {} を実行してください",
            req.name, req.name
        )
    } else {
        format!(
            "'{}' を承認待ちに追加しました。Windows で clipwire approve {} を実行してください",
            req.name, req.name
        )
    };
    #[cfg(windows)]
    win_clip::show_register_toast(
        req.name.clone(),
        entry_for_toast,
        s.config_dir.clone(),
        result.reapproval,
    );
    #[cfg(not(windows))]
    eprintln!("{msg}");

    (StatusCode::OK, format!("{msg}\n")).into_response()
}
