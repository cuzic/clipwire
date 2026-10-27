# Phase 2: HTTP 面の信頼境界(ADR-0011 §B)

**破壊的変更を含む**(トークンなしの localhost 運用は `exec`/`register` が使えなくなる)。P8 のリリースノートに必ず記載する。**出荷前提**: T0.4 のウォッチドッグが稼働していること(README「開発ループの安全網」)。

前提タスク: P1 完了。

## T2.1 ルーティングの分類とミドルウェア基盤

- **ADR**: 0011 §B
- **内容**: axum の `Router` をルート種別ごとに分ける。
  - **保護ルート**(トークン必須): `/exec`, `/register`(将来の `/jobs*`, `/targets*` を含む)
  - **クリップボード系**: `/`, `/clip`, `/file`, `/vfile`, `/open`(従来の起動条件を維持)
  - **共通**: `/health`
  - 共通ミドルウェア(tower layer): Origin 拒否、Host 許可リスト、Content-Type 検査(POST・保護ルート)。
- **AC**:
  - AC-T2.1.1 (L): ルート一覧がテーブル駆動テストに列挙され、新規ルート追加時に分類がないとテストが失敗する(ルート定義から分類を強制)。
  - AC-T2.1.2 (L): ミドルウェアの適用順(Host → Origin → Content-Type → 認証)が固定され、各段の拒否ステータスが定義どおり(421 / 403 / 415 / 401)。**認証されていないリクエストにも 415/421/403 を返す**ことは仕様として許容する(情報漏洩の実害はない)。
- **リスク**: 既存ルートの挙動変更。→ T0.3 の結合テストで回帰確認。

## T2.2 トークン必須化(保護ルート)

- **ADR**: 0011 §B-5, B-6
- **内容**:
  - 保護ルートの認証(ADR-0011 §5。**2026-09-30 改訂**): トークンあり → 必須(401)。トークンなし + `--allow-no-token` → **認証なしで受け付ける**(Origin・Host・Content-Type は適用)。トークンなし + `--allow-no-token` なし(`--bind-localhost-only` だけを含む) → 403(本文に理由と対処: `--token-file` または `--allow-no-token`)。
  - `--auto-approve` は、トークンまたは `--allow-no-token` が必須。どちらもなければ起動エラー(エラー文に両方を書く)。`--auto-approve --allow-no-token` のときは、「到達できる者は誰でも任意コードを承認なしで実行できる」ことを明示する**強い警告**と、監査イベント `auth=none` を出す。起動時に**警告ログ**と監査イベント(`serve-start auto_approve=true`。T5.2 で実装するまではログのみ)を残す。**toast は出さない**(再起動ターゲットで毎回のビルドごとに toast が出るのを避ける。自動承認は正式な運用モード=README OQ-0 (A))。
  - クリップボード系ルートは従来どおり(`--allow-no-token` / localhost 限定で許可)。
- **AC**:
  - AC-T2.2.1 (L): トークンなし・`--allow-no-token` なし(`--bind-localhost-only` のみを含む)で起動したサーバに対し、`POST /exec` と `POST /register` が 403。`GET /clip` は従来どおり 200(スタブ)。
  - AC-T2.2.1b (L): トークンなし + `--allow-no-token` で起動したサーバでは、`POST /exec` と `POST /register` が `Authorization` なしで従来どおり動く。ただし `Origin` 付きは 403、`Content-Type` が JSON でない POST は 415、不許可 `Host` は(`enforce` なら)421。
  - AC-T2.2.2 (L): `--auto-approve` かつトークンなし・`--allow-no-token` なしの起動が、非 0 終了とエラーメッセージ(両方の選択肢を案内)で失敗する。`--auto-approve --allow-no-token` は起動でき、強い警告が 1 回出る。
  - AC-T2.2.3 (L): トークンあり・正しい `Authorization` で `exec`/`register` が従来どおり動く。誤ったトークンは 401。
  - AC-T2.2.4 (L): `--auto-approve` 起動時に警告ログが 1 行出る(`warn` レベル、トークンの値は出ない)。`auth=none` のときは、より強い文言の警告が 1 行出る。
  - AC-T2.2.5 (X): **現行の `clipwire-rebuild-restart`**(トークンは環境変数 `CLIPD_TOKEN` 経由、`serve --auto-approve`)を、この版のサーバで実行しても、新サーバが起動して `/health` が 200 になる(環境変数経由のトークンが引き続き使えること。T2.4 を出荷する前の確認)。
- **リスク**: `--bind-localhost-only` + トークンなしで運用している環境(現在の運用確認が必要)。→ 着手前に現行の起動コマンドを確認し、トークン設定手順をリリースノートに。

## T2.3 Origin / Host / Content-Type

- **ADR**: 0011 §B-7, B-8, B-9
- **内容**:
  - `Origin` ヘッダがあれば全ルートで 403。
  - `Host` 許可リスト(ポート除去、大文字小文字非依存): `127.0.0.1`, `localhost`, Tailscale IP(`find_tailscale_ip`、リクエストごと or キャッシュ付き)、短いホスト名(Windows: `GetComputerNameExW`、非 Windows: `gethostname`)、`tailscale status --json` の `Self.DNSName`(FQDN、および**先頭ラベル**)と `Self.HostName`(取得失敗時は省略)、`--allow-host`(複数指定可)。不一致・欠落は 421。
  - **log-only モード**(`--host-check=log|enforce`、**初回出荷は `log` が既定**): 不一致をログ(`warn`、Host の値を含む)に出すだけで拒否しない。`clipd.log` に正当なクライアントの不一致が 0 件であることを 1 出荷期間(実運用で数日)確認してから `enforce` を既定にする(別コミット)。Tailscale のマシン名と Windows のコンピュータ名が一致する保証はなく(管理画面で変更可能)、許可リストの取りこぼしで全リクエストが 421 になり、exec で直せなくなる事故を避けるため。
  - 保護ルートの `POST` は `Content-Type: application/json` 必須、それ以外は 415。クライアント(`ureq`)が既に付けていることを確認。
- **AC**:
  - AC-T2.3.1 (L): `Origin: http://evil.example` 付きの全ルートが 403。`Origin` なしは通る。
  - AC-T2.3.2 (L): `Host: 127.0.0.1:9999`, `Host: LOCALHOST`, `Host: <許可した名前>:9999` が通り、`Host: evil.example`、`Host` なし(HTTP/1.0)は 421。
  - AC-T2.3.3 (L): `Content-Type: text/plain` の `POST /exec` が 415。`application/json; charset=utf-8` は通る。
  - AC-T2.3.4 (L): `--allow-host foo` を指定すると `Host: foo` が通る。
  - AC-T2.3.5 (X): 実機で、短いホスト名・Tailscale IP・MagicDNS FQDN・**Tailscale のマシン名(`tailscale status` の 2 列目。`clipwire-exec` スキルが `CLIPD_HOST` に使う値)** のそれぞれを `CLIPD_HOST` に設定して、`get`/`exec` が通る(log-only モードで、ログに不一致が出ないこと)。
  - AC-T2.3.7 (L): `--host-check=log` では不一致のリクエストが通り、ログに 1 行出る。`enforce` では 421。
  - AC-T2.3.8 (X): **`enforce` へ切り替える前提**: log-only で実運用した期間の `clipd.log` に、不一致が 0 件である(件数を `evidence/T2.3.md` に記録)。
  - AC-T2.3.6 (X): ブラウザ(Windows の Edge/Chrome)で、別オリジンのページから `fetch("http://127.0.0.1:9999/exec", {method:"POST", body:"{}"})` を実行し、サーバ側でリクエストが拒否される(Origin 付きで 403)。
- **リスク**: 許可リストの取りこぼしで、正当なクライアントが 421 になる。→ 421 の応答本文に「許可されているホスト名の一覧」と `--allow-host` の案内を含める。

## T2.4 `--token-file` とトークンの環境からの除去(3 段階で出荷)

- **ADR**: 0011 §B-10(所有権は 0011)
- **背景**: 現行の再起動ターゲットは、トークンを環境変数 `CLIPD_TOKEN` の継承で新サーバに渡している。サーバが自プロセスの `CLIPD_TOKEN` を消す(または子へ渡さない)と、`clipwire-rebuild-restart` が起動する新サーバにトークンが届かず、`--auto-approve`(トークン必須)で**起動エラー → 旧サーバは既に殺されているので復旧不能**になる。
- **内容**: 次の 3 段階に分け、各段階の AC を満たしてから次へ進む。
  - **(a) `--token-file` の追加のみ → T1.5 に移動済み**(T0.4 のウォッチドッグが必要とするため P1 で出荷する)。ここでは前提として扱う。
  - **(b) 再起動ターゲットの書き換え**: `clipwire-rebuild-restart`(EncodedCommand をデコードして編集)を次の形に書き換えて `register` し、実機で再起動が成功することを確認する。(1) **先にビルドし、成功したら停止して起動する**(`git pull; cargo build` → 成功時のみ `Stop-Process` → `Start-Process`。停止から起動までを数秒にする。ウォッチドッグとの競合を避けるため。RH1)。(2) 開始時に `%LOCALAPPDATA%\clipwire\maintenance`(有効期限つき)を作り、終了時に消す(ウォッチドッグはこの間は動かない。T0.4)。(3) 停止対象はプロセス名 `clipwire` ではなく、**`serve` が起動時に書く PID ファイル**(または `clipwire*` のワイルドカード)で決める(good.exe で復旧後にプロセス名が変わっても止められるように)。(4) `serve --auto-approve --token-file <path>` を使う。(5) **この時点でトークンを新しいものにローテーションする**(既存のトークンは `get` の curl 行により会話ログに出ている可能性が高い。T1.4 で出力は止まるが、過去分は残る)。手順は `dev-loop.md`。(6) 永続的なユーザー環境変数 `CLIPD_TOKEN`(レジストリ)が残っていれば削除する(RL2)。
    - コード側は完了: `serve` が設定ディレクトリの `clipwire.pid` を起動時に上書きし、正常終了時に削除する。再起動ターゲットの書き換えと実機確認は未実施。
  - **(c) `remove_var` の出荷**: サーバ起動後に自プロセスの環境から `CLIPD_TOKEN` を除去(`main` の最初、スレッド生成前。edition 2021 の間は `unsafe` 不要だが、2024 移行時は `unsafe`)。子への `env_remove`(T4.5)は、(c) の後にのみ出荷する。
    - **出荷禁止**: (b) の AC-T2.4.5 / 2.4.8〜2.4.10 を実機で確認するまで実装しない。
- **AC**:
  - AC-T2.4.3 (L): (c) サーバ起動後、自プロセスの環境に `CLIPD_TOKEN` が存在しない(テスト用の内部関数で確認)。
  - AC-T2.4.5 (X): **(b) の成功が (c) の配備条件**: 書き換えた `clipwire-rebuild-restart` を、**環境変数 `CLIPD_TOKEN` なしで**(token-file のみで)実行して、新サーバが起動し `/health` が 200。`evidence/T2.4.md` に記録。
  - AC-T2.4.6 (X): (c) 出荷後に `clipwire-rebuild-restart` を実行して再起動が成功する(リモートのまま完結)。
  - AC-T2.4.7 (W): (c) 出荷前に、T0.4 のウォッチドッグが稼働していることを確認した記録がある。
  - AC-T2.4.8 (W): (b) の再起動ターゲットで、**ウォッチドッグの稼働中に**実行して(ビルドに 3 分かかる状況でも)、終了後に新版(`/health` の `version`)が稼働している(ウォッチドッグが good.exe を割り込ませない)。
  - AC-T2.4.9 (W): good.exe で復旧した**後**に `clipwire-rebuild-restart` を実行すると、新版に置き換わる(プロセス名に依存せず止められる)。
  - AC-T2.4.10 (X): トークンをローテーションした後、旧トークンで `exec` が 401 になり、新トークンで通る。永続的なユーザー環境変数 `CLIPD_TOKEN` が存在しない(`[Environment]::GetEnvironmentVariable('CLIPD_TOKEN','User')` が空)。
- **リスク**: `remove_var` は他スレッドが `getenv` していると UB になりうる。→ `main` の最初(スレッド生成前)で実行する。復旧不能リスクは、上記の段階分けとウォッチドッグで軽減。

## T2.5 状態を変える GET の棚卸し

- **ADR**: 0011 §B-11
- **内容**: `GET /open` が状態を変える(ブラウザを開く)ことを記録し、POST 移行を別タスクとして起票(本フェーズでは変更しない)。
- **AC**:
  - AC-T2.5.1 (L): `docs/adr/0011` の該当節に、現状(GET /open)が「既知の許容事項」として記載されている。
- **リスク**: なし。

## T2.6 フェーズ 2 の出荷

- **AC**:
  - AC-T2.6.1 (X): トークン付きの既存運用(`clipwire get/put/exec/register`)が従来どおり動く。
  - AC-T2.6.2 (X): 旧クライアント(P1 時点のビルド)でも、トークン付きなら動く。
  - AC-T2.6.3 (X): `--bind-localhost-only` だけでトークンなしの環境で、`exec` が 403 になり、エラーメッセージが `--token-file` と `--allow-no-token` を案内する。
