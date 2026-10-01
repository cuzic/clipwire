# Phase 1: 即時修正(ADR-0011 §A)

既存の脆弱性。**T0.1 の完了だけを前提**に、T0.2(大規模分割)を待たず、現行の `main.rs` に最小限のテストを付けて入れてよい(1 タスク 1 コミット、互いに独立)。なお、自動承認運用ではトークン保持者は元々任意のコードを実行できるため、C1 の実害は「手動承認モード・トークンなしモード」に限られる(緊急度はそれに応じて判断してよいが、修正自体は小さいので先に入れる)。

## T1.1 ターゲット名の検証

- **ADR**: 0011 §A-1
- **内容**:
  - 共通関数 `validate_target_name(&str) -> Result<()>`(正規表現 `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`、`regex` を使わず手書きでよい)。
  - `handle_register` / `handle_exec`(サーバ)と `cmd_register` / `cmd_exec`(クライアント、早期エラー)で使用。
  - サーバ起動時に、既存の `registered.toml` / `pending.toml` の名前を検査し、違反があれば `tracing::warn!`(起動は止めない)。
- **AC**:
  - AC-T1.1.1 (L): 次の名前が `register` / `exec` で 400 になる: 空、65 文字、先頭 `.`、先頭 `-`、`..`、`a/b`、`a b`、`a'b`、`a<b`、`a\u{2018}b`、NUL を含む、日本語、改行を含む。次が通る: `awase-build`, `a`, `a.b_c-d`, 64 文字。
  - AC-T1.1.2 (L): 400 のとき、`pending.toml` / `registered.toml` が変更されない。
  - AC-T1.1.3 (L): 既存ファイルに違反名があっても、サーバは起動し、警告ログが 1 件出る。
  - AC-T1.1.4 (L): クライアント `register`/`exec` が、違反名ではネットワークアクセスの前にエラー終了する(終了コード非 0)。
- **リスク**: 既存の正当な名前が違反する可能性。→ 現在の `targets.toml` の全名前で AC-T1.1.1 の「通る」ケースを確認(`awase-build-bg` 等)。

## T1.2 toast 経路の安全化(XML エスケープ・PowerShell 廃止)

- **ADR**: 0011 §A-2, A-3
- **内容**:
  - `show_register_toast_impl`: `XmlDocument::LoadXml(format!(...))` をやめ、DOM API(`CreateElement` / `CreateTextNode`)で組み立てる、または全埋め込み値を XML エスケープする関数を経由させる。
  - `show_simple_toast`(PowerShell `-Command` 経由)を**削除**し、`show_balloon` は WinRT `ToastNotification`(ボタンなしの `ToastGeneric`)を使う。
  - toast 表示失敗時のフォールバックは、`toast.log` への記録と `tracing::error!` のみ(外部プロセス起動なし)。
  - エスケープ関数 `xml_escape(&str) -> String` は `#[cfg(windows)]` の外(`src/util.rs`)に置き、L でテスト可能にする。
- **AC**:
  - AC-T1.2.1 (L): `xml_escape` の仕様: `& < > " '` をエンティティに置換する。XML 1.0 で表現できない C0 制御文字(TAB・LF・CR 以外)は**除去する**(文字参照でも表現不能なため)。テスト: (i) 入力を「XML 1.0 で表現可能な文字」に限定したプロパティテストで、埋め込んだ XML を標準パーサでパースすると元の文字列に復元される。(ii) 制御文字を含む入力では、除去後の文字列になる(別テスト)。
  - AC-T1.2.2 (L): ソースに `powershell` を `Command::new` する箇所が toast/バルーン経路に存在しない(`grep` ベースの回帰テスト: `rg 'ShowBalloonTip|NotifyIcon' src/` が 0 件)。
  - AC-T1.2.3 (W): `name` は T1.1 で制限されるため通常は到達しないが、**直接 `show_register_toast` をテストハーネスで** `a<b&c'd"e` を渡しても、toast が表示され(または表示失敗が `toast.log` に記録され)、**電卓などの外部プロセスが起動しない**。
  - AC-T1.2.4 (W): 通常の register → toast の承認/拒否が従来どおり動作する(`evidence/T1.2.md`)。
  - AC-T1.2.5 (W): toast 表示に失敗する状況(AUMID 未登録など)を作り、フォールバックで外部プロセスが起動しないこと、`toast.log` に記録されることを確認。
- **リスク**: WinRT のみにすると、toast が出ない環境(集中モード等)で通知が消える。→ `toast.log` と `clipwire approve` の案内(ADR-0006)で補う。

## T1.3 トークン比較の定数時間化

- **ADR**: 0011 §A-4
- **内容**: `check_auth` の `s == format!("Bearer {expected}")` を、長さ非依存の定数時間比較(`subtle::ConstantTimeEq` または手書き。依存追加は `cargo add subtle` 可)に。
- **AC**:
  - AC-T1.3.1 (L): 一致・不一致(長さ違い、1 バイト違い)・ヘッダなし・`Bearer` なし・大文字小文字違い(`bearer`)の結果が従来と同じ(T0.2.3 の特性テストが緑のまま)。
  - AC-T1.3.2 (L): 比較関数が `==` を直接使っていない(コードレビューで確認。テストは困難なので AC は手順的)。
- **リスク**: 低。

## T1.4 `get` の出力にトークンを展開しない

- **ADR**: 0011(自動承認モードではトークン = コード実行権限)、README の脅威モデル
- **背景**: `cmd_get`(`files` / 仮想ファイルの分岐)は、`curl -H 'Authorization: Bearer <トークン>' ...` の行を**トークンを展開した状態で**標準出力に出す。これは Claude の会話に貼られ、`~/.claude/projects/*.jsonl` に永続化され、tmux 経由でクリップボードにも出る。自動承認モードでは、会話ログを読める者はそれだけで Windows 上で任意コードを実行できる。
- **内容**: curl の行は `-H "Authorization: Bearer $CLIPD_TOKEN"`(シェル変数への参照)の形で出力する。トークンが未設定のときは `-H` 自体を出さない。必要なら、curl 行を不要にする `clipwire fetch` サブコマンドを別途検討(backlog)。
- **AC**:
  - AC-T1.4.1 (L): `get`(files・仮想ファイル)の標準出力・標準エラーに、テスト用トークンの値が**含まれない**(トークンをセットして実行し、出力を `grep`)。
  - AC-T1.4.2 (L): 出力された curl 行の `$CLIPD_TOKEN` を、シェルが展開して正しく認証が通る(`CLIPD_TOKEN` をエクスポートした状態で、出力行を `sh -c` で実行するテスト。モックサーバ)。
  - AC-T1.4.3 (L): 他のサブコマンド(`put`/`exec`/`register`)の出力・エラーメッセージにもトークンが含まれない(網羅テスト)。
- **リスク**: 既存のスキル(`clipwire`)が curl 行をそのまま実行している場合、`CLIPD_TOKEN` が子シェルに渡っていることが前提になる。→ スキル定義(`contrib/claude-skills/clipwire.md`)を確認し、必要なら更新する(T8.3)。

## T1.5 `--token-file` の追加(旧 T2.4 (a) をここへ移動)

- **ADR**: 0011 §B-10
- **理由**: T0.4 のウォッチドッグがトークンを `--token-file` で受け取るため、T0.4 より前に出荷する必要がある(旧計画では T2.4(a) で P2 にあり、順序が逆だった)。**起動条件は厳しくならない**(既存の `--token` / `CLIPD_TOKEN` も従来どおり使える)ので、P1 に置いてよい。
- **内容**: `--token-file <path>`(中身の前後の空白・改行を除去。空ファイルはエラー)。優先順位: `--token` > `--token-file` > `CLIPD_TOKEN`。`--token` をコマンドラインで使うと非推奨の警告ログ(トークンの値は出ない)。`remove_var` はここではしない(T2.4 (c))。
- **AC**:
  - AC-T1.5.1 (L): 3 つの指定方法それぞれで認証が通り、優先順位が仕様どおり。
  - AC-T1.5.2 (L): 空の token-file、存在しない token-file でエラー終了。
  - AC-T1.5.3 (L): `--token` 指定時に警告がログに出る。トークンの値はログに出ない。
- **リスク**: 低。

## T1.6 フェーズ 1 の出荷

- **内容**: T1.1〜T1.3 を含むバイナリを W でビルド・入れ替える(T0.4 の手順)。
- **AC**:
  - AC-T1.6.1 (X): **代表セット**(`awase-build`, `awase-reboot`, `adb-status-check`, `clipwire-rebuild-restart`)が従来どおり `exec` できる(再 register は不要)。全 1571 件のパース・検証は L で T3.2.4 が確認する(全件の実行は、使い捨て・破壊的・外部状態依存のものを含むため行わない)。
  - AC-T1.6.2 (X): 旧バージョンのクライアントバイナリ(T0.2 直前のビルド)でも `get`/`put`/`exec` が動く。
