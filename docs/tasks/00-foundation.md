# Phase 0: 基盤(テスト・モジュール分割・開発ループ)

前提: 現状はテストも CI もなく、ロジックが `src/main.rs` に集中している。以降のフェーズで「L 区分で検証できる純粋ロジック」を増やすため、先にテスト可能な形にする。**挙動は変えない。**

## T0.1 テスト・lint 基盤

- **ADR**: —
- **内容**:
  - `Cargo.toml` に dev-dependencies(`tempfile`, `assert_cmd` or 同等、`proptest` は任意)を追加。**`pnpm`/`uv` ルールの対象外(Rust)なので `cargo add` を使う**。
  - `scripts/check.sh`: `cargo fmt --check` / `cargo clippy -- -D warnings` / `cargo test` を順に実行。
  - CI(Linux + Windows)は T0.5。ここでは `scripts/check.sh` がローカルで通ること。
  - 既存コードの clippy 警告は、このタスクで**挙動を変えない範囲**で解消するか、`#[allow]` を理由付きで付ける。
- **AC**:
  - AC-T0.1.1 (L): `scripts/check.sh` がクリーンな checkout で exit 0。
  - AC-T0.1.2 (L): `cargo test` が 1 件以上のテストを実行する(空のテストバイナリでない)。
  - AC-T0.1.3 (L): `scripts/check.sh` が `cargo check` と `cargo clippy -D warnings` を **`--target x86_64-pc-windows-gnu` でも必須で**実行し、緑になる(サンドボックスにツールチェーンは導入済み。「無ければ記録」の逃げ道は設けない)。
- **リスク**: clippy 警告の解消で挙動が変わる。→ 警告の解消は機械的なものに限り、`cargo test`(T0.2 の回帰テスト)で確認。

## T0.2 モジュール分割(挙動不変)

- **ADR**: 0009 §6(OS 依存の分離)、0010
- **内容**: `src/main.rs` から、テスト可能な純粋ロジックを抽出する。**抽出のみで、ロジックは変えない**。
  - `src/config.rs`: `clipwire_config_dir`, `load_target_map*`, `save_target_map`, `TargetMap`, `StoredTarget`, `StepsDef`, `ExecPayload`
  - `src/client.rs`: `ClientConfig`, `cmd_*`(クライアント側)
  - `src/server/`: `handle_*`, `check_auth`, `run_serve`(`mod.rs`, `exec.rs`, `register.rs`, `clip.rs`)
  - `src/exec_rhai.rs`: `exec_rhai`
  - `src/win.rs`(`#[cfg(windows)]`): 既存の `win_clip` をそのまま移す
  - `main.rs`: CLI 定義と `main`
- **AC**:
  - AC-T0.2.1 (L): 分割前後で `clipwire --help` と各サブコマンドの `--help` の出力が一致する(分割前の出力を `tests/fixtures/help/*.txt` に固定して比較)。
  - AC-T0.2.2 (L): `exec_rhai`・`StepsDef::into_argv`・`load_target_map`/`save_target_map` の**特性テスト**(現状の挙動を固定)が存在し緑。`exec_rhai` は `run`/`run_ok`/`file_exists`/`rm` それぞれ、成功・失敗・存在しないコマンドのケース。
  - AC-T0.2.3 (L): `check_auth` のテスト(トークンなし・一致・不一致・ヘッダなし)。
  - AC-T0.2.4 (W): 分割後のバイナリを Windows でビルドし、`serve` 起動 → Linux から `get`/`put`/`exec`(既存ターゲット 1 つ)が従来どおり動く(`evidence/T0.2.md`)。
- **リスク/ロールバック**: 大きな差分。1 コミットで機械的に移動し、ロジック変更を混ぜない。問題があれば `git revert`。

## T0.3 結合テストのためのサーバ起動ヘルパー(L)

- **内容**: Linux 上で `serve`(Windows 依存部をスタブ化した状態)をランダムポートで起動し、`ureq` クライアントで叩く結合テスト用ヘルパー(`tests/common/mod.rs`)。非 Windows では、clip 系ルートを 501 か固定応答にするスタブで足りる。`exec`/`register` は実処理が動く。
- **AC**:
  - AC-T0.3.1 (L): ヘルパーでサーバを起動し、`GET /health` が 200・`OK\n`。
  - AC-T0.3.2 (L): `POST /register` → pending.toml が一時ディレクトリに作られる(設定ディレクトリを環境変数 or 引数で差し替え可能にする。既定の挙動は不変)。**単一起動ミューテックスの名前は設定ディレクトリから導出**し、別設定ディレクトリの検証用インスタンスを、本番サーバと同時に起動できる(T5.6.3 の検証用)。
  - AC-T0.3.3 (L): テストが並列実行でポート・ディレクトリを衝突させない。
- **リスク**: `--auto-approve` なしの `register` は toast を呼ぶ(`#[cfg(windows)]`)。非 Windows では呼ばれないので L では問題なし。

## T0.4 Windows 開発ループの安全網(ウォッチドッグ)

- **前提**: T1.5(`--token-file`)が出荷済みであること(ウォッチドッグがトークンを token-file で渡すため)。
- **内容**(事実: 開発ループの実体は `clipwire-rebuild-restart`。README「運用の実態」参照):
  - **ウォッチドッグ = `clipwire watchdog` サブコマンド**(ログオン時に 1 回だけ起動して内部でループする。PowerShell を毎分起動するとコンソールがちらつくため)。動作: 30 秒ごとに `GET /health`。**連続 3 回失敗したときだけ**、かつ `%LOCALAPPDATA%\clipwire\maintenance`(有効期限つき)が**存在しない**ときだけ、`bin\good\clipwire.exe serve <現行と同じ起動引数>`(トークンは `--token-file`)を起動する。起動に失敗し続ける場合はバックオフし、連続失敗をログに残す。
  - **good.exe のファイル名は `clipwire.exe` のまま**、置き場所だけ別ディレクトリ(`bin\good\clipwire.exe`)にする(good で復旧した後もプロセス名が `clipwire` で、`Stop-Process` の対象から外れないように)。
  - **タスクスケジューラの設定**: 「ユーザーがログオンしているときのみ実行」(「ログオンしているかどうかにかかわらず」はセッション 0 で動き、クリップボードも toast も使えないサーバが起動する)。「3 日を超えたら停止」を無効にする。
  - `clipwire-stage` ターゲット: ビルド → 直前の動作版を `bin\good\clipwire.exe` として退避(退避前に `good` の `--version` と `/health` を確認)→ 新版を `bin\clipwire.new.exe` に配置。退避と配置の sha256 を出力に含め、`evidence/` に残す。
  - 手順書 `docs/tasks/dev-loop.md`: 2 つの checkout の役割、`clipwire-rebuild-restart` の現行定義(EncodedCommand をデコードして記録)、更新前の手順、失敗時の復旧、**トークンのローテーション手順**(token-file の差し替え → 再起動 → Linux 側の更新 → 旧トークンの無効化確認)。
  - **ネットワーク層の二重化**: Tailscale の ACL で、Windows の 9999/tcp への接続を Linux 側ノード(タグ)からだけに限る(「tailnet 上の第三者」への境界を、トークン一本からネットワーク層との二重にする。自動承認ではトークン = コード実行権限のため)。`serve --allow-peer <IP/CIDR>`(接続元 IP の許可リスト。Host ヘッダではなく peer アドレスで判定)は backlog に入れる。
  - `clipwire-build` と `clipwire-rebuild-restart` の重複整理の方針は T8.2。
- **AC**:
  - AC-T0.4.1 (W): ウォッチドッグが稼働し、サーバを手動で強制終了すると、**手作業なしで 2 分以内に** `bin\good\clipwire.exe` で復旧して `/health` が 200。結果を `evidence/T0.4.md` に記録。
  - AC-T0.4.2 (W): 健全な状態で 10 分観察して、サーバのプロセスが 1 つのまま(二重起動しない)。`maintenance` ファイルがある間は、サーバを止めても復旧を始めない。期限が切れたファイルは無視される。
  - AC-T0.4.3 (W): `clipwire-stage` の出力に、`good` と `new` の sha256 が含まれ、`evidence/` に記録されている。
  - AC-T0.4.4 (W): 新バイナリが起動しない状況(試験用ターゲット)を作っても、ウォッチドッグで自動復旧する。
  - AC-T0.4.5: `docs/tasks/dev-loop.md` に、`clipwire-rebuild-restart` の現行定義(デコード済み)、2 つの checkout の役割、トークンのローテーション手順が書かれている。
  - AC-T0.4.6 (W): ウォッチドッグのタスクが終わった後も、good.exe が **24 時間以上生存**する(タスクの Job・「3 日で停止」に巻き込まれない)。
  - AC-T0.4.7 (W): ウォッチドッグから起動したサーバで、Job の BREAKAWAY が許されているか(`IsProcessInJob` と、`start_detached` 相当の起動)を確認し、結果を記録する(AC-T4.4.4 の実機版。不許可なら ADR-0009 §2 の注意どおり失敗メッセージが出る)。
  - AC-T0.4.8 (X): Tailscale の ACL で、Linux 側ノード以外から 9999/tcp に接続できない(別ノード、または ACL のテスト機能で確認)。
- **リスク**: ウォッチドッグ自体のバグで二重起動・無限再起動する。→ 単一起動ミューテックス、連続 3 回の失敗条件、バックオフ。good.exe が壊れていると復旧できない → good は「過去に実際に起動・`/health` 200 を確認したバイナリ」のみ。**P3 で移行したストアを、P3 より前の good.exe が壊さない**ことは T3.4(追加だけの移行)で担保する。

## T0.5 Windows CI

- **ADR**: —(検証基盤)
- **内容**: まず OQ-3(GitHub にリモートがあるか)を確認する。あれば GitHub Actions(`windows-latest` + `ubuntu-latest`)、なければ GCP Windows spot runner(`cargo-ci-gcp-spot-instance` スキル)。Windows ジョブで `cargo test`(`#[cfg(windows)]` テストを含む。対話デスクトップを要しないもの: Job Object、名前付きパイプ、名前付きミューテックス、ファイルロック・削除、`CreateProcess` フラグ)と `clippy` を実行。toast・Tailscale・常駐アプリなど対話や環境に依存するものは W/X に残す。
- **AC**:
  - AC-T0.5.1 (C): Windows ジョブで `cargo test` が緑になり、`#[cfg(windows)]` のテストが 1 件以上実行される(空でない)。
  - AC-T0.5.2 (C): Windows ジョブで、ファイルを開いたままの別プロセスがあるとき `remove_dir_all` が失敗し、閉じると成功する、というテスト(Linux では成立しない性質)が緑になる(以降の「削除できる」系 AC のための足場)。
  - AC-T0.5.3: Linux ジョブと Windows ジョブの両方が、PR / push で自動実行される(または手元で 1 コマンドで両方実行できる)。
- **前提の確認**: (1) リモート(`github.com/cuzic/clipwire`)が**非公開**なら、GitHub-hosted の Windows ランナーは Linux の 2 倍の分数で課金されるため、公開/非公開を最初に確認し、非公開なら GCP spot を既定にする。(2) CI ランナーでは、ジョブの各ステップが**外側の Job Object の中**で動くことがあり、BREAKAWAY 系・「サーバを殺しても子が死なない」系のテストの結果が実機と変わる。テスト内で `IsProcessInJob` を確認し、前提と違う場合は skip して W に回す。(3) 対話セッションではないので、名前付きミューテックスのテストは `Global\` だけを使い、テストごとに一意な名前にする。(4) 実機の Windows 側ツールチェーン(msys2 配下の gnu か msvc か)と、CI のターゲットを一致させる(実機の値を確認して記録)。
- **リスク**: Windows ランナーのコスト・待ち時間。→ GCP spot の利用(既存スキル)。CI が使えない場合、C 区分の AC は W(実機)へ降格して `evidence/` に記録する(その場合は OQ として記録)。

## T0.6 toast PoC(P3 着手前・T4.1 と並行)

- **ADR**: 0006 の Consequences(PoC 項目)、0011 §A(WinRT 一本化)
- **内容**: ランナーと無関係なので、P3 より前に実施する。結果は `evidence/T0.6.md`。
  1. 非パッケージアプリ(COM activator 未登録)で、アクションセンターからボタンを押したとき in-process の `Activated` が発火するか。
  2. サーバ再起動後に、前プロセスが出した toast のボタンを押すとどうなるか(ショートカットの AUMID 経由で引数なしの clipwire が起動しないか)。
  3. 集中モード中の `scenario="reminder"` の挙動。
  4. ボタンなしの `ToastGeneric`(`notify` 用。T1.2・T7.4)が、AUMID 登録済みの状態で確実に表示されること。
- **AC**:
  - AC-T0.6.1 (W): 上記 1〜4 の結果(OK/NG・条件)が `evidence/T0.6.md` に記録され、NG がある場合は ADR-0006 の該当節を更新する(手動承認 UI は Phase 5b。結果は 5b の設計に使う)。
- **リスク**: なし(調査のみ)。
