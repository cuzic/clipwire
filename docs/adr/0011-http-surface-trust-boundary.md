# ADR-0011: HTTP 面の信頼境界と即時修正

- Status: Proposed
- Date: 2026-09-30
- Revised: 2026-09-30 — §5・§6 を改訂。`--allow-no-token` を**明示した場合に限り**、トークンなしでの保護ルートと `--auto-approve` を許可する(当初案は常に拒否)。判断はユーザーによるもの。理由と残るリスクは §5・§6・Consequences に記す。
- Note: 本 ADR の「即時修正」は、他の ADR の実装を待たず**最初に**行う(既存コードの脆弱性)。

## Context

Opus レビューと、`src/main.rs` の確認で次を確認した。

1. **`name` 経由のコード注入(既存バグ)**: `register` の `name` がトースト XML(`show_register_toast_impl`)に未エスケープで埋め込まれる。XML のロードに失敗すると `show_balloon` → `show_simple_toast` にフォールバックし、`msg.replace('\'', "")` だけを PowerShell の `-Command` 内 `'…'` に埋め込んで実行する。PowerShell は Unicode の引用符(U+2018〜U+201B)も一重引用符として扱うため、`name` を細工すると**承認前に**任意の PowerShell が実行されうる。ADR-0007 の `notify` が同じ関数を使う設計だと、同じ経路が広がる。
2. **トークンなしの面**: `--bind-localhost-only` ならトークンなしで起動できる。`handle_exec` / `handle_register` は `Content-Type` も `Origin` も検査しない。ブラウザの悪意あるページが `fetch("http://127.0.0.1:9999/exec", {method:"POST", body:'{"name":"..."}'})` を `text/plain` の単純リクエスト(プリフライトなし)で送ると、登録済みターゲットが実行されうる。DNS rebinding では読み取りも可能。`--allow-no-token` では tailnet 上の任意ノードが同様に実行でき、`--auto-approve` と併用すると承認なしで任意コードを登録・実行できる。
3. トークン比較(`check_auth`)が定数時間でない(tailnet 内での実害は小さいが、1行で直る)。

## Decision

### A. 即時修正(ADR 不要のバグ修正として先行)

1. `register` の `name` は `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$` で検証し、違反は 400 を返す(先頭が `.` や `-` のもの、`..` を除く。名前をパスに使う箇所があっても安全にするため)。`exec` も同様。既存の registered に違反する名前があれば、起動時に警告する。
2. トースト XML の全埋め込み値は、XML エスケープする(または `XmlDocument` の DOM API でテキストノードを作る)。ADR-0006 が追加する表示項目(dir、ハッシュ等)も同様。
3. `show_simple_toast` の PowerShell 経路は**廃止**し、WinRT の `ToastNotification` に一本化する。`show_balloon` と ADR-0007 の `notify` は WinRT を使う。トースト表示に失敗した場合のフォールバックは、`toast.log` への記録と `tracing::error!` のみとする(外部プロセスを起動しない)。
4. `check_auth` のトークン比較を定数時間比較にする。

### B. HTTP 面の信頼境界

5. **保護ルートの認証**: `/exec`, `/register`, `/jobs*`, `/targets*`, `/audit`(存在するなら)は、次のとおりとする。クリップボード系(`/clip`, `/file`, `/vfile`, `/open`)は、従来の起動条件を維持しつつ、後述 7・8 を適用する。
   - **トークンが設定されている**: `Authorization: Bearer` が必須(欠落・誤りは 401)。
   - **トークンが未設定で、`--allow-no-token` が指定されている**: **認証なしで受け付ける**。`--allow-no-token` は「この面を認証なしで公開する」ことへの明示的なオプトインであり、`--bind-localhost-only` との併用も可(到達できる範囲が localhost に限られるだけ)。
   - **トークンが未設定で、`--allow-no-token` もない**(`--bind-localhost-only` だけの場合を含む): **403**(本文に理由と対処: `--token-file` の指定、または `--allow-no-token`)。`--bind-localhost-only` だけでトークンなしの実行を許さないのは、オプトインの意思表示なしに、同一マシン上の別プロセスやブラウザ経由で実行できる状態にならないようにするため。
   - 認証なしで受け付けるときも、7〜9(Origin・Host・Content-Type)は**必ず**適用する。認証がない分、ブラウザ経由(CSRF・DNS rebinding)を塞ぐ最後の層になる。
6. **`--auto-approve` とトークン**: `--auto-approve` は、トークンがあるか、`--allow-no-token` が明示されているときだけ起動できる(どちらもなければ起動時エラー。エラー文に両方の選択肢を書く)。
   - 起動時に**警告ログ**と監査イベント(`serve-start auto_approve=true auth=token|none`)を残す。`auth=none`(`--auto-approve --allow-no-token`)のときの警告は、「**到達できるすべての者が、Windows ユーザー権限で任意のコードを承認なしで登録・実行できる**」こと(`--bind-localhost-only` でなければ tailnet 上の全ノード)を明示し、通常の警告より強い文言にする。トークンの値は出さない。
   - toast は出さない(自動承認は実運用の正式なモードで、再起動ターゲットによる再ビルドのたびに toast が出るのを避けるため)。
   - **`auth=none` のときの信頼境界はネットワーク到達性だけ**になる。Tailscale ACL で接続元を絞ることを、運用上の前提とする(T0.4)。
7. **`Origin` ヘッダ付きのリクエストは拒否**する(全ルート)。ブラウザ経由のリクエストを排除し、CLI(`ureq`)は `Origin` を付けないため影響しない。
8. **`Host` ヘッダの許可リスト**(DNS rebinding 対策)。比較はポートを除いたホスト名部分で、大文字小文字を区別しない。許可対象: `127.0.0.1` / `localhost` / サーバの Tailscale IP / 短いホスト名(`GetComputerNameExW` の値。現在の利用実態 `CLIPD_HOST=<短い名前>` のため必須)/ MagicDNS の FQDN(`tailscale status --json` の `Self.DNSName`、末尾の `.` を除く)/ `--allow-host` で追加した名前。それ以外、および `Host` ヘッダがない場合は 421。
   - 許可リストはリクエストごと(またはキャッシュ付き)に `find_tailscale_ip` で求める。Tailscale IP が変わった場合の bind のやり直しは、既存の `serve_forever` の課題として別途扱う。
   - **Host / Origin の検査は DNS rebinding・CSRF の対策であって、認可には使わない**(`Host` は接続元の証明にならず、偽装できる)。承認の可否は HTTP ではなくローカルのファイル操作で決まる(ADR-0010 §4)。
9. すべての `POST` は `Content-Type: application/json` を必須にする(`/clip` の POST はクリップ内容の型を示す既存ヘッダと区別して、`/exec` と `/register` と `/jobs/*` に適用)。
10. `--token` のコマンドライン渡しは非推奨とし、`--token-file <path>` を追加する(`CLIPD_TOKEN` 環境変数も可)。**トークンの読み込みと、読み込み後の自プロセスの `remove_var("CLIPD_TOKEN")` は本 ADR が所有する**。子プロセスの `env_remove` は ADR-0009 が持つ。
11. **状態を変える操作は POST に限る**(原則)。`Origin` の拒否は `<img src>` のような no-cors の GET には効かない(Origin が付かない)ため。現状の `GET /open` はブラウザを開く状態変更 GET だが、互換性を維持するため当面残す**既知の許容事項**とする。POST への移行は T7.7 で行う。
12. `--auto-approve`(§6)を使った場合も、承認レコード(ADR-0010 §3)と、監査の `approve` イベント(承認者 = `auto`)を必ず書く。

## Consequences

- 承認前の任意コード実行経路(1)と、ブラウザ経由の実行(2)を塞げる。
- `--bind-localhost-only` だけでトークンなしのまま `exec` を使っていた利用者は、トークン設定か `--allow-no-token` の明示が必要になる(意図した破壊的変更。ADR の実装時にリリースノートに記載)。
- **`--allow-no-token` を選ぶと、§2 の「`--allow-no-token` + `--auto-approve` で承認なしに任意コードを登録・実行できる」状態を、利用者が承知のうえで受け入れることになる**。tailnet 上の任意のノード(侵害された端末・共有ノードを含む)が、Windows ユーザー権限でコードを実行できる。Origin・Host・Content-Type の検査は、ネットワーク越しの直接のリクエストには効かない(ブラウザ経由の攻撃だけを防ぐ)。受け入れる前提は、tailnet のノードがすべて信頼でき、Tailscale ACL で接続元が絞られていること。この前提が崩れるなら、トークンを使う。
- `Host` 許可リストの運用(Tailscale IP の変化、MagicDNS 名)が増える。自動検出(既存の `find_tailscale_ip`)+ 追加許可オプション(`--allow-host`)で吸収する。
- ADR-0006 の承認 UI とは独立して、承認の前提となる「リクエストが正当なクライアントから来ていること」を保証する。

## Alternatives considered

- **トークン未設定なら常に 403(当初案)**: 自動承認の運用と相性が悪く(再起動ターゲットなどの既存運用でトークンを配る手間が増える)、利用者が明示的に拒否したため採らない。代わりに、オプトインの明示と警告・監査で責任の所在を残す。
- **`--bind-localhost-only` だけでトークンなしを許す(従来どおり)**: オプトインの意思表示なしに同一マシンの別プロセス・ブラウザから実行できる状態が残るため採らない。localhost でトークンなしを使いたい場合は `--allow-no-token` を併用する。
- **`name` をエスケープするだけで PowerShell 経路を残す**: 引用符の取り扱いに穴が残り続ける(Unicode 引用符等)。外部プロセスへ文字列を渡す経路自体を無くす。
- **CORS ヘッダで制御**: 単純リクエストはサーバに届いて実行されてしまうため、サーバ側で `Origin` を拒否する必要がある。
