# ADR-0005: `list` / `status` コマンド

- Status: Proposed (改訂: Opus レビュー反映。`list` と `status` を分割)
- Date: 2026-09-30
- Depends on: ADR-0010(サーバ計算のハッシュ)。`status` のみ ADR-0002 にも依存。

## Context

Linux 側の `targets.toml` と Windows 側の `registered.toml` / `pending.toml` は別管理で、同期状態を確認する手段がない。`exec` の 404/409 で初めて気づく。

## Decision

### `list`(早期に実装可能)

1. サーバに `POST /targets/check` を追加(トークン必須)。リクエスト: ローカル `targets.toml` の各定義(`name` と正規形)。レスポンス: 名前ごとの状態のみ(定義本文は返さない)。**ハッシュはサーバだけが計算する**(ADR-0010。Linux と Windows が独立に計算しない)。
2. 状態:
   | 状態 | 意味 |
   |---|---|
   | `ok` | ローカル定義のハッシュが registered と一致 |
   | `changed` | registered に同名があるがハッシュ不一致(再 register・再承認が必要) |
   | `pending` | 承認待ち(ハッシュ一致 / 不一致を併記) |
   | `unregistered` | ローカルにのみ存在 |
   | `remote-only` | Windows 側にのみ存在(ローカルに定義なし) |
3. `GET /targets` は名前・状態・承認時刻のみを返す(`dir` も返さない。本文・dir は承認者が Windows 側で見るもの)。
4. `clipwire list [--json]`。`--register-missing` は付けない(意図しない toast を生まないため)。
5. 定義の正規化・ハッシュの仕様は ADR-0010 に従う(`BTreeMap`、未設定値は非シリアライズ、`approve --dir` 廃止)。

### `status`(ADR-0002 の後)

6. `clipwire status` は `list` に加え、実行中ジョブ(ADR-0002)とサーバのバージョン/proto/features(ADR-0010 の `/health`)を表示する。稼働時間などの追加情報は必要になってから。

## Consequences

- 「なぜ exec できないか」が事前に分かる。ハッシュが決定的なため誤判定がない。
- サーバ計算方式により、Linux/Windows のバージョン不一致でもハッシュが食い違わない。
- `GET /targets` も認証必須。ターゲット名一覧も情報として機微になりうる。

## Alternatives considered

- **本文も返して diff 表示**: 露出が増える。承認前の差分は ADR-0006 の Windows 側 UI に任せる。
- **クライアントでハッシュ計算**: バージョン不一致で一致しないため却下。
