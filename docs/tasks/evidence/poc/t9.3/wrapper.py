#!/usr/bin/env python3
"""T9.3 PoC: ADR-0013 の永続コンテキストラッパーの試作(本実装ではない)。

stdin から 1 行 1 JSON({"code": "..."})を読み、`code` を共有 globals で
exec し、専用の fd(_PROTOCOL_STDOUT_FD)に 1 行 1 JSON の結果を返す。

起動直後に fd 1 を**恒久的に**ログ側(fd 2 と同じ送信先)へ向け直す。
ターン実行中だけ、fd 1/2 を一時ファイルへ dup2 で退避し、exec の間だけ
そこへ向け直す。dup2 はファイル記述子の「番号」が指す先を切り替える
だけなので、Python レベルの print()/sys.stdout だけでなく、同じ fd を
継承したサブプロセスや、fd へ直接 os.write するネイティブ拡張の出力も、
同じ退避先に素通りで乗る。exec が終わったら fd をログ側(プロトコル用
パイプではない)に戻す。これにより、ターン中に起動されたバックグラウ
ンドスレッドやサブプロセスが、ターンが終わった**後**に fd 1 へ書いても、
プロトコルの改行区切り JSON フレーミングを壊さない(レビューで判明した
不備の修正: 当初の実装は `finally` で fd 1 をプロトコル用パイプへ戻して
いたため、ターン外の fd 1 書き込みがそのままプロトコルに混入した)。

stdin の読み取りは専用スレッドで行い、EOF を検知したら、メインスレッドが
ターンの実行中でハングしていても構わず `os._exit()` で即座に終了する。
サーバプロセスが終了すれば stdin は EOF になる(ADR-0013 §3)。ターン実行
スレッドだけが stdin を読む設計だと、ターンがハング中(pywinauto のモー
ダルダイアログ待ち等)は EOF に気づけず、サーバ再起動後も `JobRegistry`
の「席」を握ったまま残ってしまう。
"""
import contextlib
import io
import json
import os
import queue
import sys
import tempfile
import threading
import traceback

_PROTOCOL_STDOUT_FD = os.dup(1)
# fd 1 を恒久的にログ側(fd 2 の送信先)へ向け直す。これ以降、ターンの
# 外で fd 1 に書かれたものは「ログ」であり、二度とプロトコル用パイプには
# 流れない。
os.dup2(2, 1)


def _drain(path):
    with open(path, "rb") as f:
        return f.read().decode("utf-8", errors="replace")


def run_turn(code, shared_globals):
    out_path = tempfile.mktemp(prefix="t93-out-")
    err_path = tempfile.mktemp(prefix="t93-err-")
    out_fd = os.open(out_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC)
    err_fd = os.open(err_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC)
    # ここで複製する「ターン開始前の fd 1/2」は、起動時の dup2 により
    # 既にログ側を指している(プロトコル用パイプではない)。
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
    # Windows では、ターンを越えて生き続けるサブプロセスが一時ファイルの
    # ハンドルを継承した stdout として持ち続けていると、他プロセスが開い
    # ているファイルの削除は PermissionError になる。削除の失敗でラッパー
    # 自体が落ちる(コンテキストを喪失する)のは本末転倒なので、削除の
    # 失敗は許容する(本実装では、後始末する別経路を持つか、削除を遅らせる
    # 設計にする)。
    for path in (out_path, err_path):
        try:
            os.remove(path)
        except OSError:
            pass
    return stdout_text, stderr_text, error


def main():
    shared_globals = {}
    lines: "queue.Queue[str | None]" = queue.Queue()

    def read_stdin():
        try:
            for raw in sys.stdin:
                lines.put(raw)
        finally:
            # EOF (サーバプロセスの終了) または stdin の読み取りエラー。
            # メインスレッドがターンの実行中でハングしていても、サーバが
            # 既に居ないことに変わりはないので、構わず即座に終了する。
            os._exit(0)

    threading.Thread(target=read_stdin, daemon=True).start()

    while True:
        raw = lines.get()
        line = raw.strip()
        if not line:
            continue
        request = json.loads(line)
        stdout_text, stderr_text, error = run_turn(request["code"], shared_globals)
        response = {"stdout": stdout_text, "stderr": stderr_text, "error": error}
        payload = (json.dumps(response) + "\n").encode("utf-8")
        os.write(_PROTOCOL_STDOUT_FD, payload)


if __name__ == "__main__":
    main()
