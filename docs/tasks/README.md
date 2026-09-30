# 実装計画: clipwire ADR-0001〜0011

対象: `docs/adr/` の ADR(README の「実装順」と「脅威モデル」が前提)。
現状: `src/main.rs` 1 ファイル(約 1700 行)、**テストなし・CI なし**、Linux クライアントと Windows サーバが同一バイナリ。

改訂履歴: 2026-09-30 初版 → Opus レビュー(Critical 2 / High 5 / Medium 8 / Low 6)を反映して改訂1。

## 進捗(2026-09-30 時点)

| タスク | 状態 | 備考 |
|---|---|---|
| T0.1 | 完了 (L) | `scripts/check.sh`(host + windows-gnu) |
| T1.1 / T1.3 / T1.4 / T1.5 | 完了 (L) | コミット 8575a69。Opus レビュー指摘(H1, M1〜M5, L2, L3)反映済み |
| T1.2 | L 完了 / W 未検証 | AC-T1.2.3〜1.2.5(Windows 実機の toast)が未検証 |
| T0.2 | L 完了 / W 未検証 | AC-T0.2.1〜T0.2.3 完了。AC-T0.2.4(Windows 実機)は未検証 |
| T0.3 | L 完了 | AC-T0.3.1〜T0.3.3 完了 |
| T1.6 | 未着手 | Windows での入れ替えが必要 |
| T0.4〜T0.6, P2 以降 | 未着手 | |

## 運用の実態(計画の前提)

実データ(`~/.config/clipwire/targets.toml`、`clipwire-exec` スキル)から確認した事実。**以降の優先度はこれに基づく。**

- **運用は `--auto-approve`**。スキルに「このユーザーは自動承認運用にしている」と明記され、`register` 後は承認を待たず `exec` する。
- ターゲットは **1571 件、すべて script 形式**。大半は `fb-brave-inspect2〜14` のような**使い捨て**。156 件が `-EncodedCommand`/`FromBase64String`、3 件が `Get-Clipboard` の内容を `.ps1` に書いて `-File` で実行。最長の script は約 34,000 文字。全名・全 script が T1.1/T3.2 の規則を満たす(違反 0 件)。
- **開発ループの実体は `clipwire-rebuild-restart`**(msys2 側の別 checkout `C:\Users\cuzic\scoop\persist\msys2\home\cuzic\clipwire`)。`Get-Process clipwire | Stop-Process -Force` → `git pull; cargo build` → `Start-Process target\debug\clipwire.exe -ArgumentList 'serve','--auto-approve'`。**トークンは起動引数でなく、子が継承する環境変数 `CLIPD_TOKEN`** で渡っている。`clipwire-build`(`C:\Users\cuzic\clipwire`)は別 checkout でビルドするだけで再起動しない。**2 つの checkout がある**。
- 常駐アプリ(`awase.exe`)は `cmd /c start "" /b` で起動され、標準ハンドルを継承する。

### 運用モードの決定(OQ-0)

- **(A) 自動承認を正式な運用モードとする【採用】**。優先するのは、自動承認でも効く対策: 0011(HTTP 面)、監査、`serve` 起動時の警告、**自動承認でも承認レコードを書く**(T3.4)、ストアの堅牢化、ランナー、timeout、ストリーミング/ジョブ。
- **手動承認 UI**(2 段階 toast、レビュー画面、`approve --hash` の仕上げ = T5.3〜T5.5)は **「手動承認モード用」として後回し**(Phase 5b)。自動承認運用では一度も使われないため。`--auto-approve` なしで起動したときの安全な動作(= 承認待ちに入り、`approve --hash` で承認できる最小限)だけは、P3 で保証する。
- **(B) 手動承認へ移行する**場合は、「使い捨て実行」の代替設計が要る(汎用ターゲット + params など)。その場合は 5b と T7.5 を前倒しする。この計画は (A) を前提とし、(B) へ切り替えるときは README のこの節を更新する。
- 脅威モデル(`docs/adr/README.md`)には、**自動承認モードでは「トークン = Windows ユーザー権限でのコード実行」**であることを明記した(ADR 側も更新済み)。

## フェーズ一覧と依存

| Phase | ファイル | 内容 | ADR | 規模(幅) | 依存 |
|---|---|---|---|---|---|
| 0 | [00-foundation.md](00-foundation.md) | テスト・Windows CI・モジュール分割・ウォッチドッグ・toast PoC | — | M〜L | — |
| 1 | [01-immediate-fixes.md](01-immediate-fixes.md) | 即時修正 + `get` のトークン非展開 + `--token-file` | 0011 §A, §B-10 | S | **T0.1 のみ**(T0.2 を待たない) |
| 2 | [02-http-surface.md](02-http-surface.md) | トークン必須化、Origin/Host、`--token-file`(段階出荷) | 0011 §B | M〜L | P1, T0.4 |
| 3 | [03-definition-and-store.md](03-definition-and-store.md) | 正規形・ストア・承認レコード・プロトコル版 | 0010 | L | P0, P2 |
| 4 | [04-runner.md](04-runner.md) | Windows PoC → ランナー → 自己更新 | 0009 | **XL**(PoC 結果で +1 段階) | P3, T0.5, T0.6 |
| 5a | [05-timeout-audit-approval.md](05-timeout-audit-approval.md) | timeout・監査ログ・自動承認の記録 | 0003, 0011 §12 | M | P3, P4 |
| 5b | (同上の後半) | 手動承認 UI(toast・レビュー画面) | 0006 | **L**(T5.3 単独で L) | P5a。**後回し** |
| 6 | [06-streaming-and-jobs.md](06-streaming-and-jobs.md) | ストリーミング・ジョブ管理 | 0001, 0002 | L | P4, P5a |
| 7 | [07-features.md](07-features.md) | `list`/`status`、Rhai API、`exec --copy`(params は Deferred) | 0005, 0007, 0008 | M | P3〜P6(個別) |
| 8 | [08-release.md](08-release.md) | 互換性検証・移行・リリース | 全 | M | 全 |

```
T0.1 → P1(T1.1〜1.5 は T0.2 を待たない。T1.5 = `--token-file`)
T1.5 → T0.4(ウォッチドッグ)→ P2(段階出荷。T2.4 (b)(c))→ P3 → P4 → P5a → P6 → P7(個別)→ P8
T0.5(Windows CI)/ T0.6(toast PoC)は P3 着手前に完了
P5b(手動承認 UI)は P5a 以降、必要になった時点
```

規模の目安: S=半日以内、M=1〜2 日、L=3〜5 日、XL=1〜2 週間(1 人・AI 支援前提のおおまかな目安。確度は低い。「幅」は PoC・実機検証の結果で +1 段階ぶれうる)。

## 共通規約

### タスクの書式

各タスクは次の項目を持つ: **ID** / **ADR** / **内容** / **受入条件 (AC)**(`AC-<ID>.<k>`)/ **リスク・ロールバック**。

### 検証環境(区分)

| 区分 | 環境 | 用途 |
|---|---|---|
| **L** | Linux(`cargo test`) | 純粋ロジック。**Windows 固有実装の代わりのスタブしか検証できない**ので、Windows 固有の主張(順序保証・デッドロックしないこと・ファイルの削除可否・ツリー kill・ミューテックス)は L の AC としない |
| **C** | Windows CI(非対話。T0.5) | Job Object、名前付きパイプ、名前付きミューテックス、ファイルロック、`CreateProcess` フラグなど、**対話デスクトップを要しない Windows 固有挙動**を `#[cfg(windows)]` テストで自動検証 |
| **W** | Windows 実機 | toast、対話デスクトップ、Tailscale、ファイアウォール、常駐アプリなど |
| **X** | Linux ⇔ Windows の結合(実機) | 旧/新バイナリの組み合わせ、実ターゲットでの実行 |

AC には区分を括弧で付ける。**W と X の AC は、結果を `docs/tasks/evidence/<task>.md` に記録する**(日時・コミット・結果)。

### Definition of Done(全タスク共通)

1. AC がすべて満たされ、L と C の AC は CI(`scripts/check.sh` + Windows CI)で緑。
2. `cargo fmt --check` / `cargo clippy -- -D warnings` が、**ホスト(Linux)と `--target x86_64-pc-windows-gnu` の両方**で通る(T0.1。ツールチェーンは導入済み。逃げ道なし)。
3. 挙動を変える場合は ADR が実装と一致している(ズレたら ADR を先に直し、Opus レビューの対象にする)。
4. 互換性に影響する変更は `08-release.md` のリリースノート項目に追記。
5. 1 タスク 1 コミット以上、`feat(...)` / `fix(...)` 形式。

### 開発ループの安全網(重要)

- サーバの更新は **`clipwire-rebuild-restart`**(msys2 側 checkout)で行われ、**新サーバにトークンを渡すのは環境変数 `CLIPD_TOKEN`**。新サーバが起動しないと、旧サーバは既に殺されているためリモートから復旧できない。
- ウォッチドッグと再起動ターゲットの競合を避けるため、(1) 再起動ターゲットは**先にビルドし、成功したら停止して起動**する、(2) 更新中は `maintenance` ファイル(有効期限つき)でウォッチドッグを抑止する、(3) good.exe は `bin\good\clipwire.exe`(ファイル名を変えない)に置く、(4) 停止対象は PID ファイルで決める(T0.4、T2.4 (b))。P3 でストアを移行しても、P3 より前の good.exe が壊さないよう、移行は**追加だけ**(T3.4)。
- **自動承認では、トークン = コード実行権限**。トークンは会話ログに残さない(T1.4)、ローテーションする(T2.4 (b))、Tailscale ACL で接続元を制限する(T0.4)。
- したがって、**サーバ起動条件を厳しくする変更(T2.2 のトークン必須化、T2.4 の環境変数除去、T4.7 のミューテックス変更)を出荷する前に**、T0.4 の**ウォッチドッグ**(タスクスケジューラで `/health` を定期確認し、失敗したら `bin\clipwire.good.exe` を起動)が稼働していること、かつ再起動ターゲットが新しい起動条件を満たす形に書き換えられて実機で成功していること、を出荷の前提とする。

### 互換性方針

- **先にサーバ、後にクライアント**。新サーバは旧クライアントを受理する。旧サーバに対する新クライアントは、新機能の使用を送信前に拒否する(feature 確認。ADR-0010 §2)。**新クライアントは `proto < 2` のサーバには旧フラット形式で `register` する**(T3.6)。
- `proto` は T3.6 で 2 から始める(現行を 1 とみなす)。旧フラット形式は `proto=2` の間のみ受理。
- 互換性の検証は、過去のコミットを `git worktree` でビルドして L/C で自動化する(T8.1)。

### 実データ入力のテスト

`targets.toml`(1571 件、約 1.4 MB)はリポジトリ外(git 管理外)。テストは、環境変数 `CLIPWIRE_TARGETS_FIXTURE` でパスを指定されたときに実データを使い、未設定ならスキップする(CI は合成データで代替。実データでの検証は手元・実機で実施)。

### 未解決事項

- **OQ-1(解決)**: Windows でも `cargo test` を回す → T0.5 の Windows CI(GitHub Actions `windows-latest` または既存の GCP Windows spot runner)。
- **OQ-2**: ADR-0009 の PoC 結果(crate 採用可否、パイプ方式の (a)/(b))による ADR 更新のタイミング → T4.1 の完了条件。
- **OQ-3**: GitHub にリモートがあるか(Actions を使えるか)。なければ GCP Windows spot runner(`cargo-ci-gcp-spot-instance` スキル)。T0.5 の冒頭で確認する。
