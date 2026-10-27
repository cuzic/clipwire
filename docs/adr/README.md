# Architecture Decision Records

clipwire の設計判断。Status は `Proposed` → `Accepted` / `Rejected` / `Deferred` / `Superseded`。

2026-09-30: ADR-0001〜0008 の初版を Opus にレビューさせ(Critical 4 / High 5 / Medium 10 / Low 8)、共通基盤として 0009〜0011 を新設し、0001〜0008 を改訂した。同じレビュアーによる再レビュー(Critical 1 / High 4 / Medium 7 / Low 7)を反映して改訂2とした(承認の HTTP API 化の撤回、ログのリレー方式、自己更新、レビュー画面の整形など)。

| ADR | タイトル | Status | 依存 |
|---|---|---|---|
| [0011](0011-http-surface-trust-boundary.md) | HTTP 面の信頼境界と即時修正 | Proposed | — |
| [0010](0010-canonical-definition-and-approval-record.md) | 定義の正規形・ハッシュ・承認レコード・承認の経路 | Proposed | — |
| [0009](0009-execution-runner.md) | 実行ランナー(リレー経由のジョブログ・Job Object・自己更新) | Proposed | — |
| [0003](0003-timeout-and-audit-log.md) | タイムアウトと監査ログ | Proposed | 0009, 0010 |
| [0006](0006-approval-hardening.md) | 承認フローの強化(縮小版) | Proposed | 0010, 0011, 0003 |
| [0001](0001-exec-output-streaming.md) | `/exec` の出力ストリーミング | Proposed | 0009 |
| [0002](0002-detached-jobs.md) | ジョブ管理 (detach / jobs / logs / kill) | Proposed | 0009, 0010, 0011 |
| [0005](0005-list-and-status.md) | `list` / `status` | Proposed | 0010 (`status` は 0002 も) |
| [0007](0007-rhai-api-extension.md) | Rhai API の拡充(縮小版) | Proposed | 0009, 0011 |
| [0004](0004-parameterized-targets.md) | パラメータ付きターゲット(`choices` のみ) | **Deferred** | 0010, 0006 |
| [0008](0008-clipboard-history-and-exec-copy.md) | `exec --copy` / クリップボード履歴 | `--copy` Proposed / 履歴 Deferred | — |

依存は非循環。0009 / 0010 / 0011 は互いに独立(トークンの読み込み・除去は 0011 が所有し、0009 は子の `env_remove` のみ)。0003 / 0001 / 0002 / 0007 は 0009 に、0006 / 0005 / 0004 は 0010 に依存する。

**承認は HTTP に載せない**(0010 §4): トークンは Linux 側のクライアントも持つため、承認 API があれば人間の承認を迂回できる。承認・拒否は Windows のローカルでファイルを直接操作する。

## 脅威モデル(守る対象・守らない対象)

- **守る対象**: (1) Linux 側エージェントの誤操作・意図しないコマンド、(2) tailnet 上の第三者、(3) Windows 上のブラウザ経由の攻撃(CSRF / DNS rebinding)。
- **守らない対象**: (1) **Linux 側そのものの乗っ取り・悪意**、(2) 承認済みターゲットがビルド・実行するリポジトリの中身、(3) Windows 上で承認済みスクリプトが実行するコード(ユーザー権限で何でもできる)、(4) SSH 等で Windows に入れる者(承認 CLI を実行できる)。
- **帰結**: リポジトリのコードをビルド・実行するターゲット(`awase-build`、`clipwire-build` など)の承認は、**そのリポジトリへの書き込み権を持つ者に Windows ユーザー権限を渡すことと同じ**。`build.rs` / proc-macro / テスト / 依存 crate は承認の対象外の任意コードとして Windows 上で動き、承認ファイルの書き換えや、自己更新(0009 §7)による**サーバの実行ファイルの差し替え**もできる。
- **運用モードの前提(実運用は `--auto-approve`)**: 自動承認モードでは、`register` が承認を兼ねるため、**トークン = Windows ユーザー権限でのコード実行権限**である(`--allow-no-token` を併用する場合は、0011 §5・§6 のとおり**トークンもなく、ネットワーク到達性 = コード実行権限**になる。Tailscale ACL が前提)。このモードで効く対策は、0011(トークン必須、Origin/Host/Content-Type、`name` の検証、toast 経路の安全化)、監査(0003。`serve-start auto_approve=true` と `approve`(承認者 `auto`))、承認レコードの記録(0010 §3。自動承認でも書く)、ストアの堅牢化(0010 §4)、timeout/ランナー(0003/0009)である。0006 の承認 UI(2 段階 toast・レビュー画面)は**手動承認モード専用**で、自動承認運用では使われない。
- したがって、ハッシュ束縛・制御文字の拒否・全文レビュー画面(0006)・承認の非 HTTP 化(0010 §4)は、「誤操作と、ネットワーク越しの第三者」への境界であり、「悪意ある Linux 側」への境界ではない。ビルド系ターゲットは、承認時にこの点を意識する。

## 実装順

0. **即時修正(ADR-0011 §A)**: `name` の検証、toast XML のエスケープ、PowerShell バルーン経路の廃止、定数時間のトークン比較。ADR の他の実装を待たずに行う。
1. **ADR-0011 §B**(保護ルートの認証(トークン、または `--allow-no-token` の明示)、Origin/Host/Content-Type 検査、`--auto-approve` のトークンまたは `--allow-no-token` の要求)。
2. **ADR-0010**: `BTreeMap` 化(最初のリリースに必ず含める)、サーバ計算ハッシュ、定義の制御文字検証、入れ子リクエスト + `deny_unknown_fields`、`/health` の proto/features、原子書き込み + 名前付きミューテックス、破損時の書き込み拒否、承認レコード(正規 JSON)、承認はローカルのファイル直接操作。
3. **ADR-0009**: ランナー(リレー経由のログ、Job Object、自己更新 `serve --replace`)。先に小さな PoC で「crate の `KILL_ON_JOB_CLOSE` 既定」「起動時の Job 所属」「BREAKAWAY」「単一パイプの順序」を検証。
4. **ADR-0003**(timeout 短縮のみ、監査 1 世代)と **ADR-0006**(ハッシュ束縛承認、2 段階 toast、no-op 再登録、拒否時の掃除)。
5. **ADR-0001 + 0002**(ログ follow によるストリーム、`jobs` / `logs` / `kill`、`reject` のみの排他)。
6. **ADR-0005**(`list` を先に、`status` は 0002 の後)、**ADR-0007**(`sleep` / `start_detached` / `notify`)、**ADR-0004**(`choices` のみ)。
7. **ADR-0008 の `exec --copy`** は他と独立なので、いつでもよい。
8. **Deferred / 見送り**: クリップボード履歴(0008)、TTL・静的警告(0006)、`env` / `retry` / `wait_port` / `read_text`(0007)、`queue`(0002)、`pattern`(0004)。
