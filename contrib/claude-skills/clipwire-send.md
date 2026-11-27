---
description: Windows クリップボードに Claude の出力・タスクリスト・会話内容を送る
keywords:
  - clipboard
  - windows
  - clipwire
  - クリップボード
  - コピー
  - タスク
triggers:
  - windows クリップボードに送る
  - クリップボードにコピー
  - windows にコピー
  - タスクをクリップボードに
---

# /clipwire-send

Claude の出力内容を Windows クリップボードに送る。

## 引数

`$ARGUMENTS` — `last`（デフォルト）/ `tasks` / `session`

## 実行手順

### 1. 引数に応じてコマンドを選択

| 引数 | 動作 |
|---|---|
| `last` または省略 | 最新の Claude レスポンスを送る |
| `tasks` | 現在のタスクリストを送る |
| `session` | 会話全体を送る |

### 2. コマンド実行

```bash
claude-copy $ARGUMENTS | clipwire put
```

`claude-copy` や `clipwire` が PATH に入っていない場合は `~/bin/` を補完する:

```bash
python3 ~/bin/claude-copy $ARGUMENTS | ~/bin/clipwire put
```

### 3. 結果報告

成功した場合: 「Windows クリップボードに送りました」と報告する。
失敗した場合: エラーメッセージを表示して原因を説明する。

## ファイルを送る場合

テキストではなくファイル(PPTX・PDF・画像など)を Windows に置きたいときは、クリップボードを経由せず `clipwire send` を使う。保存先は Windows の Downloads(サーバの `--files-dir` で変更)で、保存したフルパスが出力される。

```bash
clipwire send ./slides.pptx           # Downloads\slides.pptx(同名があれば "slides (1).pptx")
clipwire send ./slides.pptx --clip    # Explorer で貼り付けられるようクリップボードにも載せる
```

サーバが何をできるか分からないときは `clipwire info`(`GET /info`)で確認する。
