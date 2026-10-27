# Phase 5: タイムアウト・監査ログ(5a)・手動承認 UI(5b)(ADR-0003, 0006)

前提タスク: P3(ストア・ハッシュ・承認レコード)、P4(ランナー・kill)。

**運用モード(README OQ-0 (A))**: 実運用は `--auto-approve`。**5a(T5.1, T5.2, T5.6, T5.7)を先に行い、5b(T5.3〜T5.5: 2 段階 toast・レビュー画面・CLI の仕上げ)は手動承認モードを使う判断をした時点まで後回し**にする。5b は設計上 L 規模(T5.3 単独で L)。

## T5.1 `timeout` フィールドと強制

- **ADR**: 0003 §タイムアウト
- **内容**: `StoredTarget` に `timeout: Option<String>`(`humantime` 互換。`cargo add humantime`)。未設定なら実行時に 30 分(**保存・ハッシュ計算には補完しない**)。`exec --timeout` は短縮のみ(`/exec` リクエストに `timeout`)。超過時は Job を kill、終了コード `124`、状態 `timeout`。`features` に `timeout` を追加(T3.6)。
- **AC**:
  - AC-T5.1.1 (L): `timeout="1s"` のターゲットで `sleep 10` が 1〜2 秒で終了コード 124。ツリーの孫も終了している。
  - AC-T5.1.2 (L): `timeout` 未設定のターゲットの**ハッシュ・正規 JSON が不変**(AC-T3.1.2 のゴールデンが緑のまま)。
  - AC-T5.1.3 (L): `exec --timeout 1h` がターゲットの `timeout="10m"`(または既定 30 分)を延長しようとすると **400**(本文にサーバ側の定義値を併記)。`--timeout 5s` は短縮できる。
  - AC-T5.1.4 (L): Rhai の純ループ・`sleep`・`run` の各状態で、期限が来ると 2 秒以内に停止する(ADR-0009 §4 のキャンセル経路)。
  - AC-T5.1.5 (L): 不正な `timeout` 文字列(`"abc"`, `"-1s"`, `"0s"`)の register が 400。
  - AC-T5.1.6 (X): 新クライアントが `timeout` 付き定義を旧サーバ(`timeout` feature なし)へ送ろうとすると、送信前にエラー(T3.6)。
- **リスク**: 既存ターゲットの暗黙の 30 分上限。→ T8.2 の移行点検(全 registered の実行時間の実測)。

## T5.2 監査ログ

- **ADR**: 0003 §監査ログ
- **内容**: `%APPDATA%\clipwire\audit.jsonl`。イベント: `register` / `approve` / `deny` / `start` / `end` / `kill` / `timeout` / `auto-approve`。フィールド: 時刻、イベント、job_id、ターゲット名、定義ハッシュ、要求元 IP、引数、終了コード、所要時間。トークン・環境変数・出力は記録しない。10 MiB で `audit.1.jsonl` に退避(1 世代)。書き込み失敗は exec を止めず `tracing::error!`、承認系の失敗は警告 toast。`clipwire audit --tail N`(ファイル直接読み)。`GET /audit` は作らない。
- **AC**:
  - AC-T5.2.1 (L): `register` → `approve`(CLI)→ `exec` の一連で、期待するイベントが順序どおり JSONL に 1 行ずつ追記される。各行が有効な JSON。
  - AC-T5.2.2 (L): ログ全体に、テスト用トークン文字列・`env` の値・子の出力が含まれない(`grep` で 0 件)。
  - AC-T5.2.3 (L): 10 MiB 超で `audit.1.jsonl` に退避され、世代は 1 つのみ。
  - AC-T5.2.4 (L): 監査ファイルを書き込み不可にしても、`exec` は成功し、エラーログが出る。承認系(`approve`)は、警告が出る(L では toast をスタブ化して呼び出しを確認)。
  - AC-T5.2.5 (L): ルート一覧に `/audit` が存在しない(回帰)。
  - AC-T5.2.6 (L): 監査の `def_hash` から `approved/<hash>.json` を引いて、承認時の本文を復元できる。
  - AC-T5.2.7 (L): `--auto-approve` での登録が `approve`(承認者 = `auto`)として記録され、承認レコードが作られる(ADR-0011 §B-12。承認レコード側は T3.4.6)。
  - AC-T5.2.8 (L): `--auto-approve` 起動が `serve-start`(`auto_approve=true`)として監査に記録される(T2.2 の警告ログと対)。
  - AC-T5.2.9 (L): 1571 件の連続 register(自動承認)で、監査ログへの書き込みが、CI で計測した値を閾値として固定した時間内(初期値: 60 秒以内)に完了し、各行が有効な JSON。
- **リスク**: 低。

## T5.3 承認 toast の刷新(2 段階・ハッシュ束縛)【5b: 手動承認モード用。後回し】

- **ADR**: 0006 §1〜§5, §7
- **内容**:
  - 1 段階目の toast: ボタンは「内容を見る」「拒否」。`scenario="reminder"`。引数は `review:<full-hash>` / `deny:<full-hash>`。
  - 「内容を見る」: レビューファイルを生成(T5.4)→ `start_detached` 相当でメモ帳を起動 → 2 段階目の toast(「承認」「拒否」、`approve:<full-hash>` / `deny:<full-hash>`)。
  - 承認/拒否の書き込み前に、現行 pending のハッシュと引数のハッシュが一致することを、ストアのロック内で確認。不一致なら何もしない(pending を消さない)。
  - Dismissed の reason が `TimedOut` なら待機を続ける。`UserCanceled` は無視。待機スレッドは**ハッシュごとに 1 本**。pending の保持期限 24h。
  - サーバ起動時に `ToastNotificationManager::History().Clear(AUMID)`、未処理 pending の toast を出し直す。
  - 拒否: pending から削除し、監査に `deny`。
  - 文言の統一(register の応答、`409`)。
- **AC**:
  - AC-T5.3.1 (L): ハッシュ不一致の承認/拒否要求(テスト用の内部関数)が、何も変更しない。
  - AC-T5.3.2 (L): 同じハッシュの pending に対して、待機スレッドが 2 本にならない(再 register を 10 回繰り返して、待機管理のテーブルの件数が 1)。
  - AC-T5.3.3 (L): 拒否で pending が消え、監査に `deny` が記録される。
  - AC-T5.3.4 (L): `409` と `register` の応答の文言が、ADR-0006 §7 の文言と一致する。
  - AC-T5.3.5 (W): 新規 register で 1 段階目の toast が表示され、「内容を見る」でメモ帳が開き、続けて 2 段階目の toast が表示される。「承認」で registered に入り、`exec` できる。
  - AC-T5.3.6 (W): 1 段階目の toast を放置(数秒以上)しても、アクションセンターに残り、そこから押して動作する(PoC 項目: ADR-0006 の Consequences(1))。動作しない場合は ADR-0006 を更新し、`clipwire approve` 導線を主とする。
  - AC-T5.3.7 (W): サーバ再起動後、未処理の pending の toast が出し直される。前プロセスの toast のボタンを押しても、意図しないプロセス起動(コンソールが一瞬出る等)がない、または起動しても無害(PoC 項目 (2))。
  - AC-T5.3.8 (W): 内容 A の toast が出ている間に、内容 B を register して、A の toast の「承認」を押しても、B は承認されない(A の承認は pending のハッシュ不一致で拒否される。B の pending が残る)。
  - AC-T5.3.9 (W): ×で閉じた toast は pending が残り、拒否扱いにならない。
- **リスク**: toast の挙動が環境依存(AC-T5.3.6/7 が NG の場合の設計変更)。→ その場合は CLI(`approve --hash`)を正規の導線とし、toast は通知のみにする(ADR-0006 を更新して Opus 再レビュー)。

## T5.4 レビューファイルの生成【5b: 手動承認モード用。後回し】

- **ADR**: 0006 §2
- **内容**: `render_review(&StoredTarget, previous: Option<&StoredTarget>, hash) -> String`。冒頭にハッシュ全体・dir・env・timeout・concurrency・params と警告サマリ、全文(行番号付き、120 桁折り返し)、再承認時は unified diff(`cargo add similar`)。エスケープ対象は ADR-0006 §2 の「見えない文字・紛らわしい文字」のみ。日本語はそのまま。非 ASCII 行にマーカー、混在文字体系の警告。`%LOCALAPPDATA%\clipwire\review\<hash>.txt` に読み取り専用で書き出し、承認・拒否・掃除時に削除。
- **AC**:
  - AC-T5.4.1 (L): **書式**: 各論理行は `NNNN| ` の行頭、折り返しの継続行は `     ↳ `(行番号の桁数ぶんの空白 + `↳ `)で始まる。300 桁の行を含む script で、継続記号と行頭を除いて連結すると元の文字列に一致する。
  - AC-T5.4.8 (L): `-EncodedCommand` / `-enc` の引数(UTF-16LE の base64)を含む定義で、レビューファイルにデコード結果が併記される(156 件が base64 で、そのままでは読めないため)。デコードできない場合は警告のみ。
  - AC-T5.4.9 (L): 実データ(`CLIPWIRE_TARGETS_FIXTURE`)の最長 script(約 34,000 文字)のレビューファイルが生成でき、全文が欠けない。
  - AC-T5.4.2 (L): U+202E、U+200B、U+FEFF、U+3164 を含む文字列が `\u{XXXX}` 表記になる(通常は T3.2 で register 時に拒否されるが、二重防御として確認)。
  - AC-T5.4.3 (L): 日本語のコメント・文字列が**エスケープされずに**そのまま出力される。
  - AC-T5.4.4 (L): 再承認時に、unified diff が出力され、かつ全文も出力される(両方)。
  - AC-T5.4.5 (L): 警告サマリに「非 ASCII 文字 n 個 / 最長行 m 桁」が出る。ラテン文字とキリル文字が混在する識別子があると、警告が出る。
  - AC-T5.4.6 (L): 承認・拒否の後にレビューファイルが削除される。
  - AC-T5.4.7 (W): メモ帳の起動が、標準ハンドルを継承せず、Job の外(サーバ停止でメモ帳が閉じない)。
- **リスク**: 低〜中(表示の仕様が細かい)。

## T5.5 CLI の仕上げ(`show` の diff、`pending`、メッセージ)【5b: 手動承認モード用。後回し】

- **ADR**: 0006 §6
- **内容**: T3.8 の `show` に diff を追加(T5.4 の関数を共用)。`deny` の表示確認。`approve` の `y/N`(`--yes` で省略)。
- **AC**:
  - AC-T5.5.1 (L): `show <name>` の出力が、レビューファイルと同じ内容(ハッシュ・全文・diff)。
  - AC-T5.5.2 (L): `approve` の確認で `n` を入力すると何も変更されない。`--yes` で確認を省略できる。
  - AC-T5.5.3 (L): `pending` が、名前・ハッシュ先頭・登録時刻を一覧表示する。
- **リスク**: 低。

## T5.7 `approved/` の掃除(自動承認での肥大化対策)【5a】

- **ADR**: 0010 §3
- **内容**: 自動承認では register のたびに `approved/<hash>.json` が増える(使い捨て 1571 件規模)。**`approved/*.json` のファイルの mtime(最後に登録・承認された時刻。register のたびに既存ファイルの mtime を更新する)**が保持期間(既定 90 日)を過ぎ、かつ registered に参照されないものを、起動時と定期的に削除する。`registered` が参照するものは決して消さない。判定は監査ログの保持量(T5.2 は 10 MiB・1 世代)と切り離す(監査側に 90 日分は残らないため)。
- **AC**:
  - AC-T5.7.1 (L): registered に参照されるファイルは、古くても削除されない。
  - AC-T5.7.2 (L): registered から外れ、mtime が保持期間(90 日)を過ぎたファイルが削除される。期間内のファイルは残る。同一内容の再 register でファイルの mtime が更新され、削除対象から外れる。
  - AC-T5.7.3 (L): 削除中に exec が並行しても、exec が参照するファイルを消さない(ストアのロック内で判定)。
- **リスク**: 低。

## T5.6 承認経路の回帰(セキュリティ)【5a】

- **ADR**: 0010 §4, 0011、README の脅威モデル
- **AC**:
  - AC-T5.6.1 (L): **`--auto-approve` なしで起動したサーバ**に対し、トークンを持つクライアントが HTTP だけを使って `register` → 承認済みにする方法が存在しない(全ルートを列挙して、registered を変更できるルートが、`--auto-approve` 起動時の `register` のみであることを検証するテスト)。`--auto-approve` 起動では `register` が承認を兼ねることは仕様(= 自動承認モードでは、トークン = コード実行権限。README の脅威モデル)。
  - AC-T5.6.2 (L): `--auto-approve` なしで、`register` の応答・副作用として registered が変更されない。
  - AC-T5.6.3 (X): **本番の自動承認サーバとは別に**、別ポート・別設定ディレクトリの**検証用インスタンス**(`--auto-approve` なし。T0.3 の設定ディレクトリ差し替えと、設定ディレクトリ由来のミューテックス名を使う)を起動し、Linux クライアントのトークンだけで承認を完了させようとする手順(`register` 後に `exec`)が 409(承認待ち)で失敗する。本番サーバを一時的に切り替えない(開発ループが止まるため)。
- **リスク**: 低。
