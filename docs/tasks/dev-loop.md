# 開発ループ手順書 (T0.4)

サーバ(Windows 側 `clipwire serve`)を、リモートから安全に入れ替えるための手順。

## 2 つの checkout

| 名前 | パス | 役割 |
|---|---|---|
| rebuild-restart 用 | `C:\Users\cuzic\scoop\persist\msys2\home\cuzic\clipwire` | `clipwire-rebuild-restart` が `git pull` → `cargo build` → 起動する。**稼働中のサーバの実体** |
| build 用 | `C:\Users\cuzic\clipwire` | `clipwire-build` がビルドするだけ(再起動しない) |

重複の整理方針は T8.2。

## `clipwire-rebuild-restart` の現行定義(デコード済み、2026-09-30 時点)

ターゲットは `powershell Start-Process ... -EncodedCommand <base64> -WindowStyle Hidden` で、次のスクリプトをバックグラウンド実行する。

```powershell
Start-Sleep -Seconds 2
Get-Process clipwire -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
Set-Location 'C:\Users\cuzic\scoop\persist\msys2\home\cuzic\clipwire'
"=== rebuild started $(Get-Date) ===" | Out-File 'C:\Users\cuzic\clipwire-rebuild.log' -Encoding utf8
git pull *>> 'C:\Users\cuzic\clipwire-rebuild.log'
cargo build *>> 'C:\Users\cuzic\clipwire-rebuild.log'
Start-Process -FilePath 'target\debug\clipwire.exe' -ArgumentList 'serve','--auto-approve' -WindowStyle Hidden
Add-Content -Path 'C:\Users\cuzic\clipwire-rebuild.log' -Value '=== restart complete ==='
```

問題点(T2.4 (b) で直す):

- **先に停止してからビルドする**。ビルドが失敗してもサーバは既に死んでいる。
- 停止対象がプロセス名 `clipwire` で、PID ファイルを使わない。
- トークンは環境変数 `CLIPD_TOKEN` の継承頼み(`--token-file` ではない)。
- ウォッチドッグの抑止(`maintenance` ファイル)がない。

`clipwire serve` は、既存の設定ディレクトリ解決に従って
`clipwire.pid` を書く。Windows の通常環境では
`%APPDATA%\clipwire\clipwire.pid`、`CLIPWIRE_CONFIG_DIR` 指定時は
`%CLIPWIRE_CONFIG_DIR%\clipwire.pid` である。起動時に古い内容を上書きし、
Ctrl+C を含む正常終了時に削除する。再起動スクリプトはこのファイルの PID
だけを停止対象にする。

## ウォッチドッグ

`clipwire watchdog --good <bin\good\clipwire.exe> [--port 9999] [--interval 30] -- <serve の引数>`

- 30 秒ごとに `http://127.0.0.1:<port>/health` を確認する。
- **連続 3 回失敗**し、かつ保守ファイルが有効でないときだけ、`good serve --port <port> <serve の引数>` を起動する。
- 起動に失敗し続けると、待ち時間を倍々に増やす(上限 20 tick)。起動に成功した直後は 2 tick 様子を見る。
- 保守ファイル(既定 `%APPDATA%\clipwire\maintenance`。`--maintenance-file` で変更可): 中身は**有効期限の UNIX 秒**。期限が未来の間は復旧しない。壊れている・期限切れなら無視。
- 起動はコンソールなし・切り離し。Job の BREAKAWAY が許されなければ、フラグなしで再試行し、どちらだったかを `watchdog.log` に残す(AC-T0.4.7)。
- ログ: `%APPDATA%\clipwire\watchdog.log`。二重起動は名前付きミューテックスで防ぐ。

### good.exe の置き方

- `bin\good\clipwire.exe`(**ファイル名は `clipwire.exe` のまま**。復旧後もプロセス名が `clipwire` になり、`Stop-Process` の対象から外れない)。
- good は「実際に起動し `/health` が 200 だったバイナリ」だけにする。

### タスクスケジューラ

- トリガ: ログオン時。
- 「**ユーザーがログオンしているときのみ実行**」(「ログオンしているかどうかにかかわらず」はセッション 0 になり、クリップボードも toast も使えない)。
- 「タスクを停止するまでの時間」(3 日)を**無効**にする。
- 操作: `clipwire.exe watchdog --good ... -- --auto-approve --token-file <path>`

### 更新中の抑止

更新スクリプトの先頭で期限つきの保守ファイルを作り、終了時に消す。

```powershell
$m = Join-Path $env:APPDATA 'clipwire\maintenance'
[DateTimeOffset]::UtcNow.AddMinutes(15).ToUnixTimeSeconds() | Set-Content $m
try { <# build → 停止 → 起動 #> } finally { Remove-Item $m -ErrorAction SilentlyContinue }
```

## トークンのローテーション手順

1. 新しいトークンを生成する(例: 32 バイトの乱数の hex)。
2. Windows の token-file を新しい値で置き換える(サーバは起動時にだけ読むので、この時点では旧トークンが有効)。
3. サーバを再起動する(`clipwire-rebuild-restart`、または停止 → ウォッチドッグによる復旧)。
4. Linux 側の `CLIPD_TOKEN` を新しい値に更新する。
5. 確認: 旧トークンで `exec` が 401、新トークンで通る。
6. 永続的なユーザー環境変数 `CLIPD_TOKEN` が残っていれば削除する
   (`[Environment]::GetEnvironmentVariable('CLIPD_TOKEN','User')` が空であること)。

トークンは会話ログに出さない。

## 失敗時の復旧

- サーバが上がらない: ウォッチドッグが 1〜2 分で good を起動する。`watchdog.log` を見る。
- ウォッチドッグも動いていない: Windows で `clipwire.exe watchdog ...` を手で起動するか、タスクを手動実行する。
- good が壊れている: 過去の動作版をビルドし直して `bin\good\` に置く。

## 未検証 (Windows 実機)

AC-T0.4.1〜T0.4.4、T0.4.6〜T0.4.8 は実機で確認し、結果を `docs/tasks/evidence/T0.4.md` に記録する。
