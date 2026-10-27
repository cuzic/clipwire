# Phase 6: ストリーミングとジョブ管理(ADR-0001, 0002)

前提タスク: P4(ランナー・ログ)、P5(timeout・監査)。ADR-0001 と 0002 は「ログの follow」を共有するため、**実装は統合して進める**(ADR の README の実装順 5)。

## T6.1 ジョブのメタとレジストリ

- **ADR**: 0002 §2, §5, §7, §8, §10
- **内容**: `jobs\<id>\meta.json`(`target`, `def_hash`, 開始/終了、状態 `running|orphaned|succeeded|failed|timeout|killed|lost`、終了コード、要求元、**実行中の子の PID と作成時刻**)。ジョブ ID は ULID(`cargo add ulid`)。`JobRegistry`(メモリ上の実行中ジョブ + ディスクの履歴)。
  - **排他**: `concurrency = "reject"|"allow"`(既定 `reject`。`StoredTarget` に追加。`features` に `concurrency`)。`reject` で同名が `running` または `orphaned` なら 409(実行中のジョブ ID を返す)。`orphaned` は判定のたびに生存確認し、死んでいれば `lost` にする。
  - **起動時の整合**: `running` のまま残ったメタを `orphaned`(生存)/`lost`(死亡)に更新。
  - **保持**: 直近 50 件。`running`/`orphaned` は削除対象外。失敗と detach は成功の通常 exec より後に消す。
  - 通常の `exec` も内部的にジョブとして記録。
- **AC**:
  - AC-T6.1.1 (L): 同名ターゲットを 2 つ同時に `exec` すると、2 つ目が 409 で、本文に実行中のジョブ ID が含まれる。`concurrency="allow"` なら両方走る。
  - AC-T6.1.2 (L): 異なるターゲットは互いに排他されない。
  - AC-T6.1.3 (L+C): 起動時整合(生存確認の Windows 実装 = PID + プロセス作成時刻は C で検証): `running` のメタと、生存する子(テスト用の `sleep`)→ `orphaned`、死亡 → `lost`。`orphaned` は `reject` を起こし、子が死ぬと(次の判定で)`lost` になり、新規実行が通る。
  - AC-T6.1.4 (L): Rhai ターゲット(`run` で子を起動中にサーバを模擬的に「再起動」= レジストリを破棄して再構築)で、子が生きていれば `orphaned`(main の PID = サーバ自身、ではなく**実行中の子の PID** を記録していることの検証。N-M3)。
  - AC-T6.1.5 (L): 51 件目のジョブで古いものが削除されるが、`running` は削除されず、失敗ジョブは成功ジョブより長く残る。
  - AC-T6.1.6 (C): 保持処理が、ファイルロック(別プロセスが開いたまま)で削除に失敗しても、次回に再試行され、エラーでサーバが止まらない(Windows 固有。L では検証にならない)。
  - AC-T6.1.7 (X): 新クライアントが `concurrency` 付き定義を旧サーバへ送ると、送信前にエラー(T3.6)。
- **リスク**: 状態遷移のバグ(レース)。→ 状態遷移を 1 つのモジュールに集約し、全遷移をテーブル駆動テストする。

## T6.2 ジョブ API(`/jobs*`)と CLI

- **ADR**: 0002 §3, §4, §9
- **内容**: 保護ルート: `GET /jobs`(`?state=`)、`GET /jobs/:id`、`GET /jobs/:id/log?offset=N[&follow=1]`、`POST /jobs/:id/kill`(JSON 必須)。`POST /exec` の `detach: true`。CLI: `clipwire jobs`, `clipwire logs <id> [-f]`, `clipwire kill <id>`, `clipwire exec --detach`。`features` に `jobs`。kill/lost/timeout は監査へ。
- **AC**:
  - AC-T6.2.1 (L): `exec --detach` がジョブ ID を即座に返し(1 秒以内)、ジョブが完了するまで `GET /jobs/:id` の状態が `running` → `succeeded`。
  - AC-T6.2.2 (L): `logs <id>` が完了したジョブのログ全文を返す。`offset=N` で差分のみ。
  - AC-T6.2.3 (L): `kill <id>` でジョブが `killed` になり、子のツリーが終了する。監査に `kill`。終了済みジョブへの `kill` は 409/404 で副作用なし。
  - AC-T6.2.4 (L): 通常の `exec` のクライアントが切断(コネクションを途中で閉じる)しても、ジョブは継続し、完了後に `logs` で全出力が読める(ADR-0002 §9)。
  - AC-T6.2.5 (L): `/jobs*` がトークンなし・`--allow-no-token` なしで 403、誤トークンで 401(`--allow-no-token` 起動では認証なしで通る)、`Origin` 付きで 403、`POST /jobs/:id/kill` が `text/plain` で 415。
  - AC-T6.2.6 (L): 存在しないジョブ ID が 404。
- **リスク**: 低〜中。

## T6.3 NDJSON ストリーム(`/exec` と `follow`)

- **ADR**: 0001 §1〜§4
- **内容**: `Accept: application/x-ndjson` の `POST /exec` と `follow=1` を、ログの follow として実装。イベント `out`/`ping`(30 秒)/`exit`/`err`。UTF-8 の境界で切れた末尾は次回に持ち越し。`exit` は T4.3 のとおりリレー終了後。`features` に `stream`。`Accept` なしは従来形式(`text/plain` + `X-Exit-Code`)。
- **AC**:
  - AC-T6.3.1 (L): 5 秒かかるジョブで、`out` イベントが**ジョブ完了前に**クライアントに届く(最初のイベントが 2 秒以内)。
  - AC-T6.3.2 (L): ログに 3 バイトの多バイト文字(`あ`)を 1 バイトずつ追記しても、`out` の `d` に U+FFFD が含まれず、完全な文字が届く。
  - AC-T6.3.3 (L): 出力のないジョブで、30 秒ごとに `ping` が届く(テストでは間隔を短縮できる設定にする)。
  - AC-T6.3.4 (L): `exit` が常に最後の 1 イベントで、その前のすべての `out` を受け取っている(100 回繰り返し)。
  - AC-T6.3.5 (L): `Accept` なしの `POST /exec` が、従来どおり `text/plain` + `X-Exit-Code` で、終了後に全出力を返す(旧クライアント互換)。
  - AC-T6.3.6 (L): サーバ側エラー(未登録名など)が `err` イベント or 従来どおりのステータスコードで返る(未登録は従来どおり 404/409 を維持)。
- **リスク**: 中(ストリームの終了条件)。

## T6.4 クライアント(`cmd_exec`)の刷新

- **ADR**: 0001 §5
- **内容**: 全体の `timeout(600s)` を廃止し、`timeout_connect(10s)` + ストリームは `timeout_read(120s)`、バッファ形式(`--no-stream`・旧サーバへのフォールバック)は `timeout_read(31 分)`。`Content-Type` でストリーム/バッファを判定。`into_reader()` で読む。`exit` なしの切断は非 0 で終了し、ジョブ ID を案内(`clipwire logs <id>`)。`exec --timeout <dur>`(T5.1)。
- **AC**:
  - AC-T6.4.1 (L): 10 分超のジョブ(テストでは時間を縮めた設定)が、クライアントのタイムアウトで切れない。
  - AC-T6.4.2 (L): `--no-stream` で、120 秒を超えて無出力のジョブ(縮小した設定で再現)が切断されず、終了後に全出力を受け取る。
  - AC-T6.4.3 (L): 旧サーバ(`features` に `stream` がない、または `text/plain` 応答)に対して、自動でバッファ形式にフォールバックする。
  - AC-T6.4.4 (L): サーバが途中で落ちた場合(`exit` なしで切断)、クライアントが非 0 で終了し、ジョブ ID を stderr に出す。
  - AC-T6.4.5 (L): 10 MiB を超える出力(バッファ形式)を、`into_string()` の上限なしで受け取れる。
  - AC-T6.4.6 (X): 実機で、`awase-build`(実際のビルド、数分)を、ストリームで進捗を見ながら完了できる。`--no-stream` でも完了できる。
- **リスク**: 低。

## T6.5 フェーズ 6 の出荷と実機検証

- **AC**:
  - AC-T6.5.1 (X): `awase-build-bg` / `awase-build-log` 相当の手作業の回避策を使わずに、`exec awase-build` の進捗が見え、完了後の再実行が 409 にならない(常駐アプリ起動の挙動。ADR-0009 §3)。**かつ、awase.exe が標準ハンドルを継承していない**(T4.1b で `Start-Process` 化済み。AC-T4.1b.2 と同じ確認方法。リレーが閉じた後の `EPIPE` で常駐アプリが panic しない。「5 分生存」だけでは無操作の常駐アプリが書かないため検証にならない)。
  - AC-T6.5.2 (X): `awase-build` の連打(2 回目が 409)で、`taskkill` → `git pull` が並走しない。
  - AC-T6.5.3 (X): サーバを `--replace` で入れ替えた後、旧サーバ時代のジョブが `lost`/`orphaned` として `jobs` に表示される。
