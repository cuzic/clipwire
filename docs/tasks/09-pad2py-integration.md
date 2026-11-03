# Phase 9: pad2py 連携機能(ADR-0012, 0013, 0014)

背景: 別プロジェクト(pad2py。GCP Windows VM 上の対話的 RDP セッションで pywinauto を動かすのに clipwire を唯一の経路として使っている)からの要望 3 点に対応する。`docs/adr/0012-raw-file-write-and-script-file-exec.md`・`0013-persistent-exec-context.md`・`0014-raw-file-write.md` を参照。

依存: Phase 0〜7(現行の実行ランナー・承認モデル・HTTP 面)が前提。T9.1 と T9.2 は独立に出荷できる。T9.3(PoC)は T9.1 の `script_file` に依存。

## T9.0 ウォッチドッグのフォールバックバイナリ更新(T9.1 の出荷前提)

- **ADR**: 0012 §1, 0013 §1
- **内容**: ADR-0009 §7 の `good.exe`(タスクスケジューラ・ウォッチドッグが自動フォールバックで起動しうる旧バイナリ)を、`script_file`(T9.1)・`persistent`(T9.3 が Accepted になった場合)フィールドを認識するバージョンに更新する。`StoredTarget` は `deny_unknown_fields` を持たないため、これを認識しないバイナリが `registered.toml` を書き戻すと、該当フィールドが黙って失われ、対象ターゲットが次回 `exec` でハッシュ不一致により動かなくなる(直らない。ADR-0010 §4 の `migrate_locked` は `hash`/`approved_at` が欠けている場合しか動かない)。
- **AC**:
  - AC-T9.0.1 (W): 更新後の `good.exe` が、`script_file` を含む `registered.toml` を読み書きしてもフィールドを保持する(読み込み→書き戻し→再読み込みでバイト列が変わらないことを実機で確認し、`docs/tasks/evidence/` に記録)。
- **リスク**: 低。実機でのバイナリ入れ替え作業のみ。

## T9.1 `script_file` 実行(ADR-0012)【T9.0 の後】

- **ADR**: 0012
- **内容**: `StoredTarget` / `CanonicalTarget` / `ExecPayload` に `script_file`(`interpreter: Vec<String>`, `body: String`, `ext: Option<String>`, `env: BTreeMap<String,String>`)を追加。`validate_definition` を適用。register 時のみ `script`/`steps`/`script_file` の排他(2 つ以上は 400)を強制し、既存の承認済み `script`+`steps` 併存エントリは `into_exec` で従来どおり動かす。exec ハンドラが `state.jobs.log_path(&job_id).parent()` に本文を書き、`interpreter + [そのパス]` を argv として既存の `Runner::spawn` に渡す(`Runner` 自体は変更しない)。
- **AC**:
  - AC-T9.1.1 (L): `script_file` 付き `register` のサーバ計算ハッシュが `interpreter`/`body`/`ext`/`env` を含む(ゴールデン fixture で固定。`script`/`steps` だけの既存ハッシュが変わらないことも回帰テストで確認)。
  - AC-T9.1.2 (L): `script`/`steps`/`script_file` のうち 2 つ以上を指定した**新規** `register` が 400。**既存の**承認済み `script`+`steps` 併存エントリは、変更なしに `exec` が従来どおり成功する(回帰)。
  - AC-T9.1.3 (L): `ext` が `^[A-Za-z0-9]{1,16}$` を満たさない場合 400。`body`/`interpreter` への ADR-0010 §1 制御文字検査(Cc/Cf/Zl/Zp/Hangul Filler)が効く。
  - AC-T9.1.4 (L): exec 時、本文が `state.jobs.log_path(&job_id).parent()`(`JobRegistry::prune()` が削除する側のディレクトリ)に書かれ、argv が `interpreter + [絶対パス]` になる。
  - AC-T9.1.5 (L): 完了後のジョブが、既存の `JobRegistry::prune()`(直近 50 件保持)の対象になり、本文ファイルもログと一緒に削除される。
  - AC-T9.1.6 (W): 実機で `script_file: {interpreter: ["python"], body: "print('hello')"}` が実行され、出力が `jobs/<id>/log` に現れる。`ext: "ps1"` での BOM/CP932 文字化けの有無を確認し、結果を evidence に記録する。
- **リスク**: 中。承認モデルへの新バリアント追加は既存の register/exec 経路に手を入れる。既存の 1571 件の `script` ターゲットのハッシュが変わらないことを T9.1.1 の回帰テストで必ず確認する。

## T9.2 `/write`(ADR-0014)

- **ADR**: 0014
- **内容**: 新しい `RouteClass`(`require_auth` 相当 + `Content-Type: application/octet-stream` 必須、JSON 必須なし)、`--enable-write`/`--write-root`(複数可、起動時の denylist・承認済みターゲット重複警告)、二段階パス正規化(字句段階 → OS 呼び出し段階 → コンポーネント単位の許可判定。`std::path::Path` に依存しない純粋関数部分は Linux でテストする)、`MoveFileExW` による置き換えなし作成既定(`?overwrite=1` で明示許可)、`tower_http::limit::RequestBodyLimitLayer` によるボディ上限、監査失敗時の書き込み失敗(fail-closed)、`/health` の `features`。
- **AC**:
  - AC-T9.2.1 (L): パス分類・許可判定の純粋関数(ADR-0014 §6)が、許可ルート配下判定(コンポーネント単位。`pad2py` vs `pad2py-evil` を正しく区別)・`..`/ドライブ相対/ルート相対の拒否・最終コンポーネントの ADS(`:`)/末尾ドット空白/予約デバイス名の拒否・UNC/デバイス名前空間の拒否・denylist との重複方向(祖先・同一・配下)の判定を、Windows パス文字列のテーブル駆動テストで満たす。
  - AC-T9.2.2 (L): `--enable-write` なしでは `/write` が 403(ルート自体はマウントされている。404 ではない)。`--enable-write` ありで `--write-root` 省略は起動時エラー。ドライブルート・`%USERPROFILE%`・clipwire 自身の設定/データディレクトリ・スタートアップフォルダ・PowerShell プロファイルディレクトリ・自プロセス実行ファイルのディレクトリと重なる `--write-root`(祖先・同一・配下のいずれの方向でも)は起動時エラー。
  - AC-T9.2.3 (L): `Content-Type: application/octet-stream` 以外(欠落含む)の POST が 415。
  - AC-T9.2.4 (C): Windows 実ファイルシステムで、既定(置き換えなし)の `/write` が既存ファイルに対し 409 を返し、ファイル内容を変更しない。`?overwrite=1` では置き換わる。一時ファイルがエラー時・切断時に残らない。`ERROR_ALREADY_EXISTS`/`ERROR_FILE_EXISTS`/共有違反がすべて 409 にマップされる。
  - AC-T9.2.5 (C): ボディが 64 MiB を超えるストリームが 413 になり、一時ファイルが残らない。
  - AC-T9.2.6 (L): 監査記録(`AuditLog` の `Result` を返す経路)が失敗した場合、書き込みが 500 で失敗し、宛先ファイルが変更されない。
  - AC-T9.2.7 (W): 実機で、`--write-root` を承認済みターゲットの `dir` と重ねた構成で起動すると、起動ログに警告が出ることを確認する(evidence)。
- **リスク**: 高。新しい認可層とファイルシステム操作の正確性(パス正規化・原子性)が主な論点。ADR-0014 の §6/§7 の順序(字句 → OS 呼び出し → 比較、一時ファイル → 監査 → rename)を厳密に実装する。

## T9.3 永続実行コンテキスト PoC(ADR-0013)【T9.1 の後】

- **ADR**: 0013
- **内容**: 本実装の前に、ADR-0013 末尾の PoC 項目を小さく検証する。
  1. ラッパー内での stdout/stderr 捕捉が、pywinauto 等のネイティブ拡張の出力も含めて漏れなく拾えるか。
  2. `Stdio::piped()` の子に対する改行区切り JSON フレーミングが Windows で安定するか。
  3. アイドルタイムアウト・絶対寿命でのプロセス終了と Job Object(ADR-0009 §2)の関係。
  4. `Concurrency::Reject` と永続コンテキストの相互排他の実装場所。
  5. 毎ターンのハッシュ再検証(`approved/<hash>.json` 再読み込み)のコストが対話的応答性を損なわないか。
- **AC**:
  - AC-T9.3.1 (W): 上記 5 項目の検証結果を `docs/tasks/evidence/t9.3-persistent-context-poc.md` に記録する。
  - AC-T9.3.2: PoC 結果を ADR-0013 に反映し、Status を Proposed → Accepted(または設計変更を伴う改訂)に進めるかを判断する。
- **リスク**: 高(本 ADR の中で最も信頼モデルに触れる機能)。PoC 完了まで本実装(永続コンテキストのレジストリ・ターン API)には着手しない。

## 出荷順序

T9.0 → T9.1(pad2py が最も欲しい部分。早く出荷できる)。T9.2 は T9.0/T9.1 と並行可。T9.3 は T9.1 の後、本実装の着手前に単独で進める。
