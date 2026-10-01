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

    let entry = req.target;
    let pending_path = s.config_dir.join("pending.toml");
    let registered_path = s.config_dir.join("registered.toml");
    let mut registered = load_target_map_or_warn(&registered_path);

    if s.auto_approve {
        registered.insert(req.name.clone(), entry);
        if let Err(e) = save_target_map(&registered_path, &registered) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("保存エラー: {e}\n"),
            )
                .into_response();
        }
        return (StatusCode::OK, format!("'{}' を登録しました\n", req.name)).into_response();
    }

    // 通常フロー: pending に追加、既承認分は取り消し
    let reapproval = registered.remove(&req.name).is_some();
    let mut pending = load_target_map_or_warn(&pending_path);
    #[cfg(windows)]
    let entry_for_toast = entry.clone();
    pending.insert(req.name.clone(), entry);

    if let Err(e) = save_target_map(&pending_path, &pending)
        .and_then(|_| save_target_map(&registered_path, &registered))
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("保存エラー: {e}\n"),
        )
            .into_response();
    }

    let msg = if reapproval {
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
        reapproval,
    );
    #[cfg(not(windows))]
    eprintln!("{msg}");

    (StatusCode::OK, format!("{msg}\n")).into_response()
}
