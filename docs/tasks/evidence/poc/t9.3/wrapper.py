#!/usr/bin/env python3
"""T9.3 PoC: ADR-0013 の永続コンテキストラッパーの試作(本実装ではない)。

stdin から 1 行 1 JSON({"code": "..."})を読み、`code` を共有 globals で
exec し、stdout 側に 1 行 1 JSON の結果を返す。

実行中は自分自身の fd 1/2 (プロトコル用パイプ) を一時ファイルへ dup2 で
退避し、exec の間だけそこへ向け直す。dup2 はファイル記述子の「番号」が
指す先を切り替えるだけなので、Python レベルの print()/sys.stdout だけ
でなく、同じ fd を継承したサブプロセスや、fd へ直接 os.write するネイティ
ブ拡張の出力も、同じ退避先に素通りで乗る。exec が終わったら fd をプロ
トコル用パイプへ戻し、1 行だけ JSON を書く(ユーザーコードの出力が
フレーミングに混ざらないようにする)。
"""
import contextlib
import io
import json
import os
import sys
import tempfile
import traceback

_PROTOCOL_STDOUT_FD = os.dup(1)


def _drain(path):
    with open(path, "rb") as f:
        return f.read().decode("utf-8", errors="replace")


def run_turn(code, shared_globals):
    out_path = tempfile.mktemp(prefix="t93-out-")
    err_path = tempfile.mktemp(prefix="t93-err-")
    out_fd = os.open(out_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC)
    err_fd = os.open(err_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC)
    saved_stdout_fd = os.dup(1)
    saved_stderr_fd = os.dup(2)
    error = None
    try:
        sys.stdout.flush()
        sys.stderr.flush()
        os.dup2(out_fd, 1)
        os.dup2(err_fd, 2)
        try:
            exec(compile(code, "<turn>", "exec"), shared_globals)
        except Exception:
            error = traceback.format_exc()
        sys.stdout.flush()
        sys.stderr.flush()
    finally:
        os.dup2(saved_stdout_fd, 1)
        os.dup2(saved_stderr_fd, 2)
        os.close(saved_stdout_fd)
        os.close(saved_stderr_fd)
        os.close(out_fd)
        os.close(err_fd)
    stdout_text = _drain(out_path)
    stderr_text = _drain(err_path)
    os.remove(out_path)
    os.remove(err_path)
    return stdout_text, stderr_text, error


def main():
    shared_globals = {}
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        request = json.loads(line)
        stdout_text, stderr_text, error = run_turn(request["code"], shared_globals)
        response = {"stdout": stdout_text, "stderr": stderr_text, "error": error}
        payload = (json.dumps(response) + "\n").encode("utf-8")
        os.write(_PROTOCOL_STDOUT_FD, payload)


if __name__ == "__main__":
    main()
