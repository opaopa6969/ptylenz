# ptylenz — PROJECT.md

> [English](PROJECT.md) · 日本語

> アーキテクチャ、設計判断、実装ノートをまとめたハンドオフドキュメント。

## ワンライナー

**syslenz が `/proc` の体験を変えたように、ptylenz は PTY の体験を変える。**

PTY プロキシ + TUI。コマンド出力をブロック単位で構造化し、検索・折りたたみ・コピーを可能にする。ターミナルの中で動くターミナル。

## 解こうとしている問題

### 痛みの優先順位

1. **出力が流れて消える（スクロール地獄）** — Claude Code が 2000 行出力すると、先頭が見えない
2. **テキスト選択・コピーが辛い** — tmux/マウス選択は折り返し位置に `\n` が混入する
3. **「さっきのあれ」を探せない** — スクロールバックの目 grep は 1978 年の VT100 と変わらない

### 根本原因

これらは全て同じ原因に行き着く: **PTY ストリームが構造を持たない**。

バイトが流れて、バイトが消える。コマンドの境界も、出力の長さも、成功/失敗も、PTY レベルでは全て不可視。

## アーキテクチャ

### なぜシェルではなく PTY プロキシか

shell が fork/exec した後、**shell はデータパスに存在しない**:

```
子プロセス (claude-code)
  stdout → PTY slave → PTY master → ターミナルエミュレータ

bash は wait() しているだけ。出力を一切見ていない。
```

したがって、出力を構造化するには PTY の master 側に座る必要がある。
これは tmux と同じレイヤー（PTY multiplexer）だが、目的が違う:

- tmux: 画面分割 + セッション永続化
- ptylenz: 出力のブロック化 + 検索 + コピー

### データフロー

```
┌──────────────────────────────────────────────┐
│ ptylenz プロセス                             │
│                                              │
│  Terminal ←→ [PTY proxy] ←→ [PTY] ←→ bash    │
│                  ↓                     ↓     │
│            [Block Engine]         fork/exec  │
│                  ↓                     ↓     │
│            [TUI Renderer]         commands   │
│                                              │
│        [Claude JSONL feeder] ────────┐       │
│              ↓                       ↓       │
│        [Block Engine] ←──── claude turns     │
└──────────────────────────────────────────────┘
```

PTY proxy が全 I/O を中継し、bash が吐く OSC マーカーから「ここからここまでが 1 コマンドの出力」と認識する。並行して Claude Code セッション JSONL を tail し、ユーザー/アシスタントのターンを兄弟ブロックとしてエンジンに流し込む。

### ブロック検出: OSC 133 プロトコル

iTerm2/Warp/VS Code Terminal 互換の OSC 133 シーケンスを使用:

| Sequence | 意味 |
|----------|------|
| `\e]133;A\a` | プロンプト開始 |
| `\e]133;C\a` | コマンド実行開始 |
| `\e]133;D;N\a` | コマンド終了, exit code = N |
| `\e]133;E;cmd\a` | コマンドテキスト（ブロックタイトル用） |

Bash インテグレーションは `PROMPT_COMMAND` + `PS0` + `PS1` で発行する。`DEBUG` trap は採用しない — 関数内のサブコマンドでもネストして発火してしまう問題があるため。

### 二モードのキーマッピング

ptylenz の UI は単一の原則に従う: **Normal モードでは ptylenz は不可視**。`Ctrl+]` が唯一の境界キー。

```
Normal mode: 全キーストロークが bash へ素通し
                ↓ Ctrl+]
Ptylenz mode: ratatui オーバーレイ
              ├─ List view: ブロック一覧
              └─ Detail view: 一ブロック全画面 + カーソル + vim 風 visual 選択
```

### 折り返し非依存のコピー

`block.output` は PTY 通過時の生バイト列を保持する。`output_text()` は ANSI ストリップのみで、画面幅の折り返しは関与しない。`tmux` / マウス選択が画面グリッドからコピーするのに対し、ptylenz は元データからコピーする。長い `curl` を 80 列ターミナルで実行しても、コピー結果は元の一行のまま。

### Alt-screen サポート（vt100 シャドウ）

`vim` / `less` / `claude` のような alt-screen TUI に対しては並列で vt100 パーサに同じバイトを流し、コマンド終了時にグリッドのスナップショットを取って `rendered_text` に保存する。これにより、TUI が alt-screen を抜けて消えた後でもブロックの中身が読める状態で残る。

### Claude Code セッション統合

`~/.claude/projects/<cwd-slug>/<session-id>.jsonl` をポーリング tail し、user/assistant ターンを `BlockSource::ClaudeTurn` ブロックとしてエンジンに ingest する。シェルブロックと Claude ターンが時系列で混在表示される。

## 設計判断

### D1: bash を内側に抱える

ptylenz は bash を置き換えない。bash を PTY の中で動かし、その I/O を中継するだけ。

- 既存の `.bashrc`、補完、エイリアス、すべて動く
- ユーザーの学習コストがゼロ
- バイナリ 1 つをコピーするだけでどのマシンでも使える

### D2: ゼロコンフィグで 80 点

syslenz の「21 seconds to first insight」と同じ思想。設定ファイルは一切不要。

### D3: ratatui（syslenz と共通基盤）

syslenz で使っている ratatui をそのまま使用。将来的にウィジェットライブラリを共有する可能性がある。

### D4: claude-session-replay と互換な JSON

`e` での export は [claude-session-replay](https://github.com/opaopa6969/claude-session-replay) の common log model に準拠する。同じ JSON が HTML/ターミナル/MP4 レンダラで再生できる。

### D5: LSP/DAP は外部プロセス（将来）

シェル入力補完（LSP）やスクリプトデバッグ（DAP）を足す場合は別プロセス。ptylenz の TUI がクライアントとなり JSON-RPC で通信する。クラッシュ伝播を避け、VS Code/Neovim とも共有できる。

## MVP スコープ

### 完了

- [x] PTY proxy（fork/exec bash, I/O リレー, SIGWINCH 転送）
- [x] OSC 133 パーサ（ESC ターミネータも対応）
- [x] ブロックエンジン（セグメント、検索、JSON エクスポート）
- [x] ratatui オーバーレイ（List view / Detail view）
- [x] ブロックナビゲーション（j/k, g/G, n/N, /検索）
- [x] 全文検索（クロスブロック）
- [x] OSC 52 + xclip/pbcopy クリップボード
- [x] claude-session-replay 互換 JSON エクスポート
- [x] 自動折り畳み（>50 行）
- [x] vt100 シャドウグリッド（alt-screen TUI 対応）
- [x] Claude Code JSONL セッション統合
- [x] Detail view: linewise + blockwise（矩形）選択
- [x] 折り返し非依存コピー（生 PTY バイト列を保持）
- [x] Linux x86_64 / macOS arm64 / macOS x86_64 リリースバイナリ

### 将来

- シェル入力補完（LSP 経由、別プロセス）
- スクリプトデバッガ（DAP 経由、別プロセス）
- syslenz パネル統合
- ブロック diff 表示
- セッション永続化（tmux 置き換え）

## クレート構造

```
ptylenz/
├── Cargo.toml
├── src/
│   ├── main.rs            # エントリポイント
│   ├── pty.rs             # PTY プロキシ: fork, リレー, リサイズ
│   ├── block.rs           # ブロックエンジン: OSC パース, セグメント, 検索, vt100 シャドウ
│   ├── tui_app.rs         # TUI: イベントループ, ratatui レンダリング, キーバインディング
│   └── claude_feeder.rs   # ~/.claude/projects/ 配下の JSONL を tail
├── .github/workflows/
│   └── release.yml        # tag push でリリースバイナリビルド
├── PROJECT.md / PROJECT.ja.md
├── DESIGN.md / DESIGN.ja.md
└── README.md / README.ja.md
```

## 実装ノート

### PTY プロキシのキーポイント

- `nix::pty::openpty()` で初期 winsize を渡してペア作成（重要: 0×0 のままだと ncurses アプリが幅 0 で描画して画面が崩れる）
- 子プロセス: `setsid()` → `TIOCSCTTY` → `dup2()` で stdin/stdout/stderr を slave へ
- 親プロセス: master fd を保持し I/O 中継
- リサイズ: `TIOCSWINSZ` + 子プロセスへ `SIGWINCH`

### イベントループの多重化

`polling` クレートで stdin と PTY master fd を level-triggered で監視。crossterm の event loop は使わず、生バイトを自前でデコード（`decode_keys`）。alt-screen への切り替えも `crossterm::execute!` で制御。

### Bash インテグレーションの注入

一時 rcfile を生成して `--rcfile` で渡す:

```bash
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"
PS0='\[\e]133;C\a\]'
PS1='\[\e]133;A\a\]'"$PS1"
PROMPT_COMMAND='__ptylenz_precmd'
```

`PROMPT_COMMAND` で 133;D（exit code）と 133;E（直前の history からコマンド復元）を発行する。

### OSC パーサの状態機械

```
Normal → ESC(\x1b) → Escape
Escape → ']' → OscStart → OscBody
OscBody → BEL(\x07) → [decode] → Normal
OscBody → ESC \\ (ST) → [decode] → Normal
Escape → other → emit(ESC + byte) → Normal
```

OSC 133 以外（色、カーソル移動）はそのまま通過。

### クリップボード

OSC 52 を第一候補（tmux 内でも動く）。フォールバックとして xclip (Linux) / pbcopy (macOS)。

## テスト戦略

1. **ユニットテスト**: OSC パーサ、ブロックエンジン（境界検出、検索、エクスポート）、選択ロジック
2. **統合テスト**: PTY プロキシで実際に bash を起動 → コマンド実行 → ブロック検出
3. **回帰テスト**: 折り返し非依存コピー（400 文字を 40 列エンジンに流して 1 行のままを保証）

## 関連プロジェクト

| Project | 関係 |
|---------|------|
| [syslenz](https://github.com/opaopa6969/syslenz) | 兄弟プロジェクト。ratatui 基盤共有。`/proc` → 構造化 を PTY に適用 |
| [claude-session-replay](https://github.com/opaopa6969/claude-session-replay) | エクスポートの再生先。同じ common log model を共有 |
| tmux | 同レイヤー（PTY multiplexer）。目的が異なる（画面分割 vs 出力構造化） |
| Warp | 同じアイデア（block-based output）を GUI ではなく TUI で実現 |
| bash | ptylenz が内包する。置き換えない |

## ビルド & 実行

```bash
cargo build
cargo test
cargo run

# インストール
cargo install --path .
ptylenz
```

リリースバイナリは [Releases](https://github.com/opaopa6969/ptylenz/releases) から取得可能。
