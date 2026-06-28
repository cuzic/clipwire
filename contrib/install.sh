#!/usr/bin/env bash
# clipwire contrib インストーラ
# Linux/Mac 側のクライアントツールと Claude Code スキルをセットアップする
#
# 使い方:
#   bash contrib/install.sh            # 全てインストール
#   bash contrib/install.sh --dry-run  # 何をするか確認だけ
#   bash contrib/install.sh --skills   # スキルのみ
#   bash contrib/install.sh --binary   # clipwire バイナリのみ
#   bash contrib/install.sh --tools    # claude-copy + スキル (バイナリなし)

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTRIB="$REPO_DIR/contrib"
BIN_DIR="${CLIPWIRE_BIN_DIR:-$HOME/bin}"
CLAUDE_COMMANDS_DIR="${CLAUDE_COMMANDS_DIR:-$HOME/.claude/commands}"

DRY_RUN=0
INSTALL_BINARY=1
INSTALL_TOOLS=1
INSTALL_SKILLS=1

for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=1 ;;
        --binary)  INSTALL_TOOLS=0; INSTALL_SKILLS=0 ;;
        --tools)   INSTALL_BINARY=0; INSTALL_SKILLS=0 ;;
        --skills)  INSTALL_BINARY=0; INSTALL_TOOLS=0 ;;
    esac
done

run() {
    if [[ "$DRY_RUN" -eq 1 ]]; then
        echo "  [dry-run] $*"
    else
        "$@"
    fi
}

echo "=== clipwire contrib installer ==="
echo "repo:    $REPO_DIR"
echo "bin:     $BIN_DIR"
echo "skills:  $CLAUDE_COMMANDS_DIR"
[[ "$DRY_RUN" -eq 1 ]] && echo "(dry-run mode)"
echo ""

# ── clipwire バイナリ ─────────────────────────────────────────────────────────

if [[ "$INSTALL_BINARY" -eq 1 ]]; then
    echo "--- clipwire binary → $BIN_DIR ---"
    run mkdir -p "$BIN_DIR"

    binary="$REPO_DIR/target/release/clipwire"
    if [[ ! -f "$binary" ]]; then
        echo "  ビルド済みバイナリが見つかりません。先にビルドしてください:"
        echo "    cargo build --release"
        echo "  skip"
    else
        run cp "$binary" "$BIN_DIR/clipwire"
        run chmod +x "$BIN_DIR/clipwire"
        echo "  installed: $BIN_DIR/clipwire"
    fi
    echo ""
fi

# ── その他ツール (claude-copy) ────────────────────────────────────────────────

if [[ "$INSTALL_TOOLS" -eq 1 ]]; then
    echo "--- Tools → $BIN_DIR ---"
    run mkdir -p "$BIN_DIR"

    for tool in claude-copy; do
        src="$CONTRIB/$tool"
        dst="$BIN_DIR/$tool"
        if [[ ! -f "$src" ]]; then
            echo "  skip: $src not found"
            continue
        fi
        run cp "$src" "$dst"
        run chmod +x "$dst"
        echo "  installed: $dst"
    done
    echo ""
fi

# ── Claude Code スキル ────────────────────────────────────────────────────────

if [[ "$INSTALL_SKILLS" -eq 1 ]]; then
    echo "--- Skills → $CLAUDE_COMMANDS_DIR ---"
    run mkdir -p "$CLAUDE_COMMANDS_DIR"

    skills_dir="$CONTRIB/claude-skills"
    if [[ ! -d "$skills_dir" ]]; then
        echo "  skip: $skills_dir not found"
    else
        for skill in "$skills_dir"/*.md; do
            [[ -f "$skill" ]] || continue
            dst="$CLAUDE_COMMANDS_DIR/$(basename "$skill")"
            run cp "$skill" "$dst"
            echo "  installed: $dst"
        done
    fi
    echo ""
fi

# ── PATH チェック ─────────────────────────────────────────────────────────────

if [[ "$DRY_RUN" -eq 0 ]]; then
    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$BIN_DIR"; then
        echo "⚠  $BIN_DIR は PATH に含まれていません。"
        echo "   ~/.bashrc または ~/.zshrc に以下を追加してください:"
        echo ""
        echo "   export PATH=\"\$HOME/bin:\$PATH\""
        echo ""
    fi
fi

echo "=== 完了 ==="
