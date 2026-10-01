# ADR-0001: `/exec` の出力ストリーミング

- Status: Proposed (改訂2: Opus 再レビュー反映)
- Date: 2026-09-30
- Depends on: ADR-0009(ジョブログが出力の正本。リレーがパイプを常に読みファイルへ書く)

## Context

`handle_exec` は全ステップ完了まで待ち、出力をバッファして最後に一括返却する(`X-Exit-Code`)。長いジョブの途中経過が見えず、`awase-build-bg` / `awase-build-log` のような回避策がある。

当初案(子のパイプ2本を読んでチャネルで配信)は、クライアント切断で読み取りが止まり子がパイプ満杯で固まる点と、stdout/stderr の順序が保てない点が問題だったため、ADR-0009 で**ログファイルを正本**とし、ストリームはその follow とする方式に改めた。

## Decision

1. `POST /exec` は、クライアントが `Accept: application/x-ndjson` を送ったとき、ジョブログ(ADR-0009)を follow してチャンク転送で返す。送らない場合は、終了後にログを読んで従来形式(`text/plain` + `X-Exit-Code`)で返す(旧クライアント互換)。
2. NDJSON イベント:
   - `{"t":"out","d":"<UTF-8 lossy>"}` — ログの追記分(バイトオフセットで follow するため、UTF-8 の多バイト文字の境界で切れた末尾の不完全なバイト列は、次回に持ち越して文字化けを防ぐ)
   - `{"t":"ping"}` — 30 秒ごと(無出力時の切断誤検知防止)
   - `{"t":"exit","code":N}` — 必ず最後に 1 回
   - `{"t":"err","msg":"..."}` — サーバ側エラー
3. 終了コードは in-band の `exit` イベントで運ぶ(HTTP trailer は ureq 2 で読めないため使わない)。`exit` なしで接続が切れた場合、クライアントは「接続断」として非 0 で終了する。ジョブ自体は継続しており、ジョブ ID をエラーメッセージに含めて `clipwire logs <id>`(ADR-0002)への導線を示す。
4. stdout/stderr の区別はしない(同一ハンドルに書かれるため順序を保つ代わりに区別は失われる。必要になったら別 ADR)。非 UTF-8 出力は lossy 変換。
5. **クライアント**(`cmd_exec`):
   - 現在の `.timeout(600s)` は ureq 2 ではボディ読み取りを含む**全体**の期限なので廃止する。`timeout_connect`(10s)+ `timeout_read`(120s: サーバの ping 間隔の 4 倍)に置き換え、全体の期限は設けない。
   - 既定は NDJSON を要求する。応答の `Content-Type` を見て、旧サーバ(`text/plain`)ならバッファ形式として読む(`into_reader()` で読み、`into_string()` の 10 MB 上限を避ける)。`--no-stream` で強制的に旧形式を要求。
   - **バッファ形式(`--no-stream` と旧サーバへのフォールバック)では、終了までバイトが 1 つも来ないため、`timeout_read` を「サーバ側 timeout + 余裕」(既定 31 分)にする**。120 秒の read timeout はストリーム形式(ping あり)専用。
   - サーバの feature 確認(ADR-0010)で `stream` 未対応なら、最初から旧形式。
6. クライアント切断時、ジョブは継続する(ADR-0002 §9)。

## Consequences

- 長いジョブの進捗がリアルタイムに見え、`*-bg` / `*-log` 系ターゲットが不要になる。
- ストリームはログの follow なので、`logs -f`(ADR-0002)と同じ実装を共有し、重複設計が無い。
- stderr と stdout の区別を失う。従来は「stderr → stdout」の固定順に結合していたため、結合順序が変わる(本来の発生順になる)。
- クライアントの全体タイムアウトが無くなるため、サーバ側の timeout(ADR-0003)が唯一の上限になる。

## Alternatives considered

- **パイプ読み取り + 有界チャネル**: 切断時デッドロック・順序不定。ADR-0009 で却下。
- **HTTP trailer / SSE / WebSocket**: ureq 2 で扱えない、または過剰。
