# clipwire

Tailscale ネット越しに Windows / Wayland / tmux のクリップボードを双方向で繋ぐデーモン。  
現在は **Windows → Linux (読み取り)** を実装済み。Wayland 双方向対応は将来予定。

## リポジトリ構成

```
clipwire/
├── Cargo.toml          # Rust プロジェクト (Windows サーバ本体)
├── src/main.rs
└── contrib/
    ├── clipd.ps1       # 旧 PowerShell 版サーバ (参考用)
    ├── claude-copy     # Claude Code セッション内容を stdout に出力
    └── tmux.conf.snippet
```

## 動作イメージ

```
Windows (clipwire.exe)        Linux / Mac
┌───────────────────┐         ┌──────────────────────┐
│  クリップボード    │ tailnet │  tssh / ssh でログイン │
│  ↓ HTTP          │◀────────│  $ clipwire get       │
│  port 9999        │         │  → 内容が標準出力に    │
└───────────────────┘         └──────────────────────┘
```

---

## セットアップ手順

### 1. Windows 側のセットアップ

**1-1. Tailscale をインストール**

まだ入っていなければ [tailscale.com](https://tailscale.com/download/windows) からインストールしてサインイン。

**1-2. clipwire をビルド**

```powershell
# Rust が入っていなければ: https://rustup.rs/
git clone https://github.com/cuzic/clipwire.git
cd clipwire
cargo build --release
# → target\release\clipwire.exe
```

**1-3. URL 予約 (初回のみ・管理者 PowerShell で)**

```powershell
tailscale ip -4
# → 例: 100.x.y.z

netsh http add urlacl url=http://100.x.y.z:9999/ user="DESKTOP-XXXXX\username"
```

**1-4. トークンを決める**

```powershell
# 好きな英数字文字列 (例: "mySecretToken42")
```

**1-5. 自動起動の設定 (任意)**

スタートアップフォルダ (`shell:startup`) に以下の内容の `.bat` を置く:

```bat
@echo off
set CLIPD_TOKEN=mySecretToken42
start "" /B "C:\path\to\clipwire.exe" --token %CLIPD_TOKEN%
```

手動で試す場合:

```powershell
$env:CLIPD_TOKEN = "mySecretToken42"
.\clipwire.exe
```

---

### 2. Linux 側のセットアップ

**2-1. clipwire をビルド・インストール**

```bash
git clone https://github.com/cuzic/clipwire.git
cd clipwire
cargo build --release
bash contrib/install.sh --binary   # ~/bin/clipwire にコピー

# ~/bin が PATH に入っていなければ
echo 'export PATH="$HOME/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

**2-2. 環境変数を設定**

`~/.bashrc` (または `~/.zshrc`) に追記:

```bash
export CLIPD_HOST=my-windows-hostname   # Tailscale MagicDNS 名 または 100.x.y.z
export CLIPD_PORT=9999                  # 変えていなければ省略可
export CLIPD_TOKEN=mySecretToken42      # Windows 側と同じトークン
```

```bash
source ~/.bashrc
```

**2-2b. `CLIPD_HOST` を SSH ログイン時に自動設定する (任意)**

Windows から Tailscale 経由で SSH するたびに `CLIPD_HOST` を手動設定するのは手間なので、自動化できる。

---

**方法 A: `SSH_CONNECTION` を使う (sshd_config 変更不要・推奨)**

`sshd` は接続元 IP を `$SSH_CONNECTION` に自動セットするので、`~/.bashrc` に以下を追記するだけ:

```bash
# Tailscale 経由 SSH のとき CLIPD_HOST を自動セット
# SSH_CONNECTION の形式: <client_ip> <client_port> <server_ip> <server_port>
if [[ -n "$SSH_CONNECTION" ]]; then
    _ip=$(awk '{print $1}' <<< "$SSH_CONNECTION")
    [[ "$_ip" == 100.* ]] && export CLIPD_HOST="$_ip"
    unset _ip
fi
```

`tailscale ssh <hostname>` でログインすると `$SSH_CONNECTION` の先頭フィールドが接続元の Tailscale IP になるため、`CLIPD_HOST` が自動でセットされる。(`SSH_CLIENT` も同様の変数だが、環境によって unset のことがあるため `SSH_CONNECTION` を推奨。)

---

**方法 B: `SetEnv` + `AcceptEnv` を使う**

Windows 側の `.ssh/config` (`%USERPROFILE%\.ssh\config`) で接続先ごとにホスト名を明示できる。

```
Host dev
    HostName 100.100.45.36
    User cuzic
    SetEnv CLIPD_HOST=dragonflyg4
```

Linux 側の `/etc/ssh/sshd_config` に以下を追記して `sshd` を再起動:

```
AcceptEnv CLIPD_HOST CLIPD_PORT CLIPD_TOKEN
```

```bash
sudo systemctl restart sshd
```

方法 A より明示的で、複数の Windows マシンから接続先ごとに別のホスト名を指定したい場合に向く。

---

**2-3. 動作確認**

```bash
curl http://${CLIPD_HOST}:${CLIPD_PORT}/health
clip
```

---

### 3. tmux キーバインドのセットアップ (任意)

```bash
curl -o /tmp/tmux.snippet \
  https://raw.githubusercontent.com/cuzic/clipwire/main/contrib/tmux.conf.snippet
cat /tmp/tmux.snippet >> ~/.tmux.conf
tmux source ~/.tmux.conf
```

---

## Windows 側: clipwire

### パラメータ

| オプション | 既定 | 説明 |
|---|---|---|
| `--port` | `9999` | 待ち受けポート |
| `--token` | `$CLIPD_TOKEN` | Bearer トークン |
| `--bind-localhost-only` | off | localhost のみバインド |
| `--allow-no-token` | off | token なし tailnet 公開を明示許可 |

### セキュリティ

- token なし & tailnet 公開はデフォルトで拒否 (`--allow-no-token` で上書き可)
- 多重起動防止に名前付き Mutex を使用
- Tailscale IP は `tailscale ip -4` または CGNAT 帯 (`100.64.0.0/10`) で自動検出

### Tailscale IP へのバインド権限

初回起動時に `Failed to bind` が出る場合、管理者 PowerShell で:

```powershell
netsh http add urlacl url=http://<tailscale-ip>:9999/ user="DOMAIN\username"
```

### API エンドポイント

| エンドポイント | 認証 | 説明 |
|---|---|---|
| `GET /` | 要 | クリップボード自動判別 |
| `GET /clip` | 要 | 同上 |
| `GET /file?path=<encoded>` | 要 | CF_HDROP ファイルの実体 (クリップボード照合あり) |
| `GET /vfile?i=N` | 要 | 仮想ファイルの実体 (Outlook 添付等) |
| `GET /health` | 不要 | 死活確認 |

### クリップボード種別 (X-Clip-Kind)

| X-Clip-Kind | Content-Type | 発生ケース |
|---|---|---|
| `image` | `image/png` | スクリーンショット・ビットマップ |
| `files` | `application/json` | Explorer でコピーした既存ファイル |
| `vfiles` | `application/json` | Outlook 添付・SharePoint 等 (パスなし) |
| `audio` | `audio/wav` | CF_WAVE 音声 |
| `url` | `text/plain` | アドレスバー・リンクのコピー |
| `html` | `text/html` | ブラウザ・Office のリッチコピー |
| `rtf` | `text/rtf` | Word・Wordpad |
| `text` | `text/plain` | 通常のテキスト |
| `empty` | `text/plain` | 空 (Windows バルーン通知) |

---

## Linux / Mac 側: clipwire get / put

### 使い方

```bash
clipwire get              # クリップボードの内容を自動判別して出力
clipwire get -q           # パスやコマンドだけを出力 (Claude Code / シェルへのパイプ向け)
clipwire get -d ~/pics    # 画像の保存先を指定 (既定: /tmp 以下)

echo "hello" | clipwire put   # stdin を Windows クリップボードに書き込む
```

### 種別ごとの動作

| 種別 | 通常モード | quiet モード (`-q`) |
|---|---|---|
| `image` | `画像を保存しました: /tmp/tmp.XXX.png` | `/tmp/tmp.XXX.png` |
| `files` | パス + curl コマンドを表示 | curl コマンドのみ |
| `vfiles` | ファイル名 + curl コマンドを表示 | curl コマンドのみ |
| `audio` | `音声を保存しました: /tmp/tmp.XXX.wav` | `/tmp/tmp.XXX.wav` |
| `url` | URL をそのまま出力 | URL をそのまま出力 |
| `html` | 1KB 以下: そのまま / 超過: パスを出力 | 同左 |
| `rtf` | 1KB 以下: そのまま / 超過: パスを出力 | 同左 |
| `text` | 1KB 以下: そのまま / 超過: パスを出力 | 同左 |
| `empty` | サイレント (Windows 側にバルーン通知) | サイレント |

**1KB 閾値の理由:** Claude Code は入力を `history.jsonl` に記録するため、大容量コンテンツをインラインで貼り付けると履歴が肥大化する。1KB 超はファイルに保存してパスだけを渡す。

---

## Wayland クリップボードとの連携

### 必要パッケージ

```bash
sudo apt install wl-clipboard   # wl-copy / wl-paste を提供
```

### Windows ↔ Wayland

`clipwire get/put` は stdin/stdout ベースなので、`wl-copy`/`wl-paste` とパイプするだけで動く。

```bash
# Windows → Wayland
clipwire get -q | wl-copy

# Wayland → Windows
wl-paste | clipwire put
wl-paste --type image/png | clipwire put   # 画像の場合
```

tmux キーバインドとして登録しておくと便利（`contrib/tmux.conf.snippet` 参照）。

---

## Claude Code / tmux との連携

### キーバインド

| キー | 動作 |
|---|---|
| `<prefix> Ctrl-V` | `clipwire get -q` の出力を現在のペインに貼り付け |
| `<prefix> Alt-V` | ファイル系は curl コマンドをその場で bash 実行、それ以外は貼り付け |

**`<prefix> Ctrl-V` の種別ごとの動作:**
- **テキスト/URL** (1KB 以下) → そのまま入力欄に流れる
- **テキスト** (1KB 超) / **HTML** / **RTF** / **画像** / **音声** → パスが入力欄に入る → Claude Code がパスを読む
- **実ファイル / 仮想ファイル** → curl コマンド群が入力欄に入る
- **空** → 何も貼り付かない (Windows 側にバルーン通知)

---

## ライセンス

MIT
