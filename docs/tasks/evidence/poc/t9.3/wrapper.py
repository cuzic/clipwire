#!/usr/bin/env python3
"""T9.3 PoC: ADR-0013 の永続コンテキストラッパーの試作(本実装ではない)。

stdin 側の実プロトコル入力は、起動直後に複製した専用の fd
(`_PROTOCOL_STDIN_FD`)から読む。stdout 側の応答も専用の fd
(`_PROTOCOL_STDOUT_FD`)に書く。fd 0/1 の**番号**そのものは、起動直後に
恒久的に devnull へ向け直す。

【なぜ fd 0/1 の番号を devnull にするか(改訂7で fd 0 も追加)】
改訂6までは、fd 1 だけを devnull にし、fd 0(標準入力)は触らず
`sys.stdin`/専用スレッドで直接読んでいた。ところが「別スレッドが stdin
のパイプをブロッキング読み取り中に、別のスレッドから subprocess を
起動する」と Windows 実機 CI で必ずハングすることが判明し(T9.3
evidence 項目1 続き)、改訂6では終了検知の手段を stdin の EOF から
『親 PID の生存ポーリング』に変えて回避した。しかし Opus の最終レビュー
で、**この回避策は `script_file` の `interpreter` が venv の
`python.exe`(リダイレクタ)・`uv run`・py ランチャーのような中継
プロセスを経由する場合に正しく動かない**と指摘された: ラッパーの OS 上
の親はサーバではなく中継プロセスになり、サーバが死んでも中継プロセスは
死なないため、`getppid()` ベースの監視では永久にサーバの消滅を検知
できない。
そこで、ハングの機序について次の仮説を検証した: CPython の
`subprocess`(Windows)は `stdin=None` のとき `GetStdHandle(STD_INPUT_HANDLE)`
を子の stdin として渡すため、子プロセスを起動するたびに「読み取り中の
パイプそのもの」(= fd 0)に触れる。別スレッドがまさにその fd 0 を
`ReadFile` でブロッキング読み取り中だと、Windows の同期 I/O の直列化に
より衝突する、という仮説。
**この仮説は正しかった**: fd 0 の番号そのものを恒久的に devnull へ向け、
実際のプロトコル入力は fd 0 とは独立した non-inheritable の fd
(`_PROTOCOL_STDIN_FD`)から読むように変更したところ、ターン内で
subprocess を起動するテストが Windows 実機 CI で安定して通るように
なった(fd 1 側は改訂5の devnull 化と同じ理由・同じパターン)。
これにより、終了検知を**本来の stdin EOF 方式に戻せる**: サーバだけが
`_PROTOCOL_STDIN_FD` の書き込み側を持つので、`script_file` の
`interpreter` に何段の中継プロセスが挟まっていても、サーバが死ねば
確実に EOF になる(中継プロセスの有無に依存しない)。`os.getppid()` の
生存ポーリング(改訂6)は撤回した。

stdin の読み取りは専用スレッドで行い、EOF を検知したら、メインスレッドが
ターンの実行中でハングしていても構わず `os._exit()` で即座に終了する。
"""
import json
import os
import queue
import sys
import tempfile
import threading
import traceback

_PROTOCOL_STDOUT_FD = os.dup(1)
_PROTOCOL_STDIN_FD = os.dup(0)
# ユーザーコードが起動するサブプロセスには一切見せたくない内部用の fd
# なので non-inheritable にする(PEP 446 の既定どおりのはずだが、明示
# しておく)。
os.set_inheritable(_PROTOCOL_STDOUT_FD, False)
os.set_inheritable(_PROTOCOL_STDIN_FD, False)
# fd 0/1 の番号そのものを恒久的に devnull へ向け直す。これ以降、
# サブプロセスがターン外で fd 0/1 を継承しても、実際のプロトコルの
# パイプには一切触れない(詳細はモジュール docstring 参照)。
_null_out_fd = os.open(os.devnull, os.O_WRONLY)
os.dup2(_null_out_fd, 1)
os.close(_null_out_fd)
_null_in_fd = os.open(os.devnull, os.O_RDONLY)
os.dup2(_null_in_fd, 0)
os.close(_null_in_fd)


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
    shared_globals = {}
    lines = queue.Queue()

    def read_stdin():
        # EOF (サーバプロセスの終了。script_file の interpreter が venv の
        # リダイレクタ・uv run・py ランチャーのような中継プロセスを経由
        # していても、パイプの書き込み側を持つのはサーバだけなので中継
        # プロセスの有無に関わらず確実に起きる)なら 0、読み取りエラー
        # (不正な UTF-8 等)なら 1 で終了する。メインスレッドがターンの
        # 実行中でハングしていても、どちらにせよ構わず即座に終了する。
        exit_code = 0
        buffer = b""
        try:
            while True:
                chunk = os.read(_PROTOCOL_STDIN_FD, 65536)
                if not chunk:
                    break
                buffer += chunk
                while b"\n" in buffer:
                    raw, buffer = buffer.split(b"\n", 1)
                    lines.put(raw.decode("utf-8"))
        except Exception:
            exit_code = 1
        finally:
            os._exit(exit_code)

    threading.Thread(target=read_stdin, daemon=True).start()

    while True:
        line = lines.get().strip()
        if not line:
            continue
        request = json.loads(line)
        stdout_text, stderr_text, error = run_turn(request["code"], shared_globals)
        response = {"stdout": stdout_text, "stderr": stderr_text, "error": error}
        payload = (json.dumps(response) + "\n").encode("utf-8")
        os.write(_PROTOCOL_STDOUT_FD, payload)


if __name__ == "__main__":
    main()
