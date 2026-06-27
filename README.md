# clipd

Windows クリップボードの内容を HTTP で返す軽量サービスと、Linux 側クライアント。  
Tailscale tailnet 内での使用を前提とする。

## 構成

| ファイル | 役割 |
|---|---|
| `clipd.ps1` | Windows 側サーバ。クリップボードを HTTP で返す |
| `clip` | Linux/Mac 側クライアント。`clipd` を叩いて内容を取得する |
| `tmux.conf.snippet` | tmux キーバインド設定例 |

## 動作イメージ

```
Windows (clipd.ps1)          Linux / Mac (clip)
┌──────────────────┐          ┌──────────────────────┐
│  クリップボード   │  tailnet │  tssh / ssh でログイン│
│  ↓ HTTP         │◀─────────│  $ clip               │
│  clipd.ps1:9999  │          │  → 内容が標準出力に   │
└──────────────────┘          └──────────────────────┘
```

---

## セットアップ手順

### 1. Windows 側のセットアップ

**1-1. Tailscale をインストール**

まだ入っていなければ [tailscale.com](https://tailscale.com/download/windows) からインストールしてサインイン。

**1-2. ファイルを配置**

```powershell
# 任意のフォルダに clipd.ps1 を置く (例: C:\tools\clipd\)
```

**1-3. URL 予約 (初回のみ・管理者 PowerShell で)**

```powershell
# Tailscale IP を確認
tailscale ip -4
# → 例: 100.x.y.z

# URL 予約 (user= は実際のドメイン\ユーザー名 に合わせる)
netsh http add urlacl url=http://100.x.y.z:9999/ user="DESKTOP-XXXXX\username"
```

**1-4. トークンを決める**

```powershell
# 好きな文字列でよい (英数字推奨)
# 例: "mySecretToken42"
```

**1-5. 自動起動の設定 (任意)**

スタートアップフォルダ (`shell:startup`) に以下の内容の `.bat` ファイルを置く:

```bat
@echo off
set CLIPD_TOKEN=mySecretToken42
powershell -WindowStyle Hidden -ExecutionPolicy Bypass -File C:\tools\clipd\clipd.ps1
```

または、PowerShell 単体で試す場合:

```powershell
$env:CLIPD_TOKEN = "mySecretToken42"
powershell -ExecutionPolicy Bypass -File C:\tools\clipd\clipd.ps1
```

---

### 2. Linux 側のセットアップ

**2-1. clip をインストール**

```bash
mkdir -p ~/bin
curl -o ~/bin/clip https://raw.githubusercontent.com/cuzic/powershell-clipd/main/clip
chmod +x ~/bin/clip

# ~/bin が PATH に入っていなければ追加
echo 'export PATH="$HOME/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

**2-2. 環境変数を設定**

`~/.bashrc` (または `~/.zshrc`) に追記:

```bash
export CLIPD_HOST=my-windows-hostname   # Tailscale の MagicDNS 名 または 100.x.y.z
export CLIPD_PORT=9999                  # 変えていなければ省略可
export CLIPD_TOKEN=mySecretToken42      # Windows 側と同じトークン
```

```bash
source ~/.bashrc
```

**2-3. 動作確認**

```bash
# health チェック (token 不要)
curl http://${CLIPD_HOST}:${CLIPD_PORT}/health

# クリップボード取得テスト (Windows 側で何かコピーしてから)
clip
```

---

### 3. tmux キーバインドのセットアップ (任意)

```bash
# リポジトリを clone するか、スニペットだけダウンロード
curl -o /tmp/tmux.conf.snippet \
  https://raw.githubusercontent.com/cuzic/powershell-clipd/main/tmux.conf.snippet

# ~/.tmux.conf に追記
cat /tmp/tmux.conf.snippet >> ~/.tmux.conf

# 現在のセッションに反映
tmux source ~/.tmux.conf
```

---

## Windows 側: clipd.ps1

### 起動方法

```powershell
# tailnet に出す + Bearer token 認証 (推奨)
powershell -ExecutionPolicy Bypass -File clipd.ps1 -Token "好きな文字列"

# tailnet に出す + token なし (明示的に許可)
powershell -ExecutionPolicy Bypass -File clipd.ps1 -AllowNoToken

# localhost だけで使う (同じ Windows 上の tmux 等)
powershell -ExecutionPolicy Bypass -File clipd.ps1 -BindLocalhostOnly
```

環境変数でもトークンを渡せる:

```powershell
$env:CLIPD_TOKEN = "好きな文字列"
powershell -ExecutionPolicy Bypass -File clipd.ps1
```

### パラメータ

| パラメータ | 既定 | 説明 |
|---|---|---|
| `-Port` | `9999` | 待ち受けポート |
| `-Token` | `$env:CLIPD_TOKEN` | Bearer トークン (未指定で認証なし) |
| `-BindLocalhostOnly` | off | localhost のみバインド |
| `-AllowNoToken` | off | token なし tailnet 公開を明示許可 |

### セキュリティ

- token なし & tailnet 公開はデフォルトで拒否 (`-AllowNoToken` で上書き可)
- 多重起動防止に名前付き Mutex を使用
- Tailscale IP は `tailscale ip -4` または CGNAT 帯 (`100.64.0.0/10`) で自動検出

### Tailscale IP へのバインド権限

初回起動時に `Failed to start HttpListener` が出る場合、管理者 PowerShell で:

```powershell
netsh http add urlacl url=http://<tailscale-ip>:9999/ user="DOMAIN\username"
```

### API エンドポイント

| エンドポイント | 認証 | 説明 |
|---|---|---|
| `GET /` | 要 | クリップボード自動判別 |
| `GET /clip` | 要 | 同上 |
| `GET /file?path=<encoded>` | 要 | CF_HDROP ファイルの実体 (クリップボード照合あり) |
| `GET /vfile?i=N` | 要 | 仮想ファイルの実体 (Outlook 添付等、インデックス指定) |
| `GET /health` | 不要 | 死活確認 |

### クリップボード種別 (X-Clip-Kind)

検出は以下の優先順で行う:

| X-Clip-Kind | Content-Type | 発生ケース | `/clip` の返却内容 |
|---|---|---|---|
| `image` | `image/png` | スクリーンショット・ビットマップ | PNG バイナリ |
| `files` | `application/json` | Explorer でコピーした既存ファイル | Windows パスの配列 |
| `vfiles` | `application/json` | Outlook 添付・SharePoint 等 (パスなし) | ファイル名の配列 |
| `audio` | `audio/wav` | CF_WAVE 音声 | WAV バイナリ |
| `url` | `text/plain` | アドレスバー・リンクのコピー | URL 文字列 |
| `html` | `text/html` | ブラウザ・Office のリッチコピー | HTML (Windows ヘッダ除去済み) |
| `rtf` | `text/rtf` | Word・Wordpad | RTF 文字列 |
| `text` | `text/plain` | 通常のテキスト | テキスト文字列 |
| `empty` | `text/plain` | 空 | 空文字列 + Windows バルーン通知 |

`/file?path=` はクリップボードの FileDropList に存在するパスのみ許可 (任意パスは 403)。  
`/vfile?i=N` は仮想ファイルをインデックスで指定して取得する。

---

## Linux 側: clip

### インストール

```bash
curl -o ~/bin/clip https://raw.githubusercontent.com/cuzic/powershell-clipd/main/clip
chmod +x ~/bin/clip
```

### 設定

```bash
export CLIPD_HOST=my-windows   # Tailscale MagicDNS 名 or 100.x.y.z
export CLIPD_PORT=9999         # 省略可 (既定 9999)
export CLIPD_TOKEN=secret      # clipd を -Token 付きで起動した場合のみ
```

`.bashrc` / `.zshrc` に書いておくと便利。

### 使い方

```bash
clip              # クリップボードの内容を自動判別して出力
clip -q           # パスやコマンドだけを出力 (Claude Code / シェルへのパイプ向け)
clip -d ~/pics    # 画像の保存先を指定 (既定: /tmp 以下に mktemp)
clip -h           # ヘルプ
```

### 種別ごとの動作

| 種別 | 通常モード | quiet モード (`-q`) |
|---|---|---|
| `image` | `画像を保存しました: /tmp/tmp.XXX.png` | `/tmp/tmp.XXX.png` |
| `files` | パス + curl コマンドを表示 | curl コマンドのみ |
| `vfiles` | ファイル名 + curl コマンドを表示 | curl コマンドのみ |
| `audio` | `音声を保存しました: /tmp/tmp.XXX.wav` | `/tmp/tmp.XXX.wav` |
| `url` | URL をそのまま出力 | URL をそのまま出力 |
| `html` | 1KB 以下: HTML をそのまま出力 / 超過: パスを出力 | 同左 |
| `rtf` | 1KB 以下: RTF をそのまま出力 / 超過: パスを出力 | 同左 |
| `text` | 1KB 以下: テキストをそのまま出力 / 超過: パスを出力 | 同左 |
| `empty` | サイレント (Windows 側にバルーン通知) | サイレント |

**1KB 閾値の理由:** Claude Code は入力をそのまま `history.jsonl` に記録するため、
大容量コンテンツをインラインで貼り付けると履歴が肥大化する。
1KB 超はファイルに保存してパスだけを渡し、Claude Code が必要に応じて読む。

### 出力例

```bash
# テキスト (1KB 以下)
$ clip
コピーしたテキスト

# テキスト (1KB 超)
$ clip
テキストを保存しました (大容量): /tmp/tmp.aB3xYz.txt

# 画像
$ clip
画像を保存しました: /tmp/tmp.aB3xYz.png

$ clip -q
/tmp/tmp.aB3xYz.png

# 実ファイル (Explorer でコピー)
$ clip
クリップボード: ファイル 2件

  C:\Users\user\Desktop\foo.txt
  → curl -fsSL 'http://my-windows:9999/file?path=C%3A%5CUsers%5Cuser%5CDesktop%5Cfoo.txt' -o 'foo.txt'

  C:\Users\user\Desktop\bar.png
  → curl -fsSL 'http://my-windows:9999/file?path=...' -o 'bar.png'

$ clip -q
curl -fsSL 'http://my-windows:9999/file?path=C%3A%5C...' -o 'foo.txt'
curl -fsSL 'http://my-windows:9999/file?path=...' -o 'bar.png'

# 仮想ファイル (Outlook 添付等)
$ clip
クリップボード: 仮想ファイル 1件 (Outlook 添付等)

  [0] report.pdf
  → curl -fsSL 'http://my-windows:9999/vfile?i=0' -o 'report.pdf'

# URL
$ clip
https://example.com/

# 空
$ clip
(何も出力しない。Windows 側にバルーン通知が表示される)
```

---

## Claude Code / tmux との連携

Claude Code のキーバインドシステムはシェルコマンドを直接実行する機能を持たないため、
tmux 経由でペインに貼り付けるのが最もシンプルな方法。

### セットアップ

```bash
cat tmux.conf.snippet >> ~/.tmux.conf
tmux source ~/.tmux.conf
```

### キーバインド

| キー | 動作 |
|---|---|
| `<prefix> Ctrl-V` | `clip -q` の出力を現在のペイン (Claude Code 入力欄等) に貼り付け |
| `<prefix> Alt-V` | ファイル系のみ curl コマンドをその場で bash 実行してダウンロード、それ以外は貼り付け |

**`<prefix> Ctrl-V` の種別ごとの動作:**
- **テキスト/URL** (1KB 以下) → そのまま入力欄に流れる
- **テキスト** (1KB 超) / **HTML** / **RTF** / **画像** / **音声** → `/tmp/tmp.XXX.*` のパスが入力欄に入る → Claude Code がパスを読む
- **実ファイル / 仮想ファイル** → curl コマンド群が入力欄に入る → Claude Code が必要なものを実行
- **空** → 何も貼り付かない (Windows 側にバルーン通知)

---

## ライセンス

MIT
