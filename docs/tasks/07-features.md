# Phase 7: 機能追加(ADR-0005, 0007, 0004, 0008)

各タスクは独立に出荷できる。依存は各タスクに明記。

## T7.1 `exec --copy` / `--copy-on-fail`(ADR-0008)【P0 直後から着手可】

- **ADR**: 0008 §1〜§5
- **内容**: クライアント側のみ。受信した出力(バッファ/ストリームのどちらでも)を保持し、終了後に既存の `POST /clip` で送る。上限 1 MiB(超過は末尾優先、先頭に `[truncated]`)。ANSI エスケープは除去(`--raw` で維持)。`--copy-on-fail` は終了コード非 0 のときのみ。
- **AC**:
  - AC-T7.1.1 (L): 出力が `POST /clip` に送られる(モックサーバで本文を検証)。
  - AC-T7.1.2 (L): 2 MiB の出力で、送られる本文が 1 MiB 以下で、`[truncated]` で始まり、末尾側が保持されている。
  - AC-T7.1.3 (L): ANSI エスケープ(`\x1b[31m...`)が除去され、`--raw` では維持される。
  - AC-T7.1.4 (L): `--copy-on-fail` で、成功時は `POST /clip` が呼ばれず、失敗時は呼ばれる。
  - AC-T7.1.5 (L): `POST /clip` が失敗しても(サーバ応答 500)、`exec` の終了コードは**ジョブの終了コード**のままで、警告のみ stderr に出る。
  - AC-T7.1.6 (X): 実機で、`exec awase-build --copy` の結果が Windows のクリップボードに入る。
- **リスク**: 低。ビルドログにトークン等が含まれうる点は ADR-0008 §5 の注意事項をヘルプに記載。

## T7.2 `list`(ADR-0005)【P3 の T3.7 後】

- **ADR**: 0005 §list
- **内容**: `clipwire list [--json]`。ローカル `targets.toml` の各定義を `POST /targets/check`(T3.7)に投げ、状態を表示(`ok/changed/pending/unregistered/remote-only`)。
- **AC**:
  - AC-T7.2.1 (L): 5 状態すべてが表示される(モックサーバの固定応答でテーブル駆動)。
  - AC-T7.2.2 (L): `--json` の出力が、安定したスキーマ(`name`, `state`, `hash`)で、ゴールデンファイルと一致。
  - AC-T7.2.3 (L): サーバが旧バージョン(`/targets/check` が 404)の場合、「サーバが未対応」と表示し、非 0 終了。
  - AC-T7.2.4 (X): 実機で、ローカルの定義を 1 文字変更すると `changed` になり、`register` + 承認後に `ok` になる。
  - AC-T7.2.5 (X): 現在の全ターゲットが、T3.4 の移行後に `ok`(ハッシュの決定性の実機確認。**ここで `changed` が出たら T3.1 の互換性バグ**)。
- **リスク**: 低。

## T7.3 `status`(ADR-0005)【T6.1 の後】

- **内容**: `list` に加え、実行中ジョブ(`GET /jobs?state=running`)とサーバの `version`/`proto`/`features` を表示。
- **AC**:
  - AC-T7.3.1 (L): 実行中ジョブがあれば一覧に出る。なければ「なし」。
  - AC-T7.3.2 (L): サーバの `version`/`proto`/`features` が出る。旧サーバでは `proto=1` と表示。
- **リスク**: 低。

## T7.4 Rhai API: `sleep` / `start_detached` / `notify`(ADR-0007)【P4 の後】

- **ADR**: 0007
- **内容**:
  - `sleep(ms)`: 1 回 60 秒上限、100ms 刻みでキャンセル確認。
  - `start_detached([...])`: 標準ハンドルなし、`CREATE_BREAKAWAY_FROM_JOB | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`、`dir` 基準の相対パス解決、PID を返す。BREAKAWAY 不許可なら明示エラー。非 Windows はスタブ(`setsid`)。
  - `notify(msg)`: WinRT toast(T1.2 の経路)。`msg` は XML エスケープ。
  - `print`/`debug` は T4.5 で対応済み(専用 `log` は作らない)。
  - 採用しないもの(`env`, `retry`, `wait_port`, `read_text`, `http_*`)を、テストで「関数が存在しない」ことを固定(意図せず追加されないため)。
- **AC**:
  - AC-T7.4.1 (L): `sleep(200)` が 200ms 以上、以下 + 余裕で戻る。`sleep(61000)` は上限エラー。
  - AC-T7.4.2 (L): 実行中の `sleep(30000)` が、キャンセルで 200ms 以内に中断する。
  - AC-T7.4.3 (C): `start_detached` で起動した子が、ジョブ終了後も生存し、ジョブの `kill`(`TerminateJobObject`)で死なない。PID が返る(Windows 実装を C で検証。L のスタブ(`setsid`)は論理のみ)。
  - AC-T7.4.4 (L): `env`, `retry`, `wait_port`, `read_text`, `http_get` を呼ぶスクリプトが「関数が見つからない」で失敗する。
  - AC-T7.4.5 (C+W): `start_detached(["notepad.exe"])` が、標準ハンドルを継承せず、Job の外で起動する。サーバ停止・`TerminateJobObject` でメモ帳が閉じない。
  - AC-T7.4.6 (W): `notify("a<b&c")` が、toast として表示され、外部プロセスが起動しない。
  - AC-T7.4.7 (W): 既存の `awase-build` を `start_detached` 版に書き換えて、常駐アプリが起動し、その標準出力の `EPIPE` 問題(ADR-0009 §1)が発生しない。
  - AC-T7.4.8 (W): T4.8.7(自己更新)が動作する。
- **リスク**: 中(Windows のフラグの組み合わせ)。T4.1 の結果に依存。

## T7.5 パラメータ付きターゲット(`choices` のみ)(ADR-0004)【**Deferred**: 運用モード (A) 自動承認では、新しいターゲットを作れば足りるため価値が低い。手動承認モード(B)へ移行する場合に前倒しする。README OQ-0 参照】

- **ADR**: 0004(Status を `Deferred` にする旨を ADR 側にも反映済み)
- **内容**: `[targets.x.params.<name>]` に `choices`・`default`。`StoredTarget` に `params`(`features` に `params`)。`exec --arg k=v`(複数可)、リクエストに `args`。検証: 宣言外の名前・選択肢外の値・必須欠落・制御文字は 400。Rhai: `params` 宣言があるターゲットのみ `args` を注入。Steps: `params` 宣言があるターゲットのみ `${name}` を置換(`StepsDef::Text` は shlex 分割後)。値は `env`/`dir` に入れない。監査に引数の実値。承認のレビューファイルに `params` 宣言を表示(T5.4)。
- **AC**:
  - AC-T7.5.1 (L): `choices=["main","dev"]` に対し、`--arg branch=dev` が通り、`--arg branch=x` / `--arg unknown=1` / 引数なし(default なし)が 400。
  - AC-T7.5.2 (L): 値 `main; calc` / `../x` / `-rf` / 改行を含む値が、`choices` にないため 400。`choices` に入れた値でも、argv の **1 要素**として渡る(`run(["echo", args.branch])` で、値に空白を含む選択肢が分割されない)。
  - AC-T7.5.3 (L): `params` 宣言のないターゲットで、Rhai の `let args = 1;` が壊れない(注入されない)。Steps の `${PATH}` がリテラルのまま。
  - AC-T7.5.4 (L): `params` 宣言の変更(`choices` に値を追加)が、定義ハッシュを変え、再承認が必要になる。
  - AC-T7.5.5 (L): `args` が監査ログに記録される。
  - AC-T7.5.6 (L): `StepsDef::Text` の値置換が、値に空白を含んでも、argv の分割数を変えない。
  - AC-T7.5.7 (X): 旧サーバ(`params` feature なし)へ `--arg` を送ろうとすると、送信前にエラー。
  - AC-T7.5.8 (X): 実機で、`awase-build --arg branch=dev` で指定ブランチがチェックアウトされる。
- **リスク**: 中(承認モデルの境界に触れる。`pattern` は導入しない)。

## T7.6 見送り項目の記録

- **内容**: ADR の「Deferred / 見送り」を GitHub Issue(またはバックログファイル `docs/tasks/backlog.md`)として起票: クリップボード履歴、TTL、Rhai `env`/`retry`/`wait_port`/`read_text`、`queue`、`pattern`、Tailscale IP 変化時の bind のやり直し。
- **AC**:
  - AC-T7.6.1: `docs/tasks/backlog.md` が、各項目に「見送りの理由・再開条件・関連 ADR」を持つ。項目: クリップボード履歴、TTL、Rhai `env`/`retry`/`wait_port`/`read_text`、`queue`、`pattern`、params(運用モード (A) のため Deferred)、手動承認 UI(Phase 5b)、Tailscale IP 変化時の bind のやり直し、使い捨てターゲットの棚卸し・削除(T8.2)。

## T7.7 `/open` の POST 化

- **ADR**: 0011 §B-11
- **内容**: 状態を変える `GET /open` を `POST /open` へ移行する。互換期間、旧 GET の廃止手順、クライアントの先行・後方互換性を設計してから実装する。
- **AC**:
  - AC-T7.7.1 (L): `clipwire open` が `POST /open` を使用し、サーバが要求を処理する。
  - AC-T7.7.2 (X): 互換期間中の旧・新クライアントと旧・新サーバの組み合わせを検証し、結果を evidence に記録する。
- **リスク**: 旧クライアントとの互換性。GET の削除は Phase 8 の互換性検証とリリースノートを前提にする。
