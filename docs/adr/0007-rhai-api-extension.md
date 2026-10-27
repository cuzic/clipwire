# ADR-0007: Rhai スクリプト API の拡充(縮小版)

- Status: Proposed (改訂: Opus レビュー反映)
- Date: 2026-09-30
- Depends on: ADR-0009(ランナー/Job Object/キャンセル), ADR-0011(toast 経路)

## Context

Rhai の関数は `run`, `run_ok`, `file_exists`, `rm` のみで、`cmd /c start` の回避策が targets.toml に現れている。一方、承認済みスクリプトは `run(["cmd","/c","set"])` や `run(["powershell", ...])` で何でもできる。したがって、API 側の制限(`env` のブロックリスト、`wait_port` の localhost 限定、`read_text` のパス脱出チェック)は**能力を削らず、読みやすさだけを削る**。境界は「人間が全文を確認して承認すること」(ADR-0006)であり、API 制限は境界ではない。

## Decision

採用する(用途が明確で、承認者が読んで判断できるもの):

| 関数 | 内容 |
|---|---|
| `sleep(ms)` | 待機(1 回 60s 上限)。100ms 刻みでキャンセルフラグを確認する(ADR-0009 §4) |
| `start_detached([...])` | 標準ハンドルを継承せず、`CREATE_BREAKAWAY_FROM_JOB \| DETACHED_PROCESS \| CREATE_NEW_PROCESS_GROUP` で起動し、`dir` 基準の相対パス解決をランナーで行う(`cmd /c start` の `\` エスケープ・CWD 問題を解消)。PID を返す。Rhai はサーバプロセス内で評価されるため、そもそもジョブ外から起動される。**自己更新ターゲット(clipwire 自身の再起動)は必ずこれを使う**(ADR-0009 §3)。外側の Job が `BREAKAWAY` を許さない場合は失敗を明示する |
| `notify(msg)` | WinRT の toast を表示する。**ADR-0011 で PowerShell 経路を廃止した後の WinRT 実装を使う**。`msg` は XML エスケープされる。完了通知向け |
| (`print` / `debug`) | `Engine::on_print` / `on_debug` でジョブログ(ADR-0009)に書く。専用の `log` 関数は作らない |

採用しない:

- `env(name)`: 必要な値は、承認対象の `env` 宣言(ADR-0010 の正規形)で渡す。名前ベースのブロックリストは境界にならない(`*_PAT`、`DATABASE_URL` 等が漏れる)。
- `retry(n, delay, closure)`: `loop` + `run_ok` + `sleep` で書ける。`NativeCallContext` の実装コストに見合わない。
- `wait_port(port, timeout)`: `run(["powershell", ...])` で書ける。必要が出たら再検討。
- `read_text(path)`: `dir` 相対の単純な join だけの関数を作る価値が薄く、canonicalize/`\\?\` 比較の実装罠もある。必要が出たら再検討。
- `http_get` / `http_post`: 持ち出し経路になり、承認者が判断しにくい。必要なら `run(["curl", ...])` と明示的に書く(argv に見える)。

共通:

1. 全関数は ADR-0009 のランナー経由で動く(環境の `CLIPD_TOKEN` 除去を含む)。
2. Rhai `Engine` の上限設定(operations / string / array / map / call levels)は ADR-0009 §4 に従う。
3. `args`(ADR-0004)は `params` 宣言があるターゲットにのみ注入し、既存スクリプトの `let args = ...` を壊さない。

## Consequences

- `cmd /c start` 系の落とし穴を避けられ、ターゲットが読みやすくなる。
- 追加は 3 関数に絞ったため、承認者が確認すべき表面が小さい。`env` / `retry` 等が必要になった場合は、本 ADR を更新して追加する。
- `start_detached` は Windows 固有(BREAKAWAY の可否)に依存するため、実装着手時に PoC で確認する(ADR-0009)。

## Alternatives considered

- **API 最小のまま**: 安全だが、現状のワークアラウンドが恒常化する。
- **Rhai をやめて別言語にする**: 移行コストが大きい。
