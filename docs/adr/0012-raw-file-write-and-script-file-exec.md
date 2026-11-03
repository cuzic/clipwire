# ADR-0012: `script_file` — スクリプト本体を直接実行する新しい定義バリアント

- Status: Proposed(実装計画に進める段階。Opus 再レビューで Critical/High 指摘は解消。残りは §4 のウォッチドッグ対応ゲートのみ)
- Date: 2026-11-02(改訂2: 2026-11-03、Opus 再レビュー反映。`/write` を ADR-0014 として分離し、本 ADR は `script_file` のみを扱う)
- Depends on: ADR-0009(ランナー)、ADR-0010(正規形・ハッシュ・承認レコード)、ADR-0011(HTTP 面の信頼境界)
- Used by: ADR-0013(状態を持つ実行コンテキスト。`script_file` を起動スクリプトとして使う)
- See also: ADR-0014(生ファイル書き込み `/write`。当初本 ADR に同梱していたが、設計上の論点が多く分離した)

## Context

別プロジェクト(pad2py)が、GCP 上の Windows VM に対して clipwire を「対話的 RDP セッション内で pywinauto を動かす唯一の経路」として使っている(WinRM は Session-0 分離でスクリーンショット/UIA ツリー取得に失敗するため)。実機検証を経て、次がつらみとして報告された。

**入れ子クオーティング地獄**: Python スクリプトを実行したい場合、`register` の `steps` に `["powershell", "-NoProfile", "-EncodedCommand", <base64>]` という形で渡す必要があり、bash → Python → JSON → PowerShell という 4 層でクオーティング事故が頻発する。

調査の結果、**この問題の本質は JSON エンコーディングではない**と分かった。`register` の `script` / `steps`(`StepsDef::Text`)は、どちらも元から複数行の文字列を JSON 文字列値としてそのまま運べる(`src/config.rs`)。実際のつらみは、`steps` の各ステップが argv 配列としてしか起動できないため、複数行の Python スクリプトをステップの引数に直接書けず、`-EncodedCommand` に頼っていたこと。

(当初はファイル転送手段の欠如(`/write`)も本 ADR に同梱していたが、Opus レビューで `/write` 側に `script_file` より多くの設計論点が見つかったため、ADR-0014 として分離した。)

## Decision

### 1. `script_file` — スクリプト本体を直接実行する新しい定義バリアント

- `StoredTarget` / `CanonicalTarget` / `ExecPayload` に、`script` / `steps` と並ぶ 3 番目のバリアントとして `script_file` を追加する。

  ```rust
  struct ScriptFileDef {
      interpreter: Vec<String>, // 例: ["python"], ["pwsh", "-NoProfile", "-File"]
      body: String,             // 生のスクリプト本体(複数行)
      #[serde(default)]
      ext: Option<String>,      // 一時ファイルの拡張子。省略時は拡張子なし
      #[serde(default)]
      env: std::collections::BTreeMap<String, String>, // steps と同様に exec へ渡す
  }
  ```

  - `env` は `steps` と同じ扱いにする(`script` が今日 `env` をハッシュには含めつつ実行時に渡していない、という既存の食い違いは `script_file` では再現しない。明示的に渡す)。
  - `ext` は `^[A-Za-z0-9]{1,16}$` に限定する(validate_definition の制御文字検査だけでは、`ext` をパスの一部やファイル名と解釈した際のパストラバーサル・NTFS 代替データストリーム注入を防げないため)。
  - `CanonicalTarget` にも同形の `script_file` フィールドを追加し、**ハッシュ計算に含める**(ADR-0010 §1)。新しい承認種別は作らず、既存の register → pending/approved → exec 時再ハッシュ検証の経路をそのまま使う。
  - `validate_definition`(ADR-0010 §1 の制御文字検証)を `interpreter` の各要素・`body` に適用する。

- **排他は register 時にのみ強制する**。`script` / `steps` / `script_file` のうち 2 つ以上が設定された新規 register は 400 で拒否する。既に `script` と `steps` を両方持つ既存の承認済みエントリ(今日の実装は暗黙に `script` を優先し `steps` を無視する)は、`into_exec` では従来どおり許容し続ける(exec 時に 500 へ変える変更はしない。既存の承認済みストアを壊さない)。この非対称(新規は厳格、既存は寛容)を実装・リリースノートに明記する。
  - **副作用**: `validate_definition`(新しい排他チェックを含む)は、ADR-0010 §5 の「ハッシュが registered と同一なら no-op」判定より**前**に走る(`src/server/register.rs` の現在の呼び出し順)。そのため、`script`+`steps` を両方持つ**既存の**承認済み定義を、内容を変えずにそのまま再 register すると、同一ハッシュでも 400 になる。ユーザーはどちらか一方を落として再 register する必要があり、落とせばハッシュが変わるため手動承認運用では再承認が要る。これも「後方互換は無視する」の範囲内の破壊的変更として、リリースノートに明記する。
- **ウォッチドッグのフォールバックバイナリ対応が前提**: `StoredTarget` は `deny_unknown_fields` を持たないため、`script_file` を知らない旧バイナリが `registered.toml` を読み書きすると、このフィールドが黙って失われ、該当ターゲットは次回 `exec` 時にハッシュ不一致で動かなくなる(ADR-0010 §4 の `migrate_locked` は `hash`/`approved_at` が欠けている場合しか動かないため、これは直らない)。**本バリアントは、ウォッチドッグのフォールバック用バイナリが `script_file` を認識するバージョンに更新されるまで出荷しない**(別タスクとして `docs/tasks` に追加する)。
  - **この個別対応はクラス全体の問題の対症療法でしかない**: 今後 `script_file` に新しいフィールドを追加するたび(例: ADR-0013 の `persistent`)、同じ手順(フォールバックバイナリの更新を先に済ませる)を繰り返す必要がある。将来的に `StoredTarget` および内側の定義型に「未知フィールドを読み込んだまま保持して書き戻す」仕組み(例: `#[serde(flatten)] extra: toml::Table`)を入れれば、このクラスの問題自体を解消できる。本 ADR では個別のフィールド追加ごとのゲートで対処し、恒久対応は別途 ADR-0010 の改訂として検討する(本 ADR の対象外)。

### 2. 実行方法: 新しいランナー経路は作らない。配置先は実際に prune される場所にする

- exec 時、`JobRegistry::start` が返す job id は `Runner::spawn` を呼ぶ**前**に確定している。`script_file` の本体は、exec ハンドラがその時点で `state.jobs.log_path(&job_id).parent()`(= 実際に `JobRegistry::prune()` が削除する `jobs/<job-id>/` ディレクトリ。`src/server/exec.rs` の該当箇所、`src/jobs.rs` の `log_path`/`prune`)に `script.<ext>`(`ext` 省略時は `script`)として書き込む。`Runner::next_log_path`(`%TEMP%\clipwire-runner\<pid>-<n>\`。exec 経路では `Runner::with_output` が使われ、このディレクトリは `prune()` の対象外)は使わない。
- argv は `interpreter + [そのファイルの絶対パス]` とし、**既存の `Runner::spawn` に単発の Steps 相当のジョブとして渡す**(`Runner` 自体のコード変更は不要。変更が要るのは exec ハンドラ側)。`cwd` は従来どおり `StoredTarget.dir` を使う。
- 一時スクリプトファイルは `jobs/<job-id>/` に置かれるため、`JobRegistry::prune()`(直近 `RETAIN=50` 件の完了ジョブを超えた分を削除)がジョブログと一緒に削除する。専用の後始末処理は不要だが、直近 50 件分のスクリプト本体がディスクに残る点はリリースノートに記す。
- **PowerShell 5.1 の文字コード**: ADR-0010 §1 の制御文字検証は U+FEFF(BOM)を含む Cf を拒否するため、本文に BOM を含められない。Windows PowerShell 5.1 は BOM の無い `.ps1` を ANSI コードページ(日本語環境では CP932)として読むため、日本語の文字列リテラルが文字化けする。`ext == "ps1"` のときはサーバが書き込み時に UTF-8 BOM を**付与**するか、`pwsh`(PowerShell 7+、UTF-8 既定)の利用を推奨する旨をドキュメントに明記する(どちらにするかは実装時に決めてよい)。
- **`sys.path[0]`**: `python <絶対パス>` はスクリプト自身のディレクトリ(`jobs/<job-id>/`)を `sys.path[0]` に積む(cwd ではない)。`dir` からの相対 import に依存するコードは動かないことをドキュメントに明記する。

## Consequences

- pad2py の「bash → Python → JSON → PowerShell `-EncodedCommand`」という 4 層のクオート経由は、`register` の `target.script_file = {interpreter: ["python"], body: "<Python ソース>"}` という JSON 1 層に縮退する。`-EncodedCommand`・base64 は不要になる。
- `script_file` は既存の承認モデルにそのまま乗るため、手動承認運用でもスクリプト内容がトースト/レビュー画面に表示される(ADR-0006 の対象。表示フォーマットは本 ADR の対象外)。`/write`(ADR-0014)と異なり、本バリアントは手動承認運用の意味を壊さない(内容がハッシュ束縛・承認対象になるため)。

## Alternatives considered

- **`script_file` を `steps` の 1 ステップとして汎用化する(例: `{"write_and_run": {...}}` のような steps 内タグ)**: `steps` は argv 配列の列として設計されており、「ファイルに書いてから実行する」という副作用混じりのステップを 1 種類だけ混ぜ込むのは一貫性を崩す。`script`/`steps` と並ぶ独立フィールドにする方が、ハッシュ・検証・排他チェックが素直になる。
