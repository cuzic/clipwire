# Phase 8: 互換性検証・移行・リリース

前提タスク: 各フェーズの出荷。**このフェーズは、フェーズごとの出荷時(P1, P2, P3, P4, P5, P6)にも繰り返し使う**(マトリクスを更新しながら)。

## T8.1 互換性マトリクス(クライアント × サーバ)【L/C で自動化、X は Windows 固有のみ】

- **内容**: 過去のコミット(現行 HEAD a884091 = `v0`、および各フェーズの出荷タグ)を `git worktree` でビルドし、`tests/compat/` から旧/新のクライアントとサーバを組み合わせて起動する(サーバは Linux ではスタブ付き、Windows CI では実装)。確認項目: `get` / `put` / `exec`(既存形式のターゲット)/ `register`(旧フラット定義)/ `/health` / `exec --detach`(P6 以降)/ `list`(P7 以降)。結果は `evidence/compat-matrix.md` に表で記録。X(実機)は、toast、Job Object の実挙動、Host 名解決など Windows 固有の項目のみ。
- **AC**:
  - AC-T8.1.1 (L/C): 「新サーバ × 旧クライアント」の全組み合わせで、`get`/`put`/`exec`/`register`(旧定義)が動く(後方互換の保証範囲)。
  - AC-T8.1.2 (L/C): 「旧サーバ × 新クライアント」で、新機能の使用が**送信前に**明確なエラーで拒否され、旧機能は動く。**新クライアントが旧サーバに入れ子形式で register して空のターゲットが登録される事故が起きない**(T3.6.5)。
  - AC-T8.1.3 (L): 旧クライアントの挙動を再現するテスト(旧フラット形式の `register`、`Accept` なしの `/exec`、`/health` の `OK\n`)が、`tests/compat/` に常設される。
  - AC-T8.1.4 (X): Windows 固有の項目(toast、Host 名解決、ファイアウォール)を、実機の代表的な組み合わせ(v0/現行)で 1 回ずつ確認する。
- **リスク**: 旧バイナリの入手。→ 各フェーズ出荷時に、タグとリリースバイナリ(`bin\releases\`)を保存。

## T8.2 既存ターゲットの移行点検と棚卸し

- **ADR**: 0003 §移行、0010 §3(移行)、0009 §3(常駐アプリ)
- **内容**: 実機の全 registered(ローカル `targets.toml` は 1571 件。大半は `fb-brave-inspect2〜14` のような使い捨て)を次の観点で点検する。
  - **代表セット**(以降の実機 AC の対象): `awase-build`, `awase-reboot`, `adb-status-check`, `clipwire-rebuild-restart`, Steps 形式の合成例 1 件。**全件の実行はしない**(使い捨て・破壊的・外部状態依存を含む)。全件は L で、パース・検証・正規化・ハッシュ決定性のみ確認する(T3.2.4、T3.4.5)。
  - 点検表の列: 実行時間の実測(代表セット)、30 分超の可能性、**常駐プロセス起動の有無(T4.1b と連動)**、`start_detached` への書き換えの要否、ハッシュ(`list` が `ok`)。
  - **lint**: 次を検出して一覧化する — `Get-Clipboard` と `-File` / `iex` の組み合わせ(承認された定義の外から、実行時にコードを差し込める。自動承認運用でも設計上の穴として記録)、`-EncodedCommand` / `FromBase64String`(レビューで読めない。156 件)。
  - **使い捨ての棚卸し**: 使い捨てターゲットの削除方針(古いものから削除する手順、`registered` と `approved/` の掃除、Linux 側 `targets.toml` の整理)を決めて、実施する。
  - 2 つの checkout(`C:\Users\cuzic\clipwire` の `clipwire-build` と、msys2 側の `clipwire-rebuild-restart` / `clipwire-rebuild-log`)の整理(どちらを正とするかを決め、重複を解消)。
- **AC**:
  - AC-T8.2.1 (X): 代表セットが、P6 のバイナリで `exec` でき、`list` が `ok`。全件は `list` で `changed`/`unregistered` を出さない(L: ハッシュ決定性)。
  - AC-T8.2.2 (X): 30 分を超える可能性のあるターゲットが、明示的な `timeout` を持つ(なければ該当なしと記録)。
  - AC-T8.2.3 (X): `awase-build-bg`/`awase-build-log` が不要になったことを確認し、廃止(`targets.toml` から削除、Windows 側の registered からも削除)。
  - AC-T8.2.4 (X): `clipwire-rebuild-restart` が、P4 の自己更新(`clipwire-update`。`--replace --initiator-job` 経由)に置き換わり、実機で完走する。
  - AC-T8.2.5: lint の結果(該当件数と一覧)が `evidence/T8.2.md` に記録されている。
  - AC-T8.2.6: 使い捨ての棚卸しの実施前後の件数、`approved/` の件数が記録されている。

## T8.3 リリースノートと移行ガイド

- **内容**: `docs/RELEASE_NOTES.md`(または `CHANGELOG.md`)に、次の**破壊的変更**と移行手順を書く。
  1. トークンなしの運用で `exec`/`register` が 403 になる(P2)。`--bind-localhost-only` だけでは足りず、`--token-file` を設定するか、`--allow-no-token` を明示する必要がある。`--allow-no-token` は認証なしでコード実行を公開することになる(tailnet 全ノードが対象。Tailscale ACL が前提。ADR-0011 §5・§6)。`--auto-approve --allow-no-token` も起動できるが強い警告が出る。
  2. `--auto-approve` にはトークンまたは明示的な `--allow-no-token` が必須(P2)。
  3. `approve --dir` の廃止、`approve` に `--hash` が必須(P3)。
  4. 承認が toast の 2 段階になる(P5)。`clipwire approve <name> --hash <prefix>` が、toast が使えないときの導線。
  5. `exec` の出力順序の変更(stderr → stdout の固定順から、発生順へ)(P4)。
  6. 暗黙の 30 分タイムアウト(P5)。
  7. `registered.toml` の形式移行(`approved/` の作成、`pre-migrate.bak`)。移行後に旧バージョンへ戻すときの注意(旧サーバは新形式を読めない → バックアップからの復旧手順)。
  8. 定義に制御文字・双方向文字が含まれる場合の拒否(P3)。
  9. `README.md`(リポジトリ直下)の更新: 新コマンド、`contrib/clipwire-*.md`(スキル定義)の更新。
  10. 単一起動ミューテックス名が設定ディレクトリ由来になる(T0.3)。旧版サーバから更新するときは、旧版を停止してから新版を起動する(旧名と新名が異なるため、ローリング起動では一時的な二重起動を防げない)。`CLIPWIRE_CONFIG_DIR` を指定した検証用インスタンスは本番と並行起動できる。
  11. HTTP リクエスト検査の追加(P2)。`Origin` ヘッダ付きのリクエストは 403、保護ルートへの `POST` が `Content-Type: application/json` でない場合は 415 になる。Host 検査の初回出荷は `--host-check=log` が既定で、不一致・欠落は警告ログに記録するが通過させる。`--host-check=enforce` では不一致・欠落が 421 になり、必要な名前は `--allow-host` で複数追加できる。既存 CLI は必要なヘッダを送るため影響を受けず、独自クライアントは各ヘッダを合わせる。
- **AC**:
  - AC-T8.3.1: 上記 1〜11 がすべてリリースノートに記載され、各項目に「影響を受ける条件」「対処」が書かれている。
  - AC-T8.3.2: `README.md` と `contrib/claude-skills/clipwire*.md` が、新しいコマンド・承認フローと矛盾しない(旧コマンドの案内が残っていない。`approve` の使い方、`exec` の使い方)。
  - AC-T8.3.3: Linux 側の `clipwire-exec` スキル(`~/.claude/skills/clipwire-exec/`。リポジトリ外)の更新案を `docs/tasks/evidence/skill-update.md` に書く。**現行スキルの「このユーザーは自動承認運用にしている。`register` 後は黙って `exec` する」という記述と整合させる**(自動承認モードでも、トークン = コード実行権限であること、`--token-file`、ジョブ/ストリーミングの新コマンド、`CLIPD_HOST` の指定方法)。手動承認モード用の記述は、Phase 5b を実施するときに追加する。

## T8.4 ロールバック手順の検証

- **内容**: 各フェーズの出荷後に、前フェーズのバイナリへ戻せることを実機で確認(`bin\clipwire.good.exe`)。P3 の形式移行後の旧版への復旧(`registered.toml.pre-migrate.bak` から)を含む。
- **AC**:
  - AC-T8.4.1 (W): P3 の移行済み環境で、旧(P2)サーバを起動し、`pre-migrate.bak` を戻すと、代表セットが `exec` できる。**移行後に register したターゲットは `pre-migrate.bak` に含まれず失われる**ため、自動承認運用では Linux 側の `targets.toml` から再 register して復旧する手順を、手順書(`dev-loop.md`)に書く(AC: 手順書にその記述がある)。
  - AC-T8.4.2 (W): P6 → P5 → P4 と順にバイナリを戻しても、起動でき、ストア(`pending`/`registered`/`approved`)が壊れない(`jobs\` は旧版が無視する)。

## T8.5 最終受入(全体)

- **AC**:
  - AC-T8.5.1: すべての ADR の Status が `Accepted`(または `Deferred`)に更新され、実装との乖離がない。
  - AC-T8.5.2: `scripts/check.sh` が緑、`cargo test` が緑(L 区分の全 AC に対応するテストが存在する。AC-ID をテスト名またはコメントに含める)。
  - AC-T8.5.3: W/X 区分の全 AC に、`evidence/` の記録がある(未実施は「未実施・理由」を明記)。
  - AC-T8.5.4 (X): ADR-0011 の脅威モデルに対する最終確認: (1) `name` に細工した register で外部プロセスが起動しない、(2) ブラウザ経由の `exec` が拒否される、(3) トークンのみで承認を完了できない。
  - AC-T8.5.5: メモリ(`~/.claude/projects/-home-cuzic-powershell-clipd/memory/`)の `project_overview.md` / `project_toast_approval.md` / `project_next_steps.md` を、新しい構成・承認フローに更新する。
