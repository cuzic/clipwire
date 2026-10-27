# Phase 4: 実行ランナー(ADR-0009)

前提タスク: P0(T0.5 Windows CI、T0.6 toast PoC)、P3(`StoredTarget` 型・`Store`)。規模は XL(PoC の結果で +1 段階)。**最初に Windows での PoC(T4.1)を行い、結果で ADR-0009 を更新してから実装する**(PoC の結果次第で設計が変わるため)。

## T4.1 Windows PoC(ADR-0009 §2 の確認項目)

- **ADR**: 0009 §1, §2
- **内容**: 独立した小さな実験用クレート(`poc/runner-poc/`、本体には含めない。結果を残したら削除可)で、次を Windows 実機で検証し、`docs/tasks/evidence/T4.1.md` に記録する。
  1. 候補 crate(`process-wrap`, `command-group`)が `KILL_ON_JOB_CLOSE` を既定で付けるか、付けずに `BREAKAWAY_OK` を設定できるか。
  2. `CREATE_SUSPENDED` → Job 割り当て → `ResumeThread` で、`cmd /c start` が起動する孫が**確実に Job に入る**こと(100 回繰り返して 0 件の漏れ)。
  3. `CREATE_BREAKAWAY_FROM_JOB` で起動した子が Job から外れ、Job の kill で**死なない**こと。サーバが外側の Job に入っている場合の挙動。
  4. 単一匿名パイプの書き込み側を stdout/stderr 両方に渡したとき、順序が保たれること(交互に 1000 行ずつ書く子で検証)。
  5. リレー読み取りを閉じる方式: (a) `FILE_FLAG_OVERLAPPED` 付き名前付きパイプ + tokio `named_pipe` + `timeout`、(b) 匿名パイプ + `CancelSynchronousIo`。どちらが確実かを比較。
  6. 常駐プロセス(`notepad.exe` 等)が残った状態で、リレーの読み取り側を閉じた後、残存プロセスの標準出力書き込みが `EPIPE` になる(GUI アプリは書かないので影響なし)こと。
  7. `CreateProcessW` 自前実装の場合の `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` による継承ハンドル限定(並行起動で他ジョブの書き込みハンドルが漏れない)。
- **AC**:
  - AC-T4.1.1 (W): 上記 1〜7 の各項目に「結果(OK/NG)・使ったコミット/crate バージョン・再現コマンド」が `evidence/T4.1.md` に記録されている。
  - AC-T4.1.2 (W): NG の項目があれば、ADR-0009 の該当節が更新され(設計変更)、その差分が Opus レビュー対象になる。
  - AC-T4.1.3: (判断) crate 採用/自前実装、リレー方式 (a)/(b) が決まり、以降のタスクの前提として README に反映される。
- **リスク**: 項目 2 が NG(孫の漏れが避けられない)の場合、`PROC_THREAD_ATTRIBUTE_JOB_LIST` を使う自前実装が必須になり、工数が増える(T4.4 が L → XL)。

## T4.1b 常駐アプリ起動ターゲットの書き換え(P4 の**配備前**に完了させる)

- **ADR**: 0009 §1(リレーが閉じた後の残存プロセス)
- **背景**: `awase-build` は `cmd /c start "" /b target/debug/awase.exe` で起動し、`/b` により awase.exe はリレーのパイプ(標準ハンドル)を**継承する**。awase は Rust 製で、`println!`/`eprintln!` はパイプが閉じた後の書き込みで **panic しうる**(Windows では `ERROR_NO_DATA` が `BrokenPipe` に対応付けられる)。リレーが閉じた後に awase が標準出力へ何か書くと、常駐アプリがクラッシュする。`start_detached`(T7.4)は P7 の機能なので、P4 配備前は PowerShell の `Start-Process`(標準ハンドルを継承しない)で代替する。
- **内容**: 常駐アプリを起動するターゲット(`awase-build`, `awase-build-bg`, `awase-reboot` ほか。T8.2 の点検表で洗い出す)を、`Start-Process -FilePath <exe> -WindowStyle Hidden`(または同等の、標準ハンドルを継承しない起動)に書き換えて `register` する。
- **AC**:
  - AC-T4.1b.1 (C): **プローブ**: 毎秒 `println!` する小さな Rust プログラム(テスト用バイナリ)を、(i) 旧方式(`cmd /c start /b`、標準ハンドル継承)と (ii) 新方式(`Start-Process`、継承なし)で起動し、リレーが閉じてから 10 秒後の生存を確認する。**(i) は panic して死ぬ、(ii) は生存する**ことを示す(無操作の常駐アプリは書かないため、「5 分生存」だけでは壊れていても合格してしまう。テストが本当に効いていることを確認する)。
  - AC-T4.1b.2 (W): 書き換え後の `awase-build` / `awase-reboot` で awase.exe が起動し、**標準ハンドルを継承していない**ことを確認する(Process Explorer/`handle.exe` 等で標準入出力が親のパイプを指していない)。P4 のバイナリ配備後も同様。5 分生存は補助的な確認(記録のみ)。
  - AC-T4.1b.3: T8.2 の点検表の「常駐プロセス起動の有無」列で、該当する全ターゲットが書き換え済みになっている。
- **リスク**: `Start-Process` の挙動(CWD、相対パス)。→ `WorkingDirectory` を明示する。

## T4.2 `Runner` の型と OS 抽象

- **ADR**: 0009 §6
- **内容**: `Runner`, `JobHandle`, `JobSpec`(定義・引数・要求元・timeout)、`trait ProcessGroup`(`spawn_in_group`, `terminate`, `members`)。非 Windows 実装は、プロセスグループ(`setsid` + `killpg`)。`JobHandle` は、ログパス・状態・終了コードを提供。
- **AC**:
  - AC-T4.2.1 (L): スタブ実装で、`echo`・`false`・存在しないコマンドの 3 ケースが、期待する状態(成功・失敗・起動失敗)と終了コードを返す。
  - AC-T4.2.2 (L+C): `terminate` が子のツリー(孫を含む。`sh -c 'sleep 100 & sleep 100'`)を終了させる。
  - AC-T4.2.3 (L): `Runner` の API に Windows 固有の型が現れない(`cfg` は実装側のみ)。
- **リスク**: 抽象が Windows の実情に合わない。→ T4.1 の結果に基づいて設計する。

## T4.3 ログリレー(パイプ → ファイル、上限、終了順序)

- **ADR**: 0009 §1
- **内容**: 単一パイプ + リレースレッド。ソフト上限 10 MiB で読み捨て + マーカー。`exit`/meta 更新はリレー終了後。終了は「EOF か、2 秒間データなし」の早い方。ログは `%LOCALAPPDATA%\clipwire\jobs\<id>\log`(`dirs_next::data_local_dir`)。
- **AC**:
  - AC-T4.3.1 (C): stdout と stderr に交互に書く子(各 10,000 行)の出力が、書かれた順序でログに並ぶ。
  - AC-T4.3.2 (C): 20 MiB を出力する子で、ログは 10 MiB + マーカーで止まり、**子は正常終了する**(ブロックしない、kill されない)。
  - AC-T4.3.3 (C): リレーの終了後にのみ、ジョブの状態が `succeeded/failed` になり `exit` が配信可能になる(終了直前の出力が失われない: 最終行にマーカー文字列を書く子で 100 回繰り返して全件受信)。
  - AC-T4.3.4 (C): HTTP クライアントが存在しない(誰も follow しない)状態でも、大量出力の子が詰まらずに終了する(デッドロックしない)。
  - AC-T4.3.5 (C): ログファイルのハンドルが、ジョブ終了後に(残存プロセスが生きていても)サーバ以外に残らず、ジョブディレクトリを**削除できる**(T0.5.2 の足場を使う。**Linux では開いたファイルも削除できるため L では検証にならない**)。
  - AC-T4.3.6 (C): 常駐プロセスを `start`(main 終了後も生存)で起動するスクリプトで、main 終了から 2 秒以内にジョブが終了状態になり、ジョブディレクトリを**削除できる**。
- **リスク**: 読み取り側の閉じ方が環境依存。→ T4.1 の結果を採用。

## T4.4 Job Object(Windows 実装)

- **ADR**: 0009 §2, §3
- **内容**: T4.1 で決めた方式で `ProcessGroup` の Windows 実装。`BREAKAWAY_OK` のみ、`KILL_ON_JOB_CLOSE` なし。main 終了時は Job のハンドルを閉じて追跡をやめ、警告ログ(`[warn] N processes still running (pids: ...)`)。kill/timeout は `TerminateJobObject`。
- **AC**:
  - AC-T4.4.1 (C): `cmd /c "ping -n 100 127.0.0.1"` のツリーが、`terminate` で孫まで全て終了する。
  - AC-T4.4.2 (C): main 終了後に生存する子(`start notepad`)は、ジョブ終了時に**殺されず**、警告がログに 1 行出る。
  - AC-T4.4.3 (C): Steps の途中ステップが残した子(`start`)が、後続ステップの `timeout`/`kill` で**一緒に終了する**(ADR-0009 §3 の記述どおり)。
  - AC-T4.4.4 (C): サーバプロセスを `KILL_ON_JOB_CLOSE` 付きの外側 Job に入れた状態で、`start_detached` 相当(T7.x)が失敗し、メッセージが原因(BREAKAWAY 不許可)を示す(サイレントにフォールバックしない)。
  - AC-T4.4.5 (W): サーバを強制終了しても、実行中の子は(リレーが消えて `EPIPE` を受ける場合を除き)Job の KILL で道連れにならない。
  - AC-T4.4.6 (C): 50 個のジョブを並行起動しても、各ジョブの書き込みハンドルが他ジョブの子に漏れない(各ジョブ終了後、ログファイルを削除できる)。
- **リスク**: T4.1 項目 2 が NG の場合の工数。

## T4.5 Steps / Rhai の統合(`run`/`run_ok` の書き換え、`print`、上限、キャンセル)

- **ADR**: 0009 §1, §4, §5
- **内容**:
  - `handle_exec` の Steps 経路と `exec_rhai` の `run`/`run_ok` を `Runner` 経由にする。出力の結合(stderr→stdout の固定順)をやめ、ログの発生順に変更。
  - `Engine::on_print`/`on_debug` は**パイプの書き込み側**に書く。
  - Rhai `Engine` に `set_max_operations` / `set_max_string_size` / `set_max_array_size` / `set_max_map_size` / `set_max_call_levels`(値は実測で決め、ADR に記録)、`on_progress` によるキャンセル。
  - 子プロセスの環境から `CLIPD_TOKEN` を `env_remove`(Steps・Rhai 共通)。
  - 非キャンセルの既存挙動(終了コード、`script error:` の出力形式)は維持。
- **AC**:
  - AC-T4.5.1 (L): T0.2.2 の特性テスト(`exec_rhai` の既存挙動)が、出力の**順序以外**で同じ結果になる。順序の変更は、テストを更新し理由をコミットメッセージに書く。
  - AC-T4.5.2 (L): `run(["sh","-c","echo a; sleep 0.2"]); print("done")` の出力が `a` → `done` の順になる(Rhai の `print` と子の出力の順序。N-M1)。
  - AC-T4.5.3 (L): 無限ループ `loop {}` を含むスクリプトが、上限(operations)またはキャンセルで停止し、`script error:` を返す。
  - AC-T4.5.4 (L): Rhai のループで文字列を連結し続けるスクリプト(`let s = "a"; loop { s += s; }`)が、`max_string_size` で停止し `script error:` を返す(メモリを使い尽くさない)。
  - AC-T4.5.5 (L): 子プロセスが `env` を表示したとき、`CLIPD_TOKEN` が存在しない(テストでは `CLIPD_TOKEN` をセットして起動し、`env` の出力に含まれないことを確認)。
  - AC-T4.5.6 (L): キャンセル要求で、`run` 中の子がツリーごと終了し、`run` が戻る。
  - AC-T4.5.7 (X): **代表セット**(`awase-build`, `awase-reboot`, `adb-status-check`, `clipwire-rebuild-restart`、Steps 形式の合成例 1 件)が、実機で、従来と同じ結果で `exec` できる(出力の順序以外)。全件は L で、パース・検証・正規化・ハッシュ決定性のみ確認する(T3.2.4、T3.4.5)。
- **リスク**: 出力順序の変更が、出力をパースしている利用者(スクリプト)に影響する。→ `evidence/T4.5.md` に既存ターゲットの出力を前後比較して記録。

## T4.6 ジョブディレクトリの保持・削除(基礎)

- **ADR**: 0002 §8(ロジックは P6 で完成させるが、削除可否の基礎をここで確認)
- **AC**:
  - AC-T4.6.1 (C): ジョブ終了直後(残存プロセスあり)に `jobs\<id>\` を削除でき、ファイルロックで失敗しない(T4.3.6 と共通)。
- **リスク**: なし(確認のみ)。

## T4.7 単一起動ミューテックスの判定方式の変更

- **ADR**: 0009 §7
- **内容**: `acquire_mutex` を、`CreateMutexW` の `ERROR_ALREADY_EXISTS` 判定から、`WaitForSingleObject` による所有権取得(通常起動は待機 0、`--replace` は 30 秒)に変更。`WAIT_ABANDONED` を取得成功として扱う。**ミューテックス名は設定ディレクトリから導出する**(既定の設定ディレクトリでは従来の名前 `Global\clipwire_singleton` と同一。別設定ディレクトリの検証用インスタンスを同時起動できるように。T0.3、T5.6.3)。
- **AC**:
  - AC-T4.7.1 (C): 2 個目の `serve` が即座に「起動済み」で終了する(従来どおり)。
  - AC-T4.7.2 (C): 1 個目を強制終了(ミューテックスが abandoned)した直後に、2 個目が起動できる。
  - AC-T4.7.3 (C): 旧サーバが終了しても、存在判定のためにオブジェクトが生き続けて起動できない、という事態が起きない(`--replace` なしでも、旧プロセス終了後に新規起動できる)。
- **リスク**: 起動時の排他が緩む/きつくなる。→ W で 3 ケースすべてを確認。

## T4.8 自己更新(`serve --replace`)

- **ADR**: 0009 §7
- **内容**: `serve --replace`: 名前付きイベント `Global\clipwire_shutdown` で旧サーバに終了要求 → ミューテックス取得を最大 30 秒待つ → 起動。旧サーバはイベントを受けると、ログを flush して `std::process::exit`(tokio ランタイムの drop を待たない)。**更新ジョブ自身以外**に `running` ジョブ(P6 で確定。ここでは「実行中の子の有無」)があれば拒否、`--force` で強行。`--replace` を起動するのは実行中の更新ジョブ自身なので、**起動側が `--replace --initiator-job <id>`(または環境変数)で自分のジョブ ID を渡し**、旧サーバはそれ以外の running があるときだけ拒否する(渡さないと常に拒否され、自己更新が成立しない)。固定パス `bin\clipwire.exe` への名前入れ替え手順をスクリプト(`scripts/update-windows.ps1` 相当)として提供。**初回の `bin\clipwire.exe` への移行前に、既存の `exec` で `New-NetFirewallRule -Program <bin\clipwire.exe>` を実行して受信許可を先に作る**(パスが変わると初回だけファイアウォールの許可ダイアログが出て、応答はローカルでしかできないため)。
- **AC**:
  - AC-T4.8.1 (W): 旧サーバ稼働中に `clipwire.exe serve --replace` を実行すると、旧サーバが終了し、新サーバが起動して `/health` の `version` が新しくなる。
  - AC-T4.8.2 (W): 更新ジョブ以外に実行中の Rhai `exec`(`sleep(30)` を含む)があると `--replace` が拒否され、`--force` で旧サーバが終了する。
  - AC-T4.8.8 (W): **更新ジョブ以外に実行中のジョブが無ければ、`--force` なしで置き換えが成功する**(`--initiator-job` が渡されている場合。更新ジョブ自身は除外される)。`--initiator-job` なしで、自分自身のジョブが running のときは拒否される。
  - AC-T4.8.9 (W): 初回の `bin\clipwire.exe` 移行で、事前に作ったファイアウォール規則により、許可ダイアログが出ずに Tailscale から接続できる。
  - AC-T4.8.3 (W): 旧サーバの Rhai eval が走っていても(`spawn_blocking` 未完了)、旧プロセスが 5 秒以内に終了する。
  - AC-T4.8.4 (W): 旧サーバが存在しない状態での `--replace` が、通常起動として成功する。
  - AC-T4.8.5 (W): 要求が届かない(別セッションの旧サーバを模擬)場合、30 秒のタイムアウトでエラー終了する。
  - AC-T4.8.6 (C+W): 名前入れ替え手順(`clipwire.new.exe` → `clipwire.exe`)が、稼働中の `clipwire.exe` があっても成功し、Windows Defender ファイアウォールの許可(パス固定)が失われない(再起動後も Tailscale から接続可能)。
  - AC-T4.8.7 (W): Rhai スクリプトから `start_detached`(T7.x)で新サーバを起動する自己更新ターゲット(`clipwire-update`)が、ジョブの kill/timeout に巻き込まれずに完了する。結果は新サーバの `/health` の `version` で確認できる。
- **リスク**: 失敗すると開発ループが途切れる。→ T0.4 の `bin\clipwire.good.exe` と帯域外復旧手順を**実施前に**確認する。
