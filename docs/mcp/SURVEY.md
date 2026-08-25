# ptylenz — MCP 化調査（Phase 1）

## 概要

ptylenz は Rust 製の **PTY プロキシ + ratatui TUI** である。bash を PTY の内側で動かし、OSC 133 マーカーでコマンド出力をブロック単位に構造化する。ブロックの検索・折り畳み・コピー・JSON エクスポートを対話的に提供する。Claude Code の JSONL セッションログを並行 tail し、シェルブロックと同一タイムラインに表示する機能を持つ。

- **種類**: `cli`（単一バイナリ、引数なし、HTTP API なし、ライブラリクレートなし）
- **プラットフォーム**: Linux x86_64 / macOS（Windows 非対応）
- **依存**: ratatui, crossterm, nix, vt100, polling, serde, chrono 等

## 判定と理由

**判定: `skip`（対応しない）**

- 全ての能力が「人間が操作する対話的 PTY セッション」という状態に強く結びついており、ステートレスな MCP tool として切り出せる操作が存在しない。
- 常駐サーバ化しても、エージェントから呼べる意味のある操作がない。TUI は人間のキーボード操作対象であり、リモートから tool として叩く用途がない。
- エクスポート形式（claude-session-replay common log model）の共有は、既存の `claude-session-replay` MCP サービスがカバーしている。ptylenz 側を MCP 化しても得がない。
- ライブラリクレートでもないため、他の MCP サービスから import して使う経路もない。

## 公開候補

MCP 化しない（`skip`）ため、公開候補は構成上の参考記録である。

| kind | name | io | 副作用 | 長時間 | maps_to |
|------|------|----|--------|--------|---------|
| tool | `segment` | raw PTY bytes → Vec\<Block\> | read | false | `src/block.rs:BlockEngine::feed_output()` |
| tool | `search` | query: text → Vec\<(block_id, line_num, line_text)\> | read | false | `src/block.rs:BlockEngine::search()` |
| tool | `export` | session blocks → JSON string (file write) | write | false | `src/block.rs:BlockEngine::export_json()` |
| tool | `decode_claude_turn` | JSONL line → Option\<ClaudeEvent\> | read | false | `src/claude_feeder.rs:decode_line()` |
| resource | `spec` | — | — | — | 該当なし |
| resource | `guide` | — | — | — | 該当なし |
| skill | `pty-block-model` | — | — | — | locality: repo |

これらはいずれも「ローカル PTY セッション状態」が前提であり、ステートレスな MCP tool として独立して呼べるものではない。

## 組み合わせ例

該当なし。ptylenz の全能力がローカル PTY セッション状態に依存し、他の MCP サービスと組み合わせる絵が描けない。

エクスポート JSON は `claude-session-replay` MCP サービスがレンダリングできるが、それは claude-session-replay 側の能力であり ptylenz 側の MCP 化では得がない。

## 依存と協調

| 相手 repo | 方向 | 能力 | 現在 MCP 入口あり | 備考 |
|-----------|------|------|-------------------|------|
| `claude-session-replay` | depends_on | common log model JSON フォーマット仕様（エクスポート形式の共有） | あり（volta 参加済み） | MCP 入口ではなくデータフォーマットの依存。エクスポート JSON を claude-session-replay が消費できるが、それは claude-session-replay 側の能力 |
| `syslenz` | depends_on | 設計思想・ratatui TUI 基盤の共有（兄弟プロジェクト） | あり（volta 参加済み） | コードレベルの依存ではなく設計上の関連。ptylenz から syslenz の MCP 入口を呼む用途はない |

協調が必要な MCP 入口の依存関係はない。Phase 2 での issue-hub 登録は不要。

## ライブラリのサーバ化

該当しない。ptylenz はバイナリクレートであり、ライブラリとして他サービスから import されることもない。サーバ化の必要もない。

## リスク

- MCP 化しないため新たなリスクはない。
- 仮にサーバ化した場合: PTY はプロセス制御（fork/exec/setsid）を伴い、サーバプロセスで安全に管理する必要があるが、対話的セッションのリモート操作は本質的に脆弱。
- ブロック履歴は in-memory で揮発性。サーバ化しても状態の永続化方針が未定（v0.1 では永続化なし）。

## 持ち主への質問

1. 将来 ptylenz のブロックエンジンをライブラリクレートとして切り出し、他のツールが PTY 出力を構造化できるようにする計画はあるか？（あれば `library-serve` 再検討の余地があるが、現時点では PROJECT.md にその記載なし）
2. Claude Code JSONL のデコードロジック（`claude_feeder.rs`）を独立した MCP tool として公開する価値はあるか？（ただし `claude-session-replay` が同等のパース能力を既に持つ可能性が高い）
