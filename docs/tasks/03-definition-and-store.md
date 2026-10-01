# Phase 3: 定義の正規形・ストア・承認レコード(ADR-0010)

前提タスク: P0, P2。**`BTreeMap` 化(T3.1)は、ハッシュを保存・表示する前に入れる**(ADR-0010 §1)。本フェーズで「承認の経路(ローカルのファイル直接操作)」と「プロトコル版」の土台が確定する。

## T3.1 `StoredTarget` の正規化(BTreeMap・未設定非シリアライズ)

- **ADR**: 0010 §1
- **内容**: `env: HashMap` → `BTreeMap`。`CanonicalTarget`(`v:1`, `dir`, `script|steps`, `env`)と `canonical_json(&StoredTarget) -> Vec<u8>`、`definition_hash(&[u8]) -> String`(`sha256:` + hex。`cargo add sha2 hex`)を実装。未設定のフィールドは出力しない。将来フィールド(`timeout` 等)を追加しても、未設定ならバイト列が変わらないことをテストで固定。
- **AC**:
  - AC-T3.1.1 (L): 同じ内容で env のキー挿入順を変えた 100 通りの構築で、`canonical_json` のバイト列が常に同一。
  - AC-T3.1.2 (L): ゴールデンテスト: 固定の定義(script 形式、steps 形式、env あり/なし)に対し、期待するバイト列と sha256 が固定値(`tests/fixtures/canonical/*.json`)と一致。**この固定値は以降のフェーズで変えてはならない**(変えると既存承認が失効するため、テストの失敗が「互換性破壊」の警報になる)。
  - AC-T3.1.3 (L): `StepsDef::Text` と `Argv` は、正規形上で別表現として扱う(相互変換しない)。同じコマンドでも表現が違えばハッシュが違うことを明示するテスト。
  - AC-T3.1.4 (L): 新フィールド(テスト用に `timeout: Option<..>` を仮追加)が `None` のとき、AC-T3.1.2 のバイト列が不変。
- **リスク**: 既存の registered.toml の再シリアライズ。→ 読み込んだ構造体から `canonical_json` を作る経路のみを使う(TOML の再書き込みでハッシュを計算しない)。

## T3.2 定義の検証(制御文字・双方向文字)

- **ADR**: 0010 §1(検証)
- **内容**: `validate_definition(&StoredTarget) -> Result<(), DefinitionError>`。`script`/`steps`/`env`(キーと値)/`dir` のすべての文字列を走査し、Cc、Cf、Zl、Zp(TAB・LF・CRLF の CR を除く)、Hangul Filler(U+115F, U+1160, U+3164)を拒否。エラーは位置(行・桁)とコードポイントを含む。`handle_register` で 400。
- **AC**:
  - AC-T3.2.1 (L): 拒否されるもの: U+202E、U+2066、U+200B、U+200D、U+FEFF、U+00AD、U+2028、U+2029、U+3164、NUL、単独の CR(`\r` が `\n` を伴わない)。
  - AC-T3.2.2 (L): 通るもの: 日本語(ひらがな、漢字、全角スペース U+3000)、TAB、LF、CRLF、絵文字単体(U+1F600)、`env` の通常値。
  - AC-T3.2.3 (L): 400 の本文に「行 N 桁 M: U+XXXX」が含まれる。
  - AC-T3.2.4 (L): 環境変数 `CLIPWIRE_TARGETS_FIXTURE` で指定した実データ(`~/.config/clipwire/targets.toml`、1571 件・最長 script 約 34,000 文字。未設定ならスキップ)の**全ターゲットが、パース・名前検証(T1.1)・定義検証を通る**。通らない場合は検証を見直すか定義を修正。CI は合成データ(長い script、`-EncodedCommand` 風の長い ASCII 行を含む)で同じ経路を検証する。
- **リスク**: 既存 registered の内容が検証に違反する場合、`exec` を止めるか? → 検証は **register 時のみ**。既存の registered は `exec` を止めず、起動時に警告のみ。

## T3.3 原子的ストア・直列化・破損時の書き込み拒否

- **ADR**: 0010 §4
- **内容**: `Store` 型(`pending.toml` / `registered.toml` / `approved/` の管理)を `src/store.rs` に実装。
  - 書き込み: 一時ファイル + `rename`(Windows では `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`。`std::fs::rename` は Windows でも既存ファイルを置換するが、他プロセスが開いていると失敗するためリトライ付き)。
  - サーバ内の直列化: `tokio::sync::Mutex`。
  - 名前付きミューテックス `Global\clipwire_store`(`#[cfg(windows)]`。非 Windows はファイルロック `flock` のスタブ)。取得タイムアウト付き、`WAIT_ABANDONED` は parse 検証後に続行。**取得〜解放を 1 つの `spawn_blocking`(await なし)内で完結**。
  - parse 失敗時: `*.corrupt.<時刻>` へ退避、書き込みは拒否(エラー)。`.bak` 1 世代。
- **AC**:
  - AC-T3.3.1 (L+C): 書き込み中の強制終了をシミュレート(一時ファイル書き込み後、rename 前に中断)しても、本体ファイルが壊れず旧内容のまま。
  - AC-T3.3.2 (L): 本体ファイルを壊れた TOML にすると、`register` が 500 でエラーになり、**ファイルが空マップで上書きされない**。`*.corrupt.*` が作られる。既存の承認が消えない。
  - AC-T3.3.3 (L+C): 同時に 50 個の `register`(別名)を並列実行しても、全件が pending に残る(更新の取りこぼしなし)。**Windows 実装(名前付きミューテックス・`MoveFileExW`)は C で検証する**(L の flock スタブでは Windows の排他を検証できない)。
  - AC-T3.3.4 (L): 書き込み成功のたびに `.bak` が直前の正常版になる。
  - AC-T3.3.5 (C): サーバと CLI(別プロセス)が、同時に 100 回ずつ書き込んでも、ファイルが壊れず、最終内容が両者の書き込みの和集合になる。
  - AC-T3.3.6 (W): CLI が確認待ち(入力待ち)の間、サーバの `register` が**ブロックされない**(ロックを持たない。T3.8 と合わせて確認)。
  - AC-T3.3.7 (W): サーバと CLI を異なる整合性レベルで起動してミューテックスを開けない場合、ロックなしで続行せずエラーになる。
  - AC-T3.3.8 (W): 別ログオンセッション(`ssh` で入った Windows)から CLI を実行しても、排他が効く(`Global\`)。
- **リスク**: ミューテックスやイベントは一般ユーザーでも `Global\` に作れる(`SeCreateGlobalPrivilege` が必要なのはセッション 0 以外からのファイルマッピングの場合)。現行の `Global\clipwire_singleton` が既に動いていることが根拠。念のため C/W で確認し、失敗時は ADR-0010 の代替(`Local\` + 対話セッション限定)へ更新する。
- **注意(自動承認運用)**: 自動承認では register のたびに `approved/` が増える。掃除は T5.7。

## T3.4 承認レコード(`approved/<sha256>.json`)と移行

- **ADR**: 0010 §3
- **内容**: 承認時に正規 JSON のバイト列を `approved/<hash>.json` に保存(`O_EXCL`、既存なら内容一致を確認)。**`registered.toml` のエントリは、本文(dir/script/steps/env)を残したまま `hash` と `approved_at` を「追加」する(追加だけの移行)**。旧サーバ(P3 より前の good.exe 含む)は余分なフィールドを無視して従来どおり動くため、ウォッチドッグによる旧版への自動フォールバックが、移行済みのストアを壊さない。`exec` 時: `approved/<hash>.json` を読み、sha256 を再計算して一致しなければ実行拒否(500、理由つき)。さらに、エントリの本文が同じ `hash` に正規化されることを検証する(不一致は拒否)。本文の削除は、旧版へ戻す可能性がなくなった後(P8)に行う。
  - **移行**: サーバ起動時に一度だけ、ストアのロックを取り、旧形式(本文を持つ)エントリを `approved/` に書き出して新形式にする。読み込み経路では書き込まない。移行前に `registered.toml.pre-migrate.bak` を作る。
- **AC**:
  - AC-T3.4.1 (L): 旧形式の `registered.toml`(現在の実ファイルのコピー)を与えて起動すると、全エントリが新形式に移行し、各エントリの `exec` が移行前と同じ出力を返す(実コマンドはテスト用の安全なもの)。
  - AC-T3.4.2 (L): 移行は冪等(2 回起動しても結果が同じ、`approved/` のファイル数が増えない)。
  - AC-T3.4.3 (L): `approved/<hash>.json` の 1 バイトを書き換えると、`exec` が拒否される。ファイルを削除しても拒否される(500)。
  - AC-T3.4.4 (L): 移行前バックアップが作られる。移行中に中断しても(再起動で)再開でき、旧ファイルが失われない。
  - AC-T3.4.9 (L/C): **P3 移行済みのストアで、P2 版サーバ(T8.1 の旧バイナリ)を起動し、自動承認の `register` と `exec` を行っても、移行済みエントリが失われない**(旧サーバは本文を保持して書き戻し、`hash`/`approved_at` が消えても、次の P3 サーバ起動の移行で復元される。全エントリの `exec` が成功する)。
  - AC-T3.4.10 (L): `approved/<hash>.json` とエントリ本文が不一致(本文を 1 文字変更)のとき、`exec` が拒否される。
  - AC-T3.4.5 (X): 実機の現行 `registered.toml` で移行を実施し、**代表セット**(`awase-build`, `awase-reboot`, `adb-status-check`, `clipwire-rebuild-restart`)が `exec` できる。全件については、移行後に全エントリの `approved/<hash>.json` が存在し、sha256 が再計算と一致することを機械的に確認する(`evidence/T3.4.md`)。
  - AC-T3.4.6 (L): **`--auto-approve` で `register` した定義についても、`approved/<hash>.json` が作られ、`registered.toml` のエントリが `{hash, approved_at}` になり、直後の `exec` が成功する**(実運用は自動承認。漏れると全ターゲットの `exec` が 500 になる。ADR-0011 §12)。承認者は `auto` として扱う(監査は T5.2)。
  - AC-T3.4.7 (L): 自動承認で同一内容を再 register しても、`approved/` のファイルが増えず(内容アドレスで冪等)、`exec` が成功し続ける。
  - AC-T3.4.8 (L): 実データ(`CLIPWIRE_TARGETS_FIXTURE` の 1571 件)を自動承認で連続 register しても、**CI で計測した値を閾値として固定**(初期値: 60 秒以内。計測結果を `evidence/` に記録して調整)して完了し、`approved/` の件数 = ユニークなハッシュ数。
- **リスク**: 移行の失敗で全ターゲットが使えなくなる。→ pre-migrate バックアップからの復旧手順を `dev-loop.md` に書く。

## T3.5 同一ハッシュ再登録の no-op と pending の掃除

- **ADR**: 0010 §5
- **内容**: `register` で、定義のハッシュが registered と同一なら no-op(`200`、メッセージ「変更なし」)。別ハッシュの pending があれば削除。pending に同一ハッシュがあれば no-op(toast の再表示は T5.x が扱う。本タスクでは「再表示の要否」を返すフラグのみ用意)。
- **AC**:
  - AC-T3.5.1 (L): 同一内容の `register` を 2 回行っても、2 回目は pending を作らず、registered が残る(再承認が要求されない)。
  - AC-T3.5.2 (L): 内容 A を承認済みで、B を register して pending にした後、A を register し直すと、B の pending が消える。
  - AC-T3.5.3 (L): 内容が 1 文字でも違えば、従来どおり pending に入り再承認が必要(registered から外れる)。
  - AC-T3.5.4 (L): 応答に、定義のハッシュが含まれる(ADR-0010 §1)。
- **リスク**: 低。

## T3.6 プロトコル版・入れ子リクエスト・旧フラット形式

- **ADR**: 0010 §2
- **内容**:
  - `GET /health`: `Accept: application/json` のときのみ `{"version","proto":2,"features":[...]}`。それ以外は従来どおり `OK\n`。`features` は実装済みの機能名のみ(初期は空配列か `["hash"]`)。
  - `register` のリクエスト: まず入れ子 `{"name","target":{...}}`(`deny_unknown_fields`)で parse、失敗したら旧フラット専用 struct(`flatten` なし・`deny_unknown_fields`)で parse。どちらも失敗したら 400(エラー位置を含む)。
  - クライアント: `register`/`exec` の前に `GET /health`(JSON)を行い、応答がない(旧サーバ)場合は `proto=1`・機能なしとみなす。**`proto < 2` のサーバには旧フラット形式で `register` する**(旧サーバは `#[serde(flatten)]` で未知フィールドを許容するため、入れ子形式を送ると `target` が無視され、script も steps もない空のエントリが黙って登録され、`exec` が「script も steps もありません」で 500 になる)。**未対応のフィールドを含む定義は、送信前にエラー**(この時点では送るべき新フィールドがないため、仕組みとテスト用フィールドのみ)。
- **AC**:
  - AC-T3.6.1 (L): `Accept` なしの `/health` が `OK\n` のまま(既存のヘルスチェック利用者を壊さない)。
  - AC-T3.6.2 (L): 入れ子形式・旧フラット形式の両方の `register` が成功し、同じ定義が同じハッシュになる。
  - AC-T3.6.3 (L): 未知フィールドを含む旧フラット形式・入れ子形式が 400 になる(サイレントに捨てない)。
  - AC-T3.6.4 (L): `proto` と `features` が JSON に含まれる。テスト用に feature を追加/削除して、クライアントが「未対応フィールドを含む定義」の送信を拒否する。
  - AC-T3.6.5 (L): **旧サーバのパーサの再現**: 現行 HEAD の `Req`(`#[serde(flatten)] StoredTarget`)を `tests/compat/` にコピーし、(i) 入れ子形式を送ると空のターゲットになること(= 送ってはいけない)、(ii) 旧フラット形式を送ると正しく入ること、を確認する。新クライアントが `proto=1` のサーバへ送るのは (ii) のみであること。
  - AC-T3.6.7 (X): 新クライアント ⇔ 旧サーバ(P2 版)で、`get`/`put`/`exec`/`register`(新フィールドなしの定義)が動く。旧サーバの `/health` は JSON を返さないが、クライアントが `proto=1` として処理する。
  - AC-T3.6.6 (X): 旧クライアント ⇔ 新サーバで `register`(旧フラット形式)が動く。
- **リスク**: 旧サーバが `Accept: application/json` の `/health` に `OK\n` を返す場合の判定。→ `Content-Type` と JSON parse 失敗で `proto=1` と判定するテスト。

## T3.7 サーバ計算ハッシュの API(`POST /targets/check`)

- **ADR**: 0010 §1、0005(`list` の基盤)
- **内容**: 保護ルート。リクエスト: `{"targets":{"<name>": <定義>, ...}}`。レスポンス: 名前ごとの状態(`ok|changed|pending|unregistered`)とハッシュ。本文は返さない。`remote-only` は、リクエストにない registered 名を返す。
- **AC**:
  - AC-T3.7.1 (L): 承認済みと同一定義 → `ok`、1 文字違い → `changed`、pending 中 → `pending`(同一/不一致を併記)、未登録 → `unregistered`、registered のみ存在 → `remote-only`。
  - AC-T3.7.2 (L): レスポンスに script/steps/env/dir の本文が含まれない。
  - AC-T3.7.3 (L): トークンなし(`--allow-no-token` なし)は 403、誤トークンは 401。`--allow-no-token` 起動では認証なしで通る。
  - AC-T3.7.4 (L): **2000 件・合計 3 MB 以上**の check が成功する(実データは 1571 件・約 1.4 MB で JSON 化すると増え、axum の既定ボディ上限 2 MB に近い)。この経路には明示的に 16 MiB の上限を設定し、超過は 413。
- **リスク**: 低。ボディ上限は `/targets/check` のみ引き上げる(他のルートは既定のまま)。

## T3.8 承認・拒否のローカル CLI(HTTP 非経由)【手動承認モード用の最小限。自動承認運用では使われない】

- **ADR**: 0010 §4、0006 §6(ハッシュ束縛は T5 で完成)
- **内容**: 既存 `cmd_approve`(ファイル直接操作)を `Store` 経由に置き換え、次を実装: `clipwire pending`、`clipwire show <name>`(全文。diff は T5)、`clipwire approve <name> --hash <prefix>`(`--hash` 必須、`y/N` 確認はロックの外、書き込み直前にロック内でハッシュ再確認)、`clipwire deny <name> [--hash <prefix>]`。`--dir` は廃止。
  - **サーバの HTTP に承認エンドポイントが存在しないこと**を維持する。
- **AC**:
  - AC-T3.8.1 (L): `approve <name> --hash <正しい prefix>` で registered に移る。prefix が現行 pending のハッシュと不一致なら拒否され、pending は変わらない。
  - AC-T3.8.2 (L): `--hash` なしの `approve` は使用法エラー。`approve --dir` は「オプション廃止」のエラー。
  - AC-T3.8.3 (L): 確認プロンプト中に別ハッシュで再 register された場合、書き込み直前の再確認で拒否される(TOCTOU)。
  - AC-T3.8.4 (L): サーバのルート一覧(T2.1 の列挙)に `approve`/`deny` に相当するルートが存在しない(回帰テスト)。
  - AC-T3.8.5 (L): `deny <name>` は pending のみを削除し、registered に触れない。`--hash` 指定で不一致なら何もしない。
  - AC-T3.8.6 (W): サーバ稼働中に CLI で `approve` でき、サーバの次の `exec` が新しい承認を反映する(サーバの再起動不要)。サーバが承認済み内容をキャッシュしないことの確認。
- **リスク**: サーバが `registered.toml` をメモリキャッシュしている場合に反映されない。→ 現行は毎回ファイルから読む(確認済みのコード: `handle_exec`)。キャッシュを導入しないことを AC-T3.8.6 で固定。
