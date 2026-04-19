# ptylenz — アーキテクチャ

> [English](architecture.md) · 日本語

本ドキュメントはコンポーネントレベルの詳細解説。  
高レベルの動機は [README.ja.md](../README.ja.md)、  
設計判断の経緯は [DESIGN.ja.md](../DESIGN.ja.md) を参照。

> **対応プラットフォーム**: Linux および macOS のみ。Windows は未対応。

---

## システム全体図

```
┌─────────────────────────────────────────────────────────────┐
│ ptylenz プロセス                                             │
│                                                             │
│  ターミナル stdin ──► [PTY プロキシ] ──► PTY master fd       │
│                                              │              │
│  ターミナル stdout ◄── (クリーンバイト) ◄────┤              │
│                                              │              │
│                                       [ブロックエンジン]    │
│                                              │              │
│                                       [vt100 シャドウ]      │
│                                              │              │
│                                 ┌────────────┘              │
│                                 ▼                           │
│                          [ratatui TUI オーバーレイ]         │
│                           (alt-screen、要求時のみ)          │
│                                                             │
│  [Claude フィーダスレッド] ──► ブロックエンジン              │
│    (JSONL ログをポーリング)                                  │
└─────────────────────────────────────────────────────────────┘
                                  │
                        PTY slave fd (カーネル)
                                  │
                        bash (子プロセス)
                                  │
                        fork/exec → コマンド群
```

ソースファイル構成:

| ファイル | 責務 |
|---------|------|
| `src/main.rs` | エントリポイント。`$SHELL` を読んで `App` を生成 |
| `src/pty.rs` | PTY プロキシ: fork・リレー・リサイズ・SIGWINCH |
| `src/block.rs` | OSC パーサ・ブロックエンジン・vt100 シャドウ・JSON エクスポート |
| `src/tui_app.rs` | イベントループ・ratatui レンダリング・キーバインド |
| `src/claude_feeder.rs` | Claude Code JSONL ログ追跡・`ClaudeEvent` 送信 |

---

## PTY プロキシ (`pty.rs`)

### 役割

`PtyProxy::spawn` が子 bash を新しい PTY の中に fork する:

1. 実ターミナルの現在 winsize を取得 (`TIOCGWINSZ`)。
2. その winsize で `openpty()` — この初期サイズが重要: 省略すると `LINES`/`COLUMNS` が 0 または 80×24 で読まれ、最初の `SIGWINCH` より前に `setupterm()` を呼ぶ ncurses アプリが誤った幅で描画し、画面が階段状になる。
3. 子プロセス: `setsid()` → `TIOCSCTTY` → `dup2(slave, 0/1/2)` → `exec(bash, --rcfile, wrapper.sh, -i)`。
4. 親プロセス: master fd を保持し `PtyProxy` を返す。

### リレーループ

リレーは `tui_app.rs` が `polling` クレートの `Poller` で stdin と PTY master fd を監視することで駆動される (レベルトリガー)。

- **PTY master 読み取り可能** → `read()` → `BlockEngine::feed_output` を通す → Normal モードではクリーンバイトを stdout へ書き出す。
- **stdin 読み取り可能** → `read()` → Normal モードでは `Ctrl+]` 以外のバイトをすべて PTY master へ書き出す。`Ctrl+]` はモード切り替えのトリガー。

### リサイズ

`SIGWINCH` が ptylenz プロセスに届くとシグナルハンドラがフラグを立てる。メインループがフラグを確認し、stdout から `TIOCGWINSZ`、PTY master へ `TIOCSWINSZ`、子シェルへ `SIGWINCH` を送る。vt100 シャドウパーサも同じサイズに更新する。

---

## OSC 133 パーサ (`block.rs — OscParser`)

### なぜ OSC 133 か

ブロック検出にはコマンドの出力の開始と終了を正確に知る必要がある。OSC 133 プロトコル (iTerm2 / Warp / VS Code Terminal 互換) はシェルが発するエスケープシーケンスでそれを提供する:

| シーケンス | 意味 |
|----------|------|
| `\e]133;A\a` | プロンプト開始 |
| `\e]133;C\a` | コマンド実行開始 (出力ここから) |
| `\e]133;D;N\a` | コマンド終了、終了コード N |
| `\e]133;E;text\a` | コマンドテキスト (ブロックタイトル) |

### 5 状態機械

```
Normal
  │ \x1b
  ▼
Escape
  │ ']'          │ その他 → emit(\x1b + byte) → Normal
  ▼
OscStart
  │ (任意バイト)
  ▼
OscBody ──────────────────────────────────────────────────────►
  │ \x07 (BEL)  →  decode_osc(buf)                            │
  │               OSC 133 → Event を emit、→ Normal            │
  │               それ以外 → 元のバイトを再 emit、→ Normal      │
  │ \x1b (ESC)  →  decode_osc(buf)
                  OSC 133 → Event を emit、→ OscStSwallow
                  それ以外 → 元のバイトを再 emit、→ Normal

OscStSwallow
  │ '\\' → Normal   (ST ターミネータを消費)
  │ その他 → byte を emit → Normal
```

### パススルー保証

`\e]133;*` シーケンスのみ消費する。その他の OSC — タイトル設定 (`\e]0;title\a`)、ハイパーリンク (`\e]8;...`)、クリップボード (`\e]52;...`)、カラークエリ (`\e]11;?\e\\`) — はすべて元のまま再 emit する。`mc` のような ncurses アプリは `setupterm()` 中にターミナルの色をクエリするため、応答を黙って捨てると描画が変わる。

### インターリーブドチャンク API

`OscParser::parse` は `Vec<ParseChunk>` を返す (各チャンクは `Bytes(Vec<u8>)` か `Event(OscEvent)`)。呼び出し側はこれを順番に処理する。速いコマンドではコマンドの末尾バイト・`133;D`・次の `133;C` がひとつの `read()` に収まることが多く、バイトとイベントをまとめてしまうと出力が誤ったブロックに帰属してしまう。

---

## ブロックエンジン (`block.rs — BlockEngine`)

### ブロックのライフサイクル

```
OSC 133;C  →  新 Block を開く (self.current = Some(block))
生バイト    →  current.output に追記; cached_line_count を更新;
               vt100 シャドウにも tee (下記参照)
OSC 133;E  →  current.command をセット (133;E が 133;D の後に来る場合は
               最後に閉じたブロックにパッチ — bash は PROMPT_COMMAND から発行)
OSC 133;D  →  current を閉じる: exit_code・ended_at をセット、rendered_text を確定;
               line_count > 50 なら自動折り畳み; self.blocks に追加
OSC 133;A  →  current が存在すれば閉じる (D なしプロンプトエッジケース対応)
```

### cached_line_count

`Block.cached_line_count` は `append_clean` 呼び出しのたびに `\n` バイトを数えてインクリメントする。以前は `line_count()` が O(全出力サイズ) のスキャンを行い、claude や mc が数 MB のストリームを蓄積すると一覧表示が数秒かかっていた。これを O(1) に置き換えた。

### 検索

`BlockEngine::search(query)` は全完了ブロックと進行中のブロックを大文字小文字を区別せずに部分文字列検索する。`n`/`N` ナビゲーション用に `(block_id, 行番号, 行テキスト)` のタプルを返す。

### JSON エクスポート

`BlockEngine::export_json()` は [claude-session-replay](https://github.com/opaopa6969/claude-session-replay) 共通ログモデル形式でシリアライズする。各ブロックは `user` メッセージ (コマンドテキスト) と `assistant` メッセージ (出力) のペアになり、`exit_code` 拡張フィールドが付く。

---

## vt100 シャドウグリッド (`block.rs — per-block vt100::Parser`)

### なぜ必要か

vim・less・claude・mc などの TUI アプリはオルタネートスクリーン (`\e[?1049h` / `\e[?1049l`) を使用し、カーソル位置制御シーケンスで埋める。生バイトバッファから ANSI を単純に除去すると、部分的な上書きが混ざり読めなくなる。シャドウグリッドは画面の最終的な視覚状態を取得する。

### 仕組み

`CommandStart` 時に現在のターミナルサイズで `vt100::Parser` を新規作成する。`current.output` に追記されるすべてのバイトをこのパーサにも tee する。パーサのスクリーンが `alternate_screen() == true` を報告したとき、グリッドの内容を `screen().contents()` でスナップショットし、正規化して `last_alt_snapshot` と `current.rendered_text` の両方に保存する。

### CommandEnd より前にスナップショットする理由

TUI アプリは通常、コマンド終了直前にオルタネートスクリーンを退出する。`CommandEnd` まで待つとプライマリスクリーンが復元された状態しか見えず、見たいフレームを失う。代わりにオルタネートスクリーンが活性な間、フィード毎にスナップショットを更新し、最後の非空フレームを `finalize_rendered_text` で確定する。

### 正規化

`vt100::Screen::contents()` は各行をパーサのカラム幅までスペースでパディングする。`normalize_vt_snapshot` は各行を右トリムし、末尾の空白行を削除する。これをしないと、スナップショットがオーバーレイパネルより広い場合にパディングされた各行が 2 視覚行に折り返されてスクロール計算が狂う。

### CJK 全角ずれに関する注意

CJK 文字 (中国語・日本語・韓国語) は全角 (ダブルワイド) であり、各文字がターミナルセル 2 つを占める。`unicode-width` クレートが `UnicodeWidthChar::width()` で正確なカラム幅を提供する。ただし、ratatui オーバーレイが vt100 グリッドより狭い場合、CJK 文字を多く含む行がはみ出したり位置がずれたりする既知の表示問題がある。現時点での回避策はない。

### 行指向出力のフォールバック

ブロックがオルタネートスクリーンを使用しなかった場合、`rendered_text` は `None` のままとなり、`output_text()` は生バイトバッファから ANSI を除去したものにフォールバックする。このパスはグリッド高さを超えるスクロールバックを保持する — `cargo test` などが数百行を出力する場合に重要。

---

## ratatui TUI オーバーレイ (`tui_app.rs`)

### モデル

```
Mode::Normal
  - Ctrl+] 以外のすべてのキーストローク → proxy.write_input(bytes)
  - PTY 出力 → stdout (クリーンバイト)

Mode::Ptylenz { selected, view, search_input, last_search, status_message }
  - PTY 出力 → ブロックエンジンのみ; alt-screen UI には描画しない
  - ratatui はイベントループ毎 (最大 80 ms タイムアウト) に描画
  - Ctrl+] → Normal へ; 'q' / Esc → 同じ
```

### イベントループ

```
Poller::wait(80ms)
  ├─ PTY_KEY 読み取り可能 → proxy.read_output() → 必要なら stdout へ書き出し
  ├─ STDIN_KEY 読み取り可能 → handle_input()
  └─ SIGWINCH フラグ → リサイズ
claude_rx.try_recv() → ingest_claude_event()  (各 poll 前にドレイン)
Ptylenz モードなら → draw_ptylenz()
```

80 ms タイムアウトにより、I/O が発生しないとき (ブロックを閲覧中でシェルがアイドル状態のとき) も UI がレスポンシブに保たれる。

### リストビュー

全完了ブロックと進行中のブロックを ratatui `List` として描画する。各アイテムには折り畳みインジケータ (`▸`/`▾`)、ピン (`📌`)、ブロック ID、タイムスタンプ、行数、終了ステータス、コマンドテキストが含まれる。展開時は最大 200 行の出力本体 ("N more lines — press e to export" で打ち切り)。

### 詳細ビュー

1 ブロックを全画面表示し、2 種類の選択モードを持つ:

- **行選択** (`v`): `[anchor_row, cursor_row]` 内の全行を丸ごと選択
- **矩形選択** (`Ctrl+v`): anchor セルとカーソルセルで囲む矩形 — vim の `Ctrl-v` モデル

どちらのモードも ratatui 描画パスで選択セルをハイライトし、`y` でヤンクする。

### ステータスバー

下部の 1 行バー。コンテキストに応じたヘルプ文字列 (キーヒント) か、アクション後の一時メッセージ (例: "copied block #7 (4316 chars)") を表示する。メッセージは次のキー処理の先頭でクリアされるため、1 描画サイクルだけ見える。

---

## Claude Code セッション統合 (`claude_feeder.rs`)

### ファイルレイアウト

Claude Code はセッションログを以下に書き込む:

```
~/.claude/projects/<cwd-slug>/<session-id>.jsonl
```

`<cwd-slug>` は絶対パスの `/` を `-` に置換したもの。  
例: `/home/opa/work/ptylenz` → `-home-opa-work-ptylenz`

### ウォッチループ

`spawn_watcher(cwd)` はバックグラウンドスレッドを起動し:

1. プロジェクトディレクトリを 400 ms ごとにポーリング。
2. mtime で最新の `.jsonl` を探す。
3. ファイル切り替え時: `ClaudeEvent::SessionStarted` を emit し、EOF までシーク (起動時の履歴は再生しない)。
4. 同じファイルの場合: 前回のオフセットからシークし、新しい完全な行を読む。
5. 各行を `decode_line` でデコードし、`user`/`assistant` エントリは `ClaudeEvent::Turn` を emit、その他のレコードタイプは無視。

inotify/kqueue の代わりにポーリングを使う理由: ptylenz が SSH 越しのホームディレクトリで動作する場合、ファイルシステム通知がネットワーク経由で伝わらないことがある。

### notify クレートについて

`Cargo.toml` に `notify = "6"` が含まれているが、現在の実装では使用されていない。詳細は [docs/decisions/notify-dead-dep.md](decisions/notify-dead-dep.md) を参照。

### ClaudeEvent の取り込み

メインループは各 `Poller::wait` の前に `claude_rx.try_recv()` をドレインする。`BlockEngine::ingest_claude_event` が `source = BlockSource::ClaudeTurn` を持つ `Block` を合成する。これらのブロックはシェルブロックと同じ時系列リストに表示される。

---

## シェル統合の詳細

各シェルのセットアップは [docs/shell-integration.md](shell-integration.md) (日本語版: [shell-integration-ja.md](shell-integration-ja.md)) を参照。

bash インテグレーションはラッパー rcfile 経由で注入される:

```bash
# ptylenz が自動生成 — 削除しても問題なし
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"

__ptylenz_precmd() {
    local __ptylenz_ec=$?
    printf '\e]133;D;%d\a' "$__ptylenz_ec"          # コマンド終了 + 終了コード
    local __ptylenz_last
    __ptylenz_last=$(HISTTIMEFORMAT='' history 1 2>/dev/null \
        | sed -E 's/^[[:space:]]*[0-9]+[[:space:]]*//')
    if [ -n "$__ptylenz_last" ]; then
        printf '\e]133;E;%s\a' "$__ptylenz_last"    # コマンドテキスト
    fi
}
PROMPT_COMMAND='__ptylenz_precmd'
PS0='\[\e]133;C\a\]'
case "$PS1" in
  *'133;A'*) ;;
  *) PS1='\[\e]133;A\a\]'"$PS1" ;;
esac
```

`DEBUG` トラップではなく `PROMPT_COMMAND` を選択する理由: `DEBUG` トラップはサブコマンドを呼び出すすべての関数の内部でネストし、偽の 133;C マーカーを発生させる。

`PROMPT_COMMAND` の代入は既存の値を上書きする。詳細は [docs/decisions/prompt-command-strategy.md](decisions/prompt-command-strategy.md) を参照。

---

## 依存クレート一覧

| クレート | 用途 |
|---------|------|
| `ratatui 0.29` | TUI レンダリング |
| `crossterm 0.28` | alt-screen 切り替え、カーソル表示/非表示 |
| `nix 0.29` | `openpty`、`fork`、`execvp`、`signal`、`waitpid` |
| `libc 0.2` | `ioctl`、`TIOCGWINSZ`、`TIOCSWINSZ`、`tcgetattr`、raw モード |
| `polling 3` | fd ポーリング (stdin + PTY master)、レベルトリガー |
| `vt100 0.15` | alt-screen TUI キャプチャ用シャドウグリッド |
| `regex 1` | 将来のプロンプトパターンフォールバック用バイトレベル正規表現 |
| `unicode-width 0.2` | CJK などのワイド文字のカラム幅 |
| `unicode-segmentation 1` | ツールユース描画のグラフェムクラスタ安全な切り詰め |
| `serde + serde_json 1` | JSONL デコード (Claude フィーダ) と JSON エクスポート |
| `anyhow 1` | エラー伝播 |
| `chrono 0.4` | ブロックタイムスタンプ |
| `notify 6` | **未使用** — 将来の inotify/kqueue パス用に残存; decisions/ を参照 |
