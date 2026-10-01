# ADR-0008: `exec --copy` とクリップボード履歴

- Status: `exec --copy` = Proposed / クリップボード履歴 = **Deferred** (改訂: Opus レビュー反映)
- Date: 2026-09-30

## Context

`exec` の結果(ビルドエラーなど)を Windows のクリップボードに送って別アプリに貼る操作が手作業になっている。また `get` は現在の内容しか返さず、少し前のコピー内容を取れない。

Opus レビューで、履歴機能は実装の前提(STA スレッドの構造)と安全性の両面で重いと指摘された。

## Decision

### `exec --copy`(クライアント側のみ・他 ADR から独立)

1. `clipwire exec <name> --copy` は、受信した出力(バッファ形式でもストリーム形式でもよい)をクライアントで保持し、終了後に既存の `POST /clip` で Windows に送る。サーバ変更は不要。
2. `--copy-on-fail` で失敗時のみ送る。
3. 出力上限 1 MiB(超過時は末尾を優先して先頭を切り詰め、`[truncated]` を付ける)。
4. ANSI エスケープは除去(`--raw` で維持)。
5. **注意**: ビルドログにはトークン等が含まれうる。ADR-0009 で子プロセスの `CLIPD_TOKEN` を除去するのは事故防止であり、`--copy` がクリップボード(他アプリ・クリップボード履歴に流れうる場所)に出力を置く点はユーザーが理解した上での利用とする。

### クリップボード履歴(Deferred)

6. 実装を見送る。見送る理由:
   - `AddClipboardFormatListener` は HWND とメッセージポンプを要し、現行の `sta_loop`(`for req in rx` のブロッキング受信)では動かない。STA スレッドの作り直しが必要。
   - 変更のたびに `OpenClipboard` すると、他アプリの「クリップボードを開けません」エラーや遅延レンダリングの強制実行を誘発する。
   - 機微データ(パスワード等)を扱うため、除外フォーマット(`ExcludeClipboardContentFromMonitorProcessing`、`CanIncludeInClipboardHistory`、`Clipboard Viewer Ignore`)の尊重、メモリのみ保持、認証必須、といった条件を満たす必要がある。
7. 再検討する場合の方針(参考): 別スレッドで `GetClipboardSequenceNumber` を 500ms ごとにポーリングし、変化時のみ既存チャネルに `GetClip` を投げる(STA ループに手を入れない)。メモリのみ・テキストのみ・`--history N`(既定 0)・トークン必須(ADR-0011)・`--allow-no-token` との併用禁止。

## Consequences

- `exec --copy` は小さな変更で日常操作が楽になる。
- 履歴を見送ることで、STA スレッドの大改造とクリップボード内容の新たな露出を避けられる。

## Alternatives considered

- **Windows 標準のクリップボード履歴(Win+V)を参照**: 公開された安全な API が限られる。
- **履歴のディスク永続化**: 機微データがディスクに残る。
