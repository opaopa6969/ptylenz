# ptylenz — PROJECT.md

> Claude Code handoff document. Contains design decisions, architecture rationale, and implementation roadmap.

## One-liner

**syslenz が `/proc` の体験を変えたように、ptylenz は PTY の体験を変える。**

PTY プロキシ + TUI。コマンド出力をブロック単位で構造化し、検索・折りたたみ・コピーを可能にする。ターミナルの中で動くターミナル。

## Problem Statement

### 痛みの優先順位（ユーザーインタビューより）

1. **出力が流れて消える（スクロール地獄）** — Claude Code が 2000 行出力すると、先頭が見えない
2. **テキスト選択・コピーが辛い** — tmux の選択モードは地獄
3. **「さっきのあれ」を探せない** — スクロールバックの目 grep は 1978 年の VT100 と変わらない
4. **複数セッションの管理** — 後回し（tmux が一応動く）

### 根本原因

1-3 は全て同じ原因：**PTY ストリームが構造を持たない**。

バイトが流れて、バイトが消える。コマンドの境界も、出力の長さも、成功/失敗も、PTY レベルでは全て不可視。

## Architecture

### なぜシェルではなく PTY プロキシか

shell がプロセスを fork/exec した後、**shell はデータパスに存在しない**：

```
子プロセス (claude-code)
  stdout → PTY slave → PTY master → ターミナルエミュレータ

bash は wait() しているだけ。出力を一切見ていない。
```

したがって、出力を構造化するには PTY の master 側に座る必要がある。
これは tmux と同じレイヤー（PTY multiplexer）だが、目的が異なる：
- tmux: 画面分割 + セッション永続化
- ptylenz: 出力のブロック化 + 検索 + コピー

### データフロー

```
┌──────────────────────────────────────────────┐
│ ptylenz プロセス                              │
│                                              │
│  Terminal ←→ [PTY proxy] ←→ [PTY] ←→ bash   │
│                  ↓                     ↓     │
│            [Block Engine]         fork/exec   │
│                  ↓                     ↓     │
│            [TUI Renderer]        claude-code  │
│                                              │
└──────────────────────────────────────────────┘
```

PTY proxy が全 I/O を中継する。bash がコマンドの前後に OSC マーカーを吐き、
ptylenz がそれを検出して「ここからここまでが 1 コマンドの出力」と認識する。

### ブロック検出: OSC 133 プロトコル

iTerm2/Warp/VS Code Terminal 互換の OSC 133 シーケンスを使用：

| Sequence | Meaning |
|----------|---------|
| `\e]133;A\a` | Prompt start (新しいプロンプト表示) |
| `\e]133;C\a` | Command execution start |
| `\e]133;E;cmd\a` | Command text (ブロックのタイトル用) |
| `\e]133;D;N\a` | Command finished, exit code = N |

bash の `PROMPT_COMMAND` と `DEBUG` trap を使って自動注入する。

### フォールバック（シェルインテグレーション無し）

SSH 先など、インテグレーションを仕込めない環境では、
プロンプトパターンの正規表現マッチで「おおよそ」のブロック境界を検出する。
精度は落ちるが、無いよりはるかに良い。

## Key Design Decisions

### D1: bash を内側に抱える

ptylenz は bash を置き換えない。bash を PTY の中で動かし、その I/O を中継する。
- 既存の `.bashrc`, 補完, エイリアス, 全て動く
- ユーザーの学習コストがゼロ
- バイナリ 1 つをコピーするだけでどのマシンでも使える

### D2: ゼロコンフィグで 80 点

syslenz の「21 seconds to first insight」と同じ思想。
起動したらデフォルトで全機能が動く。設定ファイルは一切不要。

### D3: TUI は ratatui（syslenz と共通基盤）

syslenz で使っている ratatui をそのまま使用。
将来的にウィジェットライブラリを共有する可能性がある。

### D4: LSP/DAP は外部プロセス（将来）

シェルの入力補完（LSP）やスクリプトデバッグ（DAP）を足す場合、
ptylenz 本体に組み込むのではなく、別プロセスとして動かす。
ptylenz の TUI がクライアントとなり、JSON-RPC で通信する。

これは「お作法」として正しい設計：
- クラッシュが伝播しない
- VS Code / Neovim からも同じ LSP/DAP を使える
- ptylenz 本体が軽い

## MVP Scope

### In（作る）

- [x] PTY proxy (fork/exec bash inside PTY, relay all I/O)
- [ ] OSC 133 parser (detect block boundaries)
- [ ] Block engine (segment output, index, store)
- [ ] Block display (collapsed/expanded view)
- [ ] Block navigation (Ctrl+↑/↓ to jump between blocks)
- [ ] Full-text search across all blocks (Ctrl+F)
- [ ] Block copy to clipboard (Ctrl+Y)
- [ ] JSON export of session (Ctrl+E)
- [ ] Auto-collapse long output (>50 lines)
- [ ] Terminal resize forwarding (SIGWINCH)

### Out（作らない、今は）

- シェル文法の変更・パーサー（UBNF 等）
- パイプラインの構造化データ（nushell 的）
- プラグインシステム
- セッション永続化（tmux が担当）
- syslenz 統合パネル
- AI 出力の意味解析
- ブロックの diff 表示
- リモートセッション管理

## Crate Structure

```
ptylenz/
├── Cargo.toml
├── src/
│   ├── main.rs          # Entry point, CLI args
│   ├── pty.rs           # PTY proxy: fork, relay, resize
│   ├── block.rs         # Block engine: OSC parse, segment, search
│   └── tui_app.rs       # TUI: event loop, render, keybindings
├── PROJECT.md           # This file
├── DESIGN.md            # Conversation-derived design rationale
└── README.md
```

## Implementation Notes

### PTY Proxy の核心コード

`nix::pty::openpty()` で PTY ペアを作り、子プロセスで slave 側に接続、
親プロセスで master 側を保持して全 I/O を中継する。

重要なポイント：
- `setsid()` で新しいセッションリーダーにする
- `TIOCSCTTY` で PTY を制御端末にする
- `dup2()` で stdin/stdout/stderr を slave にリダイレクト
- `TIOCSWINSZ` + `SIGWINCH` でリサイズを転送

### OSC パーサーのステートマシン

```
Normal → ESC(\x1b) → Escape
Escape → ']' → OscStart → OscBody
OscBody → BEL(\x07) → [decode] → Normal
OscBody → ESC(\x1b) → [decode] → Normal (ST terminator)
Escape → other → emit(ESC + byte) → Normal
```

OSC 133 以外のエスケープシーケンス（色、カーソル移動等）は
そのまま通過させる（strip しない）。

### クリップボード

OSC 52 を第一候補（tmux 内でも動く）。
フォールバックとして xclip (Linux) / pbcopy (macOS)。

## Relationship to Other Projects

| Project | Relationship |
|---------|-------------|
| syslenz | 兄弟プロジェクト。ratatui 基盤共有。`/proc` → 構造化 の思想を PTY に適用 |
| tmux | 同じレイヤー（PTY multiplexer）。将来的に tmux の中で動くか、置き換えるか |
| Warp | 同じアイデア（block-based output）を GUI ではなく TUI で実現 |
| bash | ptylenz が内包する。置き換えない |
| unlaxer/UBNF | 将来的にシェル入力補完の文法定義に使う可能性 |

## Testing Strategy

1. **Unit tests**: OSC parser, block engine (境界検出, 検索, エクスポート)
2. **Integration tests**: PTY proxy の起動/終了, I/O リレーの正確性
3. **Manual smoke test**: `ptylenz` 起動 → `ls`, `echo`, `cat` → ブロック確認

## Build & Run

```bash
cargo build
cargo test
cargo run

# Or install
cargo install --path .
ptylenz
```
