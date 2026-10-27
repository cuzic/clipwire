# ADR-0002: ジョブ管理 (detach / jobs / logs / kill)

- Status: Proposed (改訂2: Opus 再レビュー反映)
- Date: 2026-09-30
- Depends on: ADR-0009(ランナー/ログ/Job Object), ADR-0011(認証前提), ADR-0010(ハッシュ)

## Context

`exec` は HTTP リクエストの寿命に縛られる。`awase-build` のような破壊的ターゲットを連打すると処理が並走して壊れる(排他がない)。ログの正本とプロセス管理は ADR-0009 が担うため、本 ADR は**ジョブのライフサイクルと API**に絞る。

## Decision

1. `POST /exec` に `detach: true` を追加。ジョブ ID(時刻順ソート可能な ULID 等)を即座に返す。`clipwire exec <name> --detach`。通常の `exec` も内部的にはジョブとして記録する(一覧・監査で見える)。
2. ジョブ状態は `%LOCALAPPDATA%\clipwire\jobs\<id>\meta.json`(`target`, `def_hash`, 開始/終了時刻, 状態 `running|orphaned|succeeded|failed|timeout|killed|lost`, 終了コード, 要求元)。ログは ADR-0009 の `log`。
3. API(すべてトークン必須。ADR-0011):
   - `GET /jobs` (新しい順、`?state=running`)
   - `GET /jobs/:id`
   - `GET /jobs/:id/log?offset=N[&follow=1]` (follow は ADR-0001 と同じ NDJSON)
   - `POST /jobs/:id/kill` (`Content-Type: application/json` 必須)
4. CLI: `clipwire jobs`, `clipwire logs <id> [-f]`, `clipwire kill <id>`。
5. **排他**: target 定義の `concurrency = "reject" | "allow"`(既定 `reject`)。`reject` で同名ジョブが `running` なら `409`(実行中のジョブ ID を返す)。`queue` は当初から**提供しない**(TTL 切れ・再 register・サーバ再起動との相互作用が未定義になるため)。`concurrency` は承認対象内容に含める(ADR-0010 の正規形)。
6. 「running」の定義・プロセスツリー kill・自己更新の扱いは ADR-0009 §3 に従う。
7. **サーバ再起動**: meta に、**その時点で実行中の子**(Steps の現在のステップ、Rhai の現在の `run`)の PID とプロセス作成時刻を記録し、子が変わるたびに更新する(Rhai では main の PID がサーバ自身になり、再起動後は必ず死んでいるため、子を記録する)。`orphaned` は、`reject` の判定のたびに生存を確認し、死んでいれば `lost` にする(永久に 409 を返し続けない)。状態の定義はこの ADR が持つ。起動時に `running` のまま残ったメタについて、そのプロセスが**生きていれば `orphaned`**(`reject` の判定では `running` と同じに扱う。旧ビルドと新ビルドの並走を防ぐため)、死んでいれば `lost` にする(サーバ停止でリレーが消えるため、子は出力で `EPIPE` を受けて終わる場合が多い。ADR-0009 §1)。
8. **保持**: 直近 50 件を超えた古いジョブディレクトリを、起動時と新規ジョブ時に削除する(日数による保持は設けない)。`running` / `orphaned` は削除対象から除外する。古いものから消すが、失敗(`failed` / `timeout`)したジョブと `--detach` したジョブは、成功した通常 exec より後に消す(短い exec の頻発で長いビルドのログが押し出されないように)。ジョブは `%LOCALAPPDATA%` 配下(ADR-0009 §1)。
9. クライアント切断時、ジョブは継続する(ログは残る)。通常 exec と detach で挙動を統一する。
10. `kill` / `lost` / `timeout` は監査ログ(ADR-0003)に記録する。

## Consequences

- 長時間ジョブ・切断耐性・二重実行防止が得られる。`awase-build` の連打による並走が防げる。
- サーバにディスク上の状態(ジョブ履歴)が増える。保持は件数のみでシンプルにした。
- `kill` は強力なので、トークン必須(ADR-0011)+ 監査記録を必須とする。
- `queue` がないため、連続実行したい場合は利用者側で待つ。

## Alternatives considered

- **Windows サービス / タスクスケジューラ経由**: 対話デスクトップ(toast、GUI アプリ起動)との相性が悪い。
- **インメモリのみ**: サーバ再起動でログを失う。
- **`concurrency="queue"`**: 個人用途では `reject` / `allow` で足りるため見送り。
