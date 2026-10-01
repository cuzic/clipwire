# ADR-0009: 実行ランナー(共通基盤)

- Status: Proposed (改訂2: Opus 再レビュー反映)
- Date: 2026-09-30
- Depends on: なし(定義の型を ADR-0010 と共有するのみ。トークンの読み込み・除去は ADR-0011 が所有し、本 ADR は子プロセスの `env_remove` だけを持つ)
- Used by: ADR-0001, 0002, 0003, 0007

## Context

ADR-0001〜0003/0007 は「子プロセス起動・出力配信・プロセスツリー kill」を互いに前提にし合っていたため、共通基盤としてここに集約する。当初案のパイプ2本を読む方式は、クライアント切断で読み手が止まり子がパイプ満杯で固まる問題と、stdout/stderr の順序が保てない問題があった。一方、子にログファイルのハンドルを直接継承させる方式(初版の本 ADR)には、次の問題があることが再レビューで分かった。

- サーバには子の書き込みを止める手段がなく、ログ上限(10 MiB)を守れない。無限出力でシステムドライブが埋まる。
- 常駐アプリ(`cmd /c start awase.exe`)は、ログのハンドルを継承して持ち続ける。main 終了後もログに書き続け、容量上限が効かず、Windows では `jobs\<id>\` を削除できなくなる。

本質的な問題は「読み手がクライアントに律速されること」であり、パイプを使うこと自体ではない。そこで、**サーバ内の専用リレーが常にパイプを読み、ファイルにだけ書く**方式にする。

## Decision

### 1. 出力: 単一パイプ + リレースレッド → ジョブログ

- 各実行は `%LOCALAPPDATA%\clipwire\jobs\<id>\log` を持つ(ローミングされる `%APPDATA%` ではなくローカル側。`dirs_next::data_local_dir`)。
- 子の stdout と stderr には、**同じ匿名パイプの書き込みハンドル**(複製)を渡す。順序が保たれる。
- サーバ内のリレースレッドが、そのパイプを**常に**読み出し、ログファイルにのみ書く。HTTP クライアントの速度には律速されず、クライアントが切断してもデッドロックしない。
- **ログ上限**: ソフト上限 10 MiB を超えたら、警告マーカー `[output > 10 MiB: further output discarded]` を 1 回書き、以降は**読み捨てる**(パイプは読み続けるので子は詰まらない)。子は kill しない。ハード上限(256 MiB)は設けない(ディスクに書かないため不要)。
- Rhai の `print` / `debug` は `Engine::on_print` / `on_debug` で、**サーバが保持しているパイプの書き込み側**に書く(ファイルへ直接書くと、リレーに残っている子の出力より先に書かれて順序が崩れるため。`run(...); print("done")` の順序を保つ)。ADR-0007 の独自 `log` 関数は作らない。
- **サーバ停止時**: リレーが消えるため、子は書き込みで `EPIPE`(`ERROR_BROKEN_PIPE`)を受ける。その場合、該当ジョブは `lost`/`orphaned`(ADR-0002)として扱う。これは許容する。
- **main 終了後の残存プロセス**: main が終わっても、残存プロセスがパイプを持つためリレーはすぐに EOF にならない。main 終了から一定の猶予(2 秒)の後、リレーは読み取り側を閉じて終了する。以降、残存プロセスが標準出力へ書くと `EPIPE` を受ける。常駐アプリの起動は標準ハンドルを継承しない `start_detached`(ADR-0007)に移行する(移行ガイドに記載)。ログファイルのハンドルはサーバだけが持つので、`jobs\<id>\` の削除は常に可能。
- **終了の順序**: main 終了 → サーバが自分の書き込みハンドルを閉じる(Rhai では `run` のたびに子へ複製を渡し、サーバ自身も保持しているため、これを閉じないと残存プロセスがいなくても EOF が来ない)→ **EOF が来るか、2 秒間新しいデータが来ないかの早い方**でリレーを終える(固定 2 秒より、ディスクが遅くても末尾を失わない)。ストリームの `exit` イベントと meta の状態更新は、**リレーが終了した後**に出す(先に出すと follow 中のクライアントが最後の数行を受け取れない)。
- **読み取り側の閉じ方**: 匿名パイプの同期 `ReadFile` は、別スレッドの `CloseHandle` で解除できるとは限らない。(a) `FILE_FLAG_OVERLAPPED` 付きの名前付きパイプ(サーバ側)を作り、クライアント側のハンドルを子に渡して、読み取りは tokio の `named_pipe` + `timeout` で打ち切る、または (b) リレースレッドに `CancelSynchronousIo` を使う。PoC で決める。

### 2. Job Object

- 実行ごとに Job Object を 1 つ作る。制限は `JOB_OBJECT_LIMIT_BREAKAWAY_OK` のみ。`KILL_ON_JOB_CLOSE` は**付けない**(サーバ再起動・終了で常駐の子を道連れにしない)。
- 子を Job に入れる方法: `spawn()` 後の `AssignProcessToJobObject` は、`cmd /c start` が孫を作り終えた後に割り当てる競合がある。よって、起動時点から所属させる。
  - `CREATE_SUSPENDED` で起動 → Job に割り当て → 主スレッドを `ResumeThread`
  - または `CreateProcessW` + `PROC_THREAD_ATTRIBUTE_JOB_LIST`(自前実装の場合は `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` で**継承ハンドルを限定**する。限定しないと、並行して起動した別ジョブの子に、このジョブのパイプ書き込みハンドルが漏れる)
- Steps 形式: 各ステップの子を順に Job に入れる。Rhai 形式: スクリプトはサーバプロセス内(`spawn_blocking`)で評価され、`run` / `run_ok` が起動する子のみ Job に入る。サーバ自身は Job に入らない。
- サーバ自身が外側の Job(タスクスケジューラ等、`KILL_ON_JOB_CLOSE` 付き)に入っている場合、`start_detached` が `BREAKAWAY` 不許可で失敗しうる。その場合は失敗メッセージにその旨を明示し、サイレントに通常起動へフォールバックしない。
- **PoC で確認する項目**(実装着手前に小さな PoC を作る):
  1. `process-wrap` / `command-group` 等の crate が `KILL_ON_JOB_CLOSE` を既定で付けていないか、付けずに `BREAKAWAY_OK` を設定できるか。満たせなければ fork か自前実装。
  2. 起動時所属(`CREATE_SUSPENDED` 方式)で `cmd /c start` の孫が確実に Job に入ること。
  3. `CREATE_BREAKAWAY_FROM_JOB` で起動した子が Job から外れること。
  4. 単一パイプ + リレーで、stdout/stderr と Rhai の `print` の順序が保たれること。読み取り側を閉じる方式((a) 名前付きパイプ + tokio、(b) `CancelSynchronousIo`)が期待どおり動くこと。

### 3. 「running」の定義

- 状態(`running` / `orphaned` / `lost` などの定義は ADR-0002 が持つ)は **メインプロセスの終了**で遷移する: Steps なら最終ステップ、Rhai なら eval の終了。Job 内に残ったプロセスがあっても `running` は終わる。
- main 終了時、サーバは Job の**追跡をやめ、Job のハンドルを閉じる**。Windows ではプロセスを Job から外せず、`KILL_ON_JOB_CLOSE` も付けていないため、残存プロセスは Job のメンバーのまま生き続ける。ログに警告 `[warn] N processes still running (pids: ...)` を残す。これにより `cmd /c start awase.exe` 形式の常駐アプリ起動が従来どおり動き、`concurrency=reject`(ADR-0002)が永久に 409 を返し続ける問題を避ける。
- main が動いている間の kill / タイムアウト(ADR-0003)は `TerminateJobObject` でツリーごと終了する。Steps の途中のステップが残したプロセスは、同じ Job の中にあるため、後続ステップの timeout / kill で**一緒に終了する**(途中では解放しない)。
- **自己更新**は §7 に従う。

### 4. キャンセル

- `Engine::on_progress` はネイティブ関数実行中には呼ばれない。したがって:
  - `sleep` は 100ms 刻みでキャンセルフラグを確認するループで実装する。
  - `run` / `run_ok` の中断は、Job を `TerminateJobObject` して `wait` / `output()` を戻す方式にする。
  - 純 Rhai ループは `on_progress` で中断する。
- Rhai `Engine` には `set_max_operations` / `set_max_string_size` / `set_max_array_size` / `set_max_map_size` / `set_max_call_levels` を設定する。

### 5. 環境

- 子プロセス環境から `CLIPD_TOKEN` を `env_remove` する。トークンの読み込みと、自プロセスの `remove_var` は ADR-0011 が所有する。
- これは**事故防止**であり、境界ではない(同一ユーザーの承認済みスクリプトは `Get-CimInstance Win32_Process` でコマンドラインを読める)。

### 6. 構成

- `Runner` は(ジョブ ID, 定義, 引数, 要求元)を受け取り、ログ・Job Object・子プロセスを管理して `JobHandle` を返す。Steps と Rhai の両方がこれを使う。OS 依存部(Job Object、パイプ)は trait で分離し、非 Windows ではスタブ(プロセスグループ kill、通常のパイプ)とする。

### 7. 自己更新(clipwire 自身のビルド→再起動)

`start_detached` だけでは成立しない。次の制約がある。

- 単一起動ミューテックス(`Global\clipwire_singleton`、`acquire_mutex`)を旧サーバが保持している。新サーバは「既に起動中」で終了してしまう。
- 稼働中の `target\release\clipwire.exe` から起動したサーバがあると、`cargo build` はその exe を上書きできない。
- 自己更新は Rhai でしか書けない(Steps から `start_detached` は呼べない)。

方針:

1. ビルド成果物は稼働用の**固定パス**(`%LOCALAPPDATA%\clipwire\bin\clipwire.exe`)へ、名前の入れ替えで反映する(稼働中の exe は上書きできないが、名前の変更はできる)。手順: 新ビルドを `clipwire.new.exe` にコピー → 稼働中の `clipwire.exe` を `clipwire.old.exe` に改名 → `clipwire.new.exe` を `clipwire.exe` に改名 → `clipwire.exe serve --replace` を実行。`clipwire.old.exe` は次回起動時に削除する。**版ごとにパスを変えない**(Windows Defender ファイアウォールの受信許可は exe のパスごとに付き、スタートアップのショートカットは旧パスを指したままになって、再起動で旧版に戻るため)。
2. `clipwire serve --replace` を用意する。起動すると、名前付きイベント `Global\clipwire_shutdown` を通じて旧サーバに終了を要求し(HTTP を使わない。ADR-0010 §4 と同じく、管理操作を HTTP に載せない)、単一起動ミューテックスが空くまで待ってから起動を続ける。
   - 単一起動の判定は、現行の「`CreateMutexW` が `ERROR_ALREADY_EXISTS` なら起動済み」(存在で判定)をやめ、`WaitForSingleObject` による**所有権の取得**で行う(`WAIT_ABANDONED` も取得成功)。通常の起動は `WaitForSingleObject(h, 0)` で即判定し、`--replace` は待機にタイムアウト(30 秒)を設けて、超えたらエラーにする(存在で判定すると、新サーバのハンドルがオブジェクトを生かし続けて永久に待つ)。別ログオンセッションの旧サーバなど、要求が届かない場合も、このタイムアウトで失敗させる。
   - 旧サーバは停止要求を受けたら、ログを flush して `std::process::exit` する(tokio ランタイムの drop は、未完了の `spawn_blocking`(更新ジョブの Rhai eval)を待ってしまうため)。更新スクリプトは `start_detached` の後、新サーバの起動を**待たない**(待つとデッドロックする)。
   - `running` のジョブがあるときの `--replace` は拒否する(`--force` で強行。旧サーバが止まると他ジョブはリレーを失って `EPIPE` で失敗するため)。
3. 更新スクリプトは Rhai で `start_detached([copy_path, "serve", "--replace", ...])` を呼ぶ(`BREAKAWAY` 付きなので、ジョブの kill で新サーバが道連れにならない)。
4. 更新ジョブは旧サーバの終了とともに `lost` になる。結果は、新サーバの `/health` の `version`(ADR-0010 §2)で確認する。

## Consequences

- ADR-0001/0002/0003/0007 の循環が解消される(すべて 0009 に依存)。0009 は他の ADR に依存しない。
- 出力の順序保証と、切断・ログ上限・ハンドル保持の問題が解決する。ストリーミングは「ログの follow」に縮退し、実装が単純になる。
- サーバ停止時に、実行中の子が `EPIPE` を受ける(`lost` 扱い)。許容する代わりに、ログ上限とファイル削除の安全性を得る。
- 常駐アプリは、main 終了 2 秒後にパイプが閉じるため、標準出力への書き込みで `EPIPE` を受けうる。**Rust 製の常駐アプリ(awase など)は、`println!`/`eprintln!` がパイプ閉鎖後の書き込みで panic しうる**(Windows では `ERROR_NO_DATA` が `BrokenPipe` に対応付けられる)ため、`cmd /c start /b`(標準ハンドルを継承する)で起動しているターゲットは、**P4 の配備前に**標準ハンドルを継承しない起動(`Start-Process`。P7 以降は `start_detached`)へ書き換える(docs/tasks T4.1b)。
- Windows 固有の実装(起動時 Job 所属、BREAKAWAY、ハンドル継承の限定)は PoC で先に検証する。

## Alternatives considered

- **子にログファイルを直接継承(初版)**: ログ上限が守れず、常駐プロセスがハンドルを保持し続けるため却下。
- **パイプ + クライアント向けチャネル(最初の案)**: 読み手がクライアントに律速され、切断時にデッドロック。
- **ジョブ内の全プロセス終了まで running**: 常駐アプリ起動ターゲットで排他が詰まる。
- **`KILL_ON_JOB_CLOSE`**: サーバ再起動・自己更新時に子を巻き込む。
