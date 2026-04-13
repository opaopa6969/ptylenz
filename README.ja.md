# ptylenz

> [English](README.md) · 日本語

> **PTY のための Wireshark** — ターミナルの出力をブロック単位で構造化する。

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## これは何か

ptylenz はあなたとシェルの間に座る。すべてのコマンドの出力が**ブロック**になり、ターミナルから離れることなくナビゲート・折り畳み・検索・コピーできる。スクロール地獄も、「さっきのエラーどこいった？」も、もう終わり。

```
Before:                              After:
$ claude-code "fix auth"             ┌─ #42 claude-code "fix auth" ─ 14:23 ─┐
(2000 行が流れる)                    │ ▶ 3 ファイル修正 (2847 行)            │
(必死にスクロールアップ)             │ ▷ [展開] [コピー] [検索] [pin]        │
(諦めて再実行)                       └───────────────────────────────────────┘
```

## なぜ tmux / マウス選択ではなく ptylenz か

ほとんどのターミナルツールは、スクロールバックを**画面グリッド**（80×24 のセル配列）から読む。ptylenz は **PTY のバイトストリーム**（子プロセスが実際に書いたバイト列）から読む。文字面では小さな違いだが、実用上の差は大きい:

| | tmux / マウス選択 | ptylenz |
|-|------------------|---------|
| **長い行の折り返し** | 折り返し位置に `\n` が混入 | 元の一行のままコピー |
| **ANSI エスケープ** | カラーコードがクリップボードに残る | コピー時に除去 |
| **ブロック境界** | なし — 目でプロンプトを探す | OSC 133 マーカーで正確 |
| **長い出力の検索** | 手動スクロール | 全ブロック横断の全文検索 |
| **選択** | 画面セル単位の矩形 | 元出力の linewise + blockwise |

具体例: 200 文字の `curl -X POST … -H "Authorization: …" -d '{…}'` を 80 列ターミナルに貼ると、ターミナルは表示用に折り返す。tmux でコピーすると 3 分割された壊れたコマンドになる。ptylenz でコピーすると元の一行に戻る — ptylenz は画面ではなくバイト列を見ているから。

## インストール

### ビルド済みバイナリ

[Releases](https://github.com/opaopa6969/ptylenz/releases) から最新版を取得:

```bash
# Linux x86_64
curl -L https://github.com/opaopa6969/ptylenz/releases/latest/download/ptylenz-linux-x86_64 -o ptylenz
chmod +x ptylenz && sudo mv ptylenz /usr/local/bin/

# macOS (Apple Silicon)
curl -L https://github.com/opaopa6969/ptylenz/releases/latest/download/ptylenz-macos-aarch64 -o ptylenz
chmod +x ptylenz && sudo mv ptylenz /usr/local/bin/

# macOS (Intel)
curl -L https://github.com/opaopa6969/ptylenz/releases/latest/download/ptylenz-macos-x86_64 -o ptylenz
chmod +x ptylenz && sudo mv ptylenz /usr/local/bin/
```

### ソースから

```bash
cargo install --path .
```

実行:

```bash
ptylenz
```

bash が ptylenz の中で起動する。挙動は今までと一切変わらない — ただし出力を振り返りたくなった瞬間、それは構造化されている。

## 二つのモード、一つのプレフィックスキー

UI は単一のルールに従う: **Normal モードでは ptylenz は不可視**。すべてのキーストローク（一つを除いて）はそのまま bash へ、すべての出力バイトはそのまま画面へ。ptylenz は何もしない。例外は `Ctrl+]` — これだけが Ptylenz モードへの境界。

### Normal モード

| キー | 動作 |
|------|------|
| (すべて) | bash へ素通し |
| `Ctrl+]` | Ptylenz モードへ |

### Ptylenz モード — ブロック一覧

| キー | 動作 |
|------|------|
| `j` / `k` / `↑` / `↓` | 次 / 前のブロック |
| `g` / `G` | 先頭 / 末尾のブロックへ |
| `Enter` | 選択中のブロックを展開 / 折り畳み |
| `v` | Detail view を開く |
| `/` | 全ブロック横断検索 |
| `n` / `N` | 次 / 前の検索ヒット |
| `y` | 選択中のブロックをクリップボードへコピー |
| `e` | セッションを JSON エクスポート |
| `p` | 選択中のブロックを pin / unpin |
| `q` / `Esc` / `Ctrl+]` | Normal モードへ戻る |

### Ptylenz モード — Detail view

一ブロックを全画面表示し、カーソル移動と vim 風の visual 選択ができる。

| キー | 動作 |
|------|------|
| `h` / `j` / `k` / `l` | カーソル移動 |
| `g` / `G` / `0` / `$` | 先頭 / 末尾 / 行頭 / 行末 |
| `Ctrl+u` / `Ctrl+d` | ページアップ / ダウン |
| `v` | **行選択** (visual line) の開始 / 終了 |
| `Ctrl+v` | **矩形選択** (visual block) の開始 / 終了 |
| `y` | 選択範囲をコピー（選択なしならブロック全体） |
| `Y` | 常にブロック全体をコピー |
| `Esc` | 選択解除（選択なしならリストへ戻る） |
| `q` | リストへ戻る |

矩形選択は `ls -l` から特定のカラムを抜き出すとき、あるいはスクリプト本体を先頭マーカーなしで取り出したいときの定番。vim ユーザーには馴染みのある操作。

## 仕組み

ptylenz は **PTY プロキシ**。擬似端末を作り、その中で bash を動かし、すべての I/O を中継する。Shell インテグレーション（OSC 133 マーカー — iTerm2 / Warp / VS Code Terminal と同じプロトコル）が各コマンドの出力境界を ptylenz に伝える。

```
You ←→ ptylenz (PTY master) ←→ bash (PTY slave) → fork/exec → commands
              ↓
         Block Engine
         (segment, index, store)
```

インテグレーションは一時 rcfile を生成して `--rcfile` で渡す方式。先に既存の `~/.bashrc` を `source` するので、プロンプト・エイリアス・補完はそのまま動き続ける。

`vim` / `less` / `claude` のような alt-screen を使う TUI に対しては並列で vt100 シャドウグリッドを保持し、コマンド終了後でもブロックの中身が読める状態を保つ — ライブ表示には影響しない。

## ゼロコンフィグ

`.ptylenzrc` なし。テーマなし。プラグインなし。起動するだけ。

バイナリ 1 つコピーすればどのマシンでも動く。SSH 先で `./ptylenz`、それで終わり。

## Claude Code セッション統合

ptylenz が Claude Code が触ったプロジェクトディレクトリで起動した場合、ptylenz はアクティブなセッション JSONL ログを tail し、各ターン（user / assistant）をシェルコマンドと同じリストの中に独立したブロックとして表示する。シェルブロックと AI ターンが時系列で混在 — Claude とペアで作業しているとき、単一のタイムラインで見たい場面で便利。

`e` でのエクスポートは [claude-session-replay](https://github.com/opaopa6969/claude-session-replay) の common log model に準拠するので、エクスポートされた JSON はそのプロジェクトの HTML / ターミナル / MP4 レンダラで再生可能。

## syslenz との関係

| | [syslenz](https://github.com/opaopa6969/syslenz) | ptylenz |
|-|---------|---------|
| **構造化対象** | `/proc` と `/sys` | PTY 出力ストリーム |
| **動機** | `cat /proc/meminfo` は 1970 年代の UX | スクロールバック grep は 1978 年の VT100 UX |
| **技術** | Rust + ratatui | Rust + ratatui |

同じファミリー、同じ哲学。OS が提供する生のテキストインターフェイスを、ナビゲート可能な構造に変える。

## Tips: シェル起動時に自動で入る（任意・上級者向け）

`~/.bashrc` の末尾に置くと、ターミナルを開いた瞬間に ptylenz の中で bash が起動する:

```bash
# 非対話シェル（scp / rsync / ssh host cmd 等）では何もしない
case $- in *i*) ;; *) return ;; esac
[ -z "$PTYLENZ" ] && command -v ptylenz >/dev/null && exec ptylenz
```

`$PTYLENZ` は ptylenz が子 bash に対してセットする環境変数で、二重起動を防ぐ。`$-` の対話チェックは必須 — これがないと scp / rsync / `ssh host 'cmd'` のような非対話セッションで bashrc が読まれた際に ptylenz が起動してしまい、転送プロトコルが壊れる。

ptylenz が落ちると新しいシェルがすべて詰まる可能性があるので、しばらく使って安定を確認してから入れるのを推奨。リカバリは `bash --norc`。

## ドキュメント

- [PROJECT.ja.md](PROJECT.ja.md) — アーキテクチャ、設計判断、実装ノート
- [DESIGN.ja.md](DESIGN.ja.md) — 設計判断に至るまでの思考プロセス

## ライセンス

MIT
