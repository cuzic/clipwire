# Phase 9: ファイル送信と自己紹介(ADR-0012)

依頼元: 別セッション(2026-11-27。PPTX を Downloads に置くために使い捨てターゲット `save-clip-file-to-downloads` を作った件)。各タスクは独立に出荷できるが、T9.1 は T9.3(表から作る一覧)と同じコミットでよい。

## T9.1 `POST /files` と `clipwire send`(ADR-0012 §1・§2・§7)

- **内容**: 生バイトの受信、保存先の固定(`--files-dir`、既定 Downloads)、ファイル名の検証、`(n)` の付与、上限(`--files-max-bytes`、既定 100 MiB)、ストリーム書き込み、MotW(`--files-no-motw`)、toast、監査 `file-receive`、`?clip=1` の CF_HDROP。ルートクラス `Upload`(保護ルートと同じ認証 + `application/octet-stream` 必須)。クライアント `clipwire send FILE [--name] [--clip]`。
- **AC**:
  - AC-T9.1.1 (L): ファイル名の検証が、ADR-0012 §1 の各規則を表駆動テストで満たす(`..`、`\`、`/`、`:`、制御文字、先頭 `.`、予約名、末尾の `.`・空白、双方向制御文字、`.lnk` 等、長さ)。
  - AC-T9.1.2 (L): 同名があると `name (1).ext`、拡張子なし・ドット始まりでない複数ドットも正しく番号が付く。
  - AC-T9.1.3 (L): `POST /files` で保存され、応答の `bytes`・`sha256` が内容と一致し、監査に `file-receive` が残る。
  - AC-T9.1.4 (L): 上限超過は 413 で、ファイルが残らない(`Content-Length` あり・なしの両方)。
  - AC-T9.1.5 (L): トークン設定時は 401、トークン未設定かつ `--allow-no-token` なしは 403、`application/json` などは 415、Origin 付きは 403。
  - AC-T9.1.6 (L): DROPFILES の組み立て(純粋関数)が、ヘッダ 20 バイト・`fWide=1`・UTF-16 の二重 NUL 終端になる。
  - AC-T9.1.7 (L): `clipwire send` がモックサーバへ生バイトと `name` を送り、保存先を表示する。
  - AC-T9.1.8 (W): 実機で、PPTX が Downloads に保存され、toast が出て、`Zone.Identifier` が付き、`--clip` で Explorer に貼り付けられる。
- **リスク**: 低〜中。Windows 固有部分(Known Folder、ADS、CF_HDROP)は W で確認する。ロールバックはルートの削除のみ(ストアに影響しない)。

## T9.2 `GET /targets` と `DELETE /register/:name`、`clipwire unregister`(ADR-0012 §4・§5)

- **AC**:
  - AC-T9.2.1 (L): `GET /targets` が registered と pending の名前・状態・ハッシュを返し、定義の本文(`script` 等)を含まない。
  - AC-T9.2.2 (L): `DELETE /register/x` が registered と pending の両方から消し、監査に `unregister` が残る。無ければ 404。不正な名前は 400。
  - AC-T9.2.3 (L): `RegisteredMutation` の固定一覧に `DELETE /register/:name`(削除のみ)が加わる(T5.6 の回帰テストの更新)。
  - AC-T9.2.4 (X): 実機で `clipwire unregister save-clip-file-to-downloads` が成功し、`exec` が「未登録」になる。
- **リスク**: 低。

## T9.3 `GET /info` と 404 の一覧(ADR-0012 §3・§6)、`clipwire info`

- **AC**:
  - AC-T9.3.1 (L): `GET /info` が認証なしで `app=clipwire`・`version`・`features`・`endpoints` を返し、`endpoints` が `ROUTES` の全件と一致する。
  - AC-T9.3.2 (L): 存在しないパスが 404 で、本文に全エンドポイントと `/info` の案内を含む。Origin 付きなら 403 のまま。
- **リスク**: 低。
