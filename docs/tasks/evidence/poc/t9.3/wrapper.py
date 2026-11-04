#!/usr/bin/env python3
"""T9.3 PoC: ADR-0013 の永続コンテキストラッパーの試作(本実装ではない)。

stdin から 1 行 1 JSON({"code": "..."})を読み、`code` を共有 globals で
exec し、専用の fd(_PROTOCOL_STDOUT_FD)に 1 行 1 JSON の結果を返す。

起動直後に fd 1 を**恒久的に**捨て先(devnull)へ向け直す。ターン実行中
だけ、fd 1/2 を一時ファイルへ dup2 で退避し、exec の間だけそこへ向け
直す。dup2 はファイル記述子の「番号」が指す先を切り替えるだけなので、
Python レベルの print()/sys.stdout だけでなく、同じ fd を継承した
サブプロセスや、fd へ直接 os.write するネイティブ拡張の出力も、同じ
退避先に素通りで乗る。exec が終わったら fd を devnull(プロトコル用
パイプではない)に戻す。これにより、ターン中に起動されたバックグラウ
ンドスレッドやサブプロセスが、ターンが終わった**後**に fd 1 へ書いても、
プロトコルの改行区切り JSON フレーミングを壊さない(レビューで判明した
不備の修正: 当初の実装は `finally` で fd 1 をプロトコル用パイプへ戻して
いたため、ターン外の fd 1 書き込みがそのままプロトコルに混入した)。
fd 1 の捨て先は fd 2(stderr リレー)への dup2 ではなく専用の devnull
にしている。fd 1 と fd 2 を dup2 で同一のパイプへ恒久的に別名付けすると、
ターン内で `subprocess.run()` を呼ぶと Windows 実機 CI でだけハングする
ことが判明したため(原因は特定できていないが、再現性があり、fd1/fd2の
別名付けが唯一の差分だった)、恒久リダイレクト先を独立した devnull に
変更して回避した。本実装では、ターン外の書き込みをログとして観測したい
なら、devnull ではなく診断用の別ファイル/パイプへ向けることを検討する
(stderr との別名付けは避ける)。

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
# ユーザーコードが起動するサブプロセスには一切見せたくない内部用の fd
# なので non-inheritable にする(PEP 446 の既定どおりのはずだが、明示
# しておく)。
os.set_inheritable(_PROTOCOL_STDOUT_FD, False)
# fd 1 を恒久的に devnull へ向け直す。これ以降、ターンの外で fd 1 に
# 書かれたものは捨てられ、二度とプロトコル用パイプには流れない。
_idle_sink_fd = os.open(os.devnull, os.O_WRONLY)
os.dup2(_idle_sink_fd, 1)
os.close(_idle_sink_fd)


def _drain(path):
    with open(path, "rb") as f:
        return f.read().decode("utf-8", errors="replace")


def run_turn(code, shared_globals):
    # mkstemp (open済みのfdを返す) を使う。mktemp は作成までに競合の
    # 余地がある非推奨API。
    out_fd, out_path = tempfile.mkstemp(prefix="t93-out-")
    err_fd, err_path = tempfile.mkstemp(prefix="t93-err-")
    # out_fd/err_fd 自体は内部用の fd 番号で、ユーザーコードが見るのは
    # これらを dup2 した**先**の fd 1/2 だけでよい。non-inheritable に
    # しておく(fd 1/2 自体は PEP 446 の特例で dup2 の宛先なら常に
    # inheritable になるので、サブプロセスへの継承はそちらで足りる)。
    os.set_inheritable(out_fd, False)
    os.set_inheritable(err_fd, False)
    # ここで複製する「ターン開始前の fd 1/2」は、fd 1 が devnull、
    # fd 2 が stderr リレー(いずれもプロトコル用パイプではない)を
    # 指している。こちらもユーザーコードのサブプロセスに見せる必要は
    # ない。
    saved_stdout_fd = os.dup(1)
    saved_stderr_fd = os.dup(2)
    os.set_inheritable(saved_stdout_fd, False)
    os.set_inheritable(saved_stderr_fd, False)
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
    # DIAGNOSTIC BUILD (temporary): the background stdin-reader thread is
    # disabled to isolate whether its mere presence (concurrent with a
    # subprocess spawned inside a turn) is what hangs on Windows CI. This
    # intentionally drops the EOF-during-a-hung-turn requirement; do not
    # ship this as is.
    shared_globals = {}

    while True:
        raw = sys.stdin.readline()
        if not raw:
            break
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
