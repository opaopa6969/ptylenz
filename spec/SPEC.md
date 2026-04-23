# ptylenz — Technical Specification

**Version**: 0.1 (Phase 1 MVP)
**Date**: 2026-04-19
**Repository**: https://github.com/opaopa6969/ptylenz
**Language**: Rust 2021 edition
**Binary**: `ptylenz` (single binary, no runtime deps)

---

## 1. 概要 (Overview)

### 1.1 背景と動機

現代のターミナルエミュレータはシェルセッション全体を1つのスクロール可能な文字ストリームとして扱う。このモデルでは以下の操作が難しいまたは不可能である:

- あるコマンドの出力だけを選択してコピーする
- 以前実行したコマンドの出力をキーワード検索する
- コマンドの終了コードと実行時刻を対応付けて記録する
- Claude Code セッション (AI 操作) とシェル操作を統一したタイムラインで閲覧する
- 長大な出力を持つコマンドを折り畳んで他のブロックとともに一覧する

ptylenz はこの問題を、シェルのセッションを **ブロック (Block)** の列として構造化することで解決する。1 ブロック = 1 コマンド実行 + その出力という単位に分割し、TUI オーバーレイで閲覧・検索・コピー・エクスポートを提供する。

### 1.2 アーキテクチャ概要

ptylenz は **PTY プロキシ** と **ratatui TUI オーバーレイ** の 2 つの主要コンポーネントで構成される。

```
ユーザーの端末 (ターミナルエミュレータ)
        │ stdin
        ▼
┌──────────────────────────────────────────────────────┐
│                    ptylenz プロセス                    │
│                                                      │
│  stdin ──→ handle_input() ──→ proxy.write_input()   │
│                                     │                │
│                              PTY master fd           │
│                                     │                │
│                             ┌───────────────┐        │
│                             │   bash 子プロセス│        │
│                             │  (PTY slave)   │        │
│                             └───────────────┘        │
│                                     │ stdout         │
│                             proxy.read_output()      │
│                                     │                │
│                            BlockEngine.feed_output() │
│                           ┌─────────┴──────────┐    │
│                           │       OscParser     │    │
│                           │  (5-state machine)  │    │
│                           └──────────┬──────────┘    │
│                     clean bytes      │ OSC events    │
│                          │           │               │
│                    stdout へ書き出し  ブロック蓄積     │
│                    (Normal mode)    (Block History)  │
│                                     │               │
│                              ratatui TUI overlay     │
│                              (Ctrl+] で表示)          │
└──────────────────────────────────────────────────────┘
        │ stdout
        ▼
ユーザーの端末 (ターミナルエミュレータ)
```

加えて、バックグラウンドスレッドが Claude Code の JSONL セッションログを監視し、`ClaudeEvent` を mpsc チャネル経由でメインループへ送る。メインループが `ingest_claude_event()` で BlockEngine へ注入することで、Shell ブロックと ClaudeTurn ブロックが時系列順に並列表示される。

### 1.3 動作モード

| モード | 説明 |
|--------|------|
| **Normal** | ptylenz は完全透過プロキシ。ユーザー入力をそのまま bash へ転送し、bash 出力 (OSC 133 除去済み) をそのまま端末へ転送する。唯一インターセプトするキーは `Ctrl+]` (0x1d) のみ。 |
| **Ptylenz** | `Ctrl+]` で切り替わる。ratatui が alt-screen を占有し、ブロック一覧または Detail View を表示する。bash は引き続き動作するが、その出力は画面に書き込まれない (BlockEngine への蓄積は継続)。 |

### 1.4 対象プラットフォーム

| プラットフォーム | サポート状況 |
|-----------------|------------|
| Linux (x86_64, aarch64) | サポート |
| macOS (x86_64, Apple Silicon) | サポート |
| Windows | 非サポート。`openpty(2)`, `fork(2)`, `setsid(2)` が存在しないため |
| FreeBSD / OpenBSD | 未検証 (nix クレートは対応しているが CI なし) |

### 1.5 設計原則

1. **透過性ファースト**: Normal モードでは ptylenz が存在することをユーザーが気づかない。端末の全機能 (色、ハイパーリンク、クリップボード OSC 等) を維持する。
2. **シンプルな永続化**: in-memory のみ。DB やファイルシステムへの自動書き込みはしない。エクスポートは手動操作。
3. **外部依存の最小化**: ポーリングによる監視、自前 base64、自前 JSON エスケープで `notify`, `base64`, `serde_json` 以外の依存を増やさない。
4. **フェーズド実装**: Phase 1 で動くものを作り、Phase 2–4 で段階的に拡張する。

---

## 2. 機能仕様 (Feature Specification)

### 2.1 機能全体像

```
┌─────────────────────────────────────────────────────┐
│  Phase 1: PTY + OSC 133 + TUI 基本 (実装済み v0.1)  │
│                                                     │
│  ┌─────────────────┐  ┌──────────────────────────┐  │
│  │  PTY Proxy       │  │  Block Engine             │  │
│  │  - openpty/fork  │  │  - OscParser (5 states)   │  │
│  │  - bash rcfile   │  │  - Block lifecycle         │  │
│  │  - SIGWINCH      │  │  - vt100 shadow grid       │  │
│  │  - polling       │  │  - search / export / pin  │  │
│  └─────────────────┘  └──────────────────────────┘  │
│                                                     │
│  ┌─────────────────────────────────────────────┐   │
│  │  TUI Overlay (ratatui)                       │   │
│  │  - List View (j/k/g/G/Enter/v/y/e/p//)      │   │
│  │  - Detail View (hjkl/v/Ctrl+v/y/Y)          │   │
│  │  - Search (/, n/N)                           │   │
│  │  - Clipboard (OSC 52 + xclip/pbcopy)         │   │
│  └─────────────────────────────────────────────┘   │
│                                                     │
├─────────────────────────────────────────────────────┤
│  Phase 2: Claude Code JSONL 統合 (実装済み v0.1)    │
│                                                     │
│  ┌─────────────────────────────────────────────┐   │
│  │  claude_feeder                               │   │
│  │  - ~/.claude/projects/<slug>/*.jsonl tail    │   │
│  │  - 400ms polling (no inotify)                │   │
│  │  - ClaudeEvent → BlockEngine                 │   │
│  │  - tool_use サマリー表示                     │   │
│  └─────────────────────────────────────────────┘   │
│                                                     │
├─────────────────────────────────────────────────────┤
│  Phase 3: ブロックエンジン拡張 (計画中)              │
│  - 正規表現検索                                     │
│  - ブロック間 diff                                  │
│  - 外部コマンドへのパイプ                           │
│  - ブロック単位再実行                               │
│                                                     │
├─────────────────────────────────────────────────────┤
│  Phase 4: vt100 高度化 (計画中)                     │
│  - 256色/Truecolor の Detail View 転写             │
│  - CJK 全角幅補正                                   │
│  - ブロックリプレイ                                 │
└─────────────────────────────────────────────────────┘
```

### 2.2 Phase 1 詳細機能リスト

#### PTY プロキシ

- `/dev/ptmx` で openpty し PTY マスター/スレーブペアを生成する
- 現在の端末ウィンドウサイズ (`TIOCGWINSZ`) を取得し、PTY オープン時に初期サイズとして設定する。これにより、ncurses アプリが setupterm 時に正しい LINES/COLUMNS を取得できる
- 子プロセス (bash) を fork し、PTY スレーブ側の stdin/stdout/stderr にアタッチして起動する
- `execvp` で bash を `--rcfile <wrapper> -i` で起動する (インタラクティブモード必須)
- `$PTYLENZ=1`, `$PTYLENZ_VERSION=<version>` を子プロセスの環境に設定する
- SIGWINCH を受信した際に PTY ウィンドウサイズを更新し、SIGWINCH を子プロセスへ転送する
- 子プロセス終了を `waitpid(WNOHANG)` でポーリング検出し、メインループを終了する
- `Drop` 実装で子プロセスへ SIGHUP を送信する

#### Shell Integration 自動注入

- `$TMPDIR/ptylenz-rc-<PID>.sh` にラッパー rcfile を書き出す
- `~/.bashrc` を先にソースし、その後 ptylenz integration を注入する
- PROMPT_COMMAND / PS0 / PS1 に OSC 133 シーケンス 4 種を組み込む
- PS1 への注入はべき等チェック付き (`*'133;A'*` の case 検査)

#### BlockEngine

- PTY ストリームをリアルタイムで `OscParser` に通して OSC 133 マーカーを検出する
- OSC 133 以外のシーケンスは clean バイトとして端末へ再送する (タイトル、ハイパーリンク、カラークエリ等)
- ブロック単位で `output: Vec<u8>` を蓄積する
- 各ブロックに vt100 シャドウパーサーをアタッチし、alt-screen フレームをサンプリングする
- `cached_line_count` により `line_count()` を O(1) で提供する
- `search(query)` で全ブロックをキーワード検索し `(block_id, line_num, line_text)` を返す
- `export_json()` で common log model フォーマットの JSON を生成する
- `toggle_collapse(id)` / `toggle_pin(id)` でブロックの UI 状態を切り替える
- `get_block_by_index(index)` で完了ブロック + 進行中ブロックを統一インデックスで参照する

#### TUI オーバーレイ

詳細は§7参照。

#### Claude Code 統合

詳細は§5.4参照。

### 2.3 既知の制限 (v0.1)

| 制限 | 詳細 |
|------|------|
| bash のみ自動統合 | zsh, fish は手動スニペット (§8 参照) |
| CLI オプションなし | `--shell`, `--no-integrate` は未実装 |
| ブロック上限なし | 長時間セッションで数百 MB になりうる |
| CJK 全角ずれ | ratatui の幅計算と unicode-width の不一致 (§10.4) |
| Windows 非対応 | §1.4 参照 |
| ブロック永続化なし | セッション終了で消える (エクスポートで手動保存可) |

---

## 3. データ永続化層 (Data Persistence)

### 3.1 方針: In-memory ブロック履歴

ptylenz は **外部ストレージを一切使用しない**。ブロック履歴はプロセスのヒープ上にのみ存在し、ptylenz 終了とともに消える。

### 3.2 根拠

- シェルセッションは本質的に揮発的。セッション終了後の履歴は `history`, `~/.bash_history`, tmux scrollback 等の既存ツールで対応可能。
- 永続化はデータベース・ファイルフォーマット・スキーママイグレーション等の複雑性を生む。Phase 1 では採用しない。
- エクスポート機能 (`e` キー) により、保存が必要なブロックを任意のタイミングで JSON として書き出せる。

### 3.3 ブロック履歴のデータ構造

```rust
pub struct BlockEngine {
    blocks: Vec<Block>,       // 完了済みブロックの配列 (ID 昇順)
    next_id: usize,           // 次のブロックに割り当てる ID
    current: Option<Block>,   // 実行中のブロック (あれば)
    osc_parser: OscParser,    // OSC 133 パーサーステートマシン
    prompt_pattern: Option<Regex>,  // フォールバック用プロンプトパターン
    claude_turn_counters: HashMap<String, usize>,  // セッション別ターンカウンタ
    vt_parser: Option<vt100::Parser>,   // 現在ブロックのシャドウパーサー
    term_rows: u16,
    term_cols: u16,
    last_alt_snapshot: Option<String>,  // 最後の alt-screen スナップショット
}
```

`blocks: Vec<Block>` は単純な配列として保持する。検索は O(n) の線形スキャンであり、数千ブロック規模での通常使用では問題ない。

### 3.4 Block 構造体

```rust
pub struct Block {
    pub id: usize,                      // 単調増加の一意 ID
    pub command: Option<String>,        // OSC 133;E から取得したコマンドテキスト
    pub output: Vec<u8>,                // 生の PTY 出力バイト列
    pub exit_code: Option<i32>,         // OSC 133;D から取得
    pub started_at: DateTime<Local>,    // CommandStart 時刻
    pub ended_at: Option<DateTime<Local>>,  // CommandEnd 時刻
    pub collapsed: bool,                // UI での折り畳み状態
    pub pinned: bool,                   // ピン留め状態
    pub source: BlockSource,            // Shell か ClaudeTurn か
    pub rendered_text: Option<String>,  // vt100 スナップショット (alt-screen のみ)
    pub cached_line_count: usize,       // \n バイト数のキャッシュ
}
```

`BlockSource` は Shell ブロックと ClaudeTurn ブロックを区別するための enum:

```rust
pub enum BlockSource {
    Shell,
    ClaudeTurn {
        session_id: String,
        role: String,       // "user" | "assistant"
        turn_index: usize,  // セッション内の連番
        tool_uses: Vec<ToolUse>,
    },
}
```

### 3.5 エクスポートフォーマット

`e` キーまたは `export_json()` で生成される JSON は common log model に準拠する:

```json
{
  "source": "ptylenz-20260419-123456.json",
  "agent": "ptylenz",
  "messages": [
    {
      "role": "user",
      "text": "ls -la",
      "tool_uses": [],
      "tool_results": [],
      "thinking": [],
      "timestamp": "2026-04-19T10:23:45+09:00"
    },
    {
      "role": "assistant",
      "text": "total 128\ndrwxr-xr-x ...",
      "tool_uses": [],
      "tool_results": [],
      "thinking": [],
      "timestamp": "2026-04-19T10:23:46+09:00",
      "exit_code": 0
    }
  ]
}
```

JSON エスケープは `json_escape()` 関数で自前実装する。対応する文字: `"`, `\`, `\n`, `\r`, `\t`, `\b` (`\x08`), `\f` (`\x0c`), `U+0000`–`U+001F` (4 桁 Unicode エスケープ)。

### 3.6 ブロック上限と将来計画

v0.1 ではブロック数の上限を設けていない。各ブロックの `output` フィールドが生バイト列を保持するため、長時間セッションでは数百 MB に達することがある。

将来的に検討するアプローチ:
- リングバッファによる古いブロックの逐次破棄 (pinned ブロックは除外)
- 大きなブロックの output をディスクへスワップ
- ブロック数または総メモリ使用量の上限設定 (設定ファイルで調整可能に)

---

## 4. ステートマシン (State Machines)

### 4.1 OSC 133 パーサー (OscParser) — 5 状態

`OscParser` は PTY バイトストリームを 1 バイトずつ処理し、OSC 133 マーカーを検出する。5 つの状態を持つ。

#### 状態定義

```rust
enum ParseState {
    Normal,         // 通常バイト転送中
    Escape,         // \x1b を受信した
    OscStart,       // \x1b] を受信した
    OscBody,        // OSC ボディを buf に蓄積中
    OscStSwallow,   // OSC 133 の ESC 終端後、\ を飲み込む待機中
}
```

#### 状態遷移の詳細

**Normal**
- `\x1b` → `Escape` (pending には追加しない)
- その他 → `Normal` (pending へ push)

**Escape**
- `]` → `OscStart` (buf をクリア)
- その他 → `Normal` (`\x1b` + byte を pending へ push)

  注: ESC の直後に `]` 以外が来た場合は CSI シーケンスや単体 ESC として扱い、`\x1b` と当該バイトを両方 clean バイトとして通過させる。

**OscStart**
- 任意バイト → `OscBody` (buf へ push)

**OscBody**
- `\x07` (BEL) → `finish_osc()` → `Normal`
- `\x1b` (ST 先頭) → `finish_osc()` → `OscStSwallow`
- その他 → `OscBody` (buf へ push)

  `finish_osc()` の動作:
  - buf の内容を `decode_osc()` でパース
  - OSC 133 として認識できた場合: pending を Bytes チャンクとして出力し、Event チャンクを追加する (OSC 133 バイトは clean ストリームに含めない)
  - 認識できなかった場合: `\x1b]` + payload + terminator を pending に push し、clean ストリームとして通過させる (非 133 OSC の透過)

**OscStSwallow**
- `\` → `Normal` (ST の `\` を飲み込む。clean ストリームへ流さない)
- `\x1b` → `Escape` (次の ESC シーケンスへ)
- その他 → `Normal` (byte を pending へ push)

#### 状態遷移図

```
                    ┌──────────────────────────────────┐
                    │ その他バイト → pending            │
                    ▼                                  │
          ┌──────────────────┐                        │
          │     Normal       │ ←──────────────────────┘
          └──────────────────┘
                    │ \x1b
                    ▼
          ┌──────────────────┐
          │     Escape       │
          └──────────────────┘
          │ ]              │ その他
          ▼                ▼ (\x1b + byte → pending, Normal)
 ┌──────────────────┐
 │    OscStart      │
 └──────────────────┘
          │ any byte → buf
          ▼
 ┌──────────────────┐
 │    OscBody       │ ←────── buf へ push
 └──────────────────┘
    │ \x07 (BEL)     │ \x1b
    ▼                ▼
finish_osc()   finish_osc()
    │                │
    ▼                ▼
 Normal      ┌──────────────────┐
             │  OscStSwallow    │
             └──────────────────┘
                  │ \      │ その他     │ \x1b
                  ▼        ▼           ▼
               Normal    Normal      Escape
```

#### ParseChunk と時系列保持

`parse()` の戻り値:

```rust
enum ParseChunk {
    Bytes(Vec<u8>),    // clean バイト列
    Event(OscEvent),   // OSC 133 イベント
}
```

チャンクは **ストリーム内の到着順** を保持する。これにより BlockEngine は、1 回の `read()` 内に `CommandStart` と `CommandEnd` が同時に含まれる場合でも、各バイト群を正しいブロックへ帰属させることができる。

例: `"output tail\x1b]133;D;0\x07\x1b]133;A\x07new prompt"` という読み取りが発生した場合:
```
[Bytes("output tail"), Event(CommandEnd{0}), Event(PromptStart), Bytes("new prompt")]
```
BlockEngine はチャンクを順番に処理するため、"output tail" は前のブロックへ、"new prompt" は PromptStart 後の状態で処理される。

#### OSC 133 イベント種別

```rust
pub enum OscEvent {
    PromptStart,                     // \e]133;A\a
    CommandStart,                    // \e]133;C\a
    CommandText(String),             // \e]133;E;text\a
    CommandEnd { exit_code: i32 },   // \e]133;D;N\a
}
```

`decode_osc()` のルール:
- `"133;"` で始まらない → `None` (非 133 OSC、透過)
- `"133;A"` → `PromptStart`
- `"133;C"` → `CommandStart`
- `"133;D;N"` → `CommandEnd { exit_code: N.parse().unwrap_or(-1) }`
- `"133;E;text"` → `CommandText(text.to_string())`
- その他の `"133;"` 系 → `None` (将来の OSC 133 拡張のために透過)

### 4.2 ブロックライフサイクルステートマシン

#### ブロックの状態

ブロックは BlockEngine の `current` フィールド (進行中) または `blocks` 配列 (完了) に存在する。

```
(存在しない)
      │
      │ OscEvent::CommandStart
      │   → Block::new(next_id)
      │   → vt_parser = Some(vt100::Parser::new(...))
      │   → last_alt_snapshot = None
      ▼
┌────────────────────────────────────────────┐
│  OPEN (current: Some(block))               │
│                                            │
│  Bytes 受信:                               │
│    block.output.extend(bytes)              │
│    block.cached_line_count += newlines     │
│    vt_parser.process(bytes)                │
│    if alt_screen active:                   │
│      last_alt_snapshot = snap              │
│      block.rendered_text = Some(snap)      │
│                                            │
│  CommandText(cmd) 受信:                    │
│    block.command = Some(cmd)               │
└────────────────────────────────────────────┘
      │
      │ OscEvent::CommandEnd(exit_code) または PromptStart
      │   → block.exit_code = Some(exit_code)
      │   → block.ended_at = Some(now)
      │   → finalize_rendered_text()
      │   → if line_count > 50: collapsed = true
      │   → blocks.push(block)
      │   → vt_parser = None
      │   → last_alt_snapshot = None
      ▼
┌────────────────────────────────────────────┐
│  CLOSED (blocks: Vec<Block>)               │
│                                            │
│  toggle_collapse(id) で collapsed 変更可    │
│  toggle_pin(id) で pinned 変更可           │
└────────────────────────────────────────────┘
```

#### 自動折り畳みルール

`line_count() > 50` を超えるブロックは自動的に `collapsed = true` に設定される。これは CommandEnd 時と ClaudeTurn ingest 時の両方で適用される。

#### ClaudeTurn ブロックのライフサイクル

`ingest_claude_event(ClaudeEvent::Turn {...})` で呼ばれると:
1. `claude_turn_counters` でセッション別ターンカウンタをインクリメント
2. `render_turn(role, text, tool_uses)` でブロックの出力テキストを生成
3. OPEN 状態を経ずに即座に CLOSED 状態のブロックを `blocks.push()` する
4. `line_count > 50` の場合は自動折り畳み

#### SessionStarted イベント

`ClaudeEvent::SessionStarted` はブロックを生成しない。`claude_turn_counters` に新しいセッション ID を登録するのみ (カウンタを 0 で初期化)。

### 4.3 TUI モードステートマシン

#### Mode enum

```rust
enum Mode {
    Normal,
    Ptylenz {
        selected: usize,              // 選択中のブロックインデックス
        view: PtylenzView,            // List または Detail
        search_input: Option<String>, // 検索入力中の場合 Some(文字列)
        last_search: Option<SearchState>, // 最後の検索結果
        status_message: Option<String>,  // 一時ステータス (1 キーで消える)
    },
}

enum PtylenzView {
    List,
    Detail(DetailState),
}
```

#### モード遷移

```
Normal
  │ Ctrl+] (0x1d)
  │   → enter_ptylenz()
  │     → execute!(EnterAlternateScreen)
  │     → Terminal::new()
  │     → term.hide_cursor()
  ▼
Ptylenz::List
  │         │               │
  │ Ctrl+]  │ v (ブロック    │ / (検索
  │ q / Esc │  選択中)       │  モード)
  │         ▼               ▼
  │  Ptylenz::Detail   search_input = Some("")
  │    │                    │ Enter → 実行 / Esc → キャンセル
  │    │ q / Esc(選択なし)   └──→ search_input = None
  │    └──→ List             (last_search 更新)
  │
  ▼ leave_ptylenz()
    → term.show_cursor()
    → execute!(LeaveAlternateScreen)
Normal
```

`Ctrl+]` は **どのサブ状態からでも** `leave_ptylenz()` を呼ぶ。Detail View 中、検索入力中であっても即座に Normal モードへ戻る。

#### status_message のライフサイクル

`handle_ptylenz_bytes()` の冒頭で `*status_message = None` をセットする。ハンドラが新しいメッセージをセットした場合はそれが表示され、次のキー入力でリセットされる。これにより「コピー完了」などの一時メッセージが正確に 1 ドローサイクル表示される。

### 4.4 検索ステートマシン

```
(last_search: None, search_input: None)
    │ / キー
    ▼
(search_input: Some(""))
    │ 文字入力 → query 更新
    │ Backspace → query から末尾削除
    │ Esc → search_input = None (last_search は変えない)
    │ Enter
    │   → results = proxy.blocks().search(query)
    │   → last_search = Some(SearchState { query, results, result_index: 0 })
    │   → selected = 最初のヒットのブロックインデックス
    │   → search_input = None
    ▼
(last_search: Some(state), search_input: None)
    │ n → result_index = (result_index + 1) % len
    │ N → result_index = (result_index - 1 + len) % len
    │     selected = ヒットのブロックインデックス
    │ / → 新しい search_input を開始 (last_search は保持)
    └── q / Esc / Ctrl+] → Normal モードへ (last_search は破棄)
```

---

## 5. ビジネスロジック (Business Logic)

### 5.1 PROMPT_COMMAND 上書き戦略

ptylenz が bash を起動する際、ラッパー rcfile を `$TMPDIR/ptylenz-rc-<PID>.sh` に書き出す。構造:

```
1. [ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"
2. ptylenz shell integration (PROMPT_COMMAND 上書き, PS0, PS1 先頭追加)
```

PROMPT_COMMAND は **追記でなく上書き** する。詳細な根拠は `docs/decisions/prompt-command-strategy.md` に記録されている。

#### 上書きを選択した理由

**追記 (`${PROMPT_COMMAND:+$PROMPT_COMMAND; }__ptylenz_precmd`) の問題点**:

1. **順序依存**: 既存の PROMPT_COMMAND が `$?` を読む場合、ptylenz の precmd が先に `$?` をキャプチャしてしまう。追記すると ptylenz が先頭になるため既存関数が正しい終了コードを取得できない。上書きは ptylenz の precmd のみが動作するため問題が発生しない。

2. **重複蓄積**: 同じシェル内で ptylenz を再起動した場合、追記すると `__ptylenz_precmd` が 2 回以上チェーンされる。

3. **低リスク**: ラッパー rcfile は `~/.bashrc` を先にソースし、その後で PROMPT_COMMAND を設定する。通常の `~/.bashrc` が設定した PROMPT_COMMAND は ptylenz が上書きする時点で既に評価済みである。

#### 失敗するケース

`~/.bashrc` が末尾で別のファイル (`company-dotfiles.sh` 等) をソースし、そのファイルが PROMPT_COMMAND を再設定する場合、ptylenz の設定が上書きされる。この場合の修正方法は `docs/shell-integration.md` に記載している。

#### DEBUG トラップを使用しない理由

| 問題 | 詳細 |
|------|------|
| **サブシェル再帰** | `__ptylenz_precmd` 内の `history 1` が `$(...)` 経由で実行される。DEBUG トラップはこの内部コマンドにも発火し、spurious な `133;C` イベントが生成される |
| **排他性** | bash は 1 つの DEBUG トラップしか持てない。既存の DEBUG トラップを上書きすると、`set -x` や profiler との干渉が生じる |
| `extdebug` の副作用 | `$BASH_COMMAND` に展開前のコマンド文字列を取得するには `set -o extdebug` が必要だが、これは `return` の挙動を変える副作用がある |

#### PS0 だけでコマンドテキストを取得できない理由

PS0 は bash がコマンドを実行する直前 (ReadLine が行を読み込んだ後、execve の前) に展開・表示される。この時点では bash はコマンドを履歴に記録していない。PS0 内で `history 1` を呼ぶと **1 つ前の** コマンドが返る。

`133;E` は PROMPT_COMMAND 内 (コマンド完了後) に `history 1` を呼ぶことで **直前に実行したコマンド** を正確に取得する。

### 5.2 CommandText の遅延帰属 (Late Attribution)

bash integration では OSC 133 シーケンスの到着順が iTerm2 モデルと異なる:

| モデル | 133;E の到着タイミング |
|--------|----------------------|
| iTerm2 / Warp | 133;A と同時 (プロンプト表示時) |
| ptylenz bash | 133;D の直後 (PROMPT_COMMAND から) |

BlockEngine の `handle_event(CommandText)` は両順序を吸収する:

```rust
OscEvent::CommandText(cmd) => {
    if let Some(ref mut block) = self.current {
        // 133;C の後に 133;E が届いた (iTerm2 互換モード)
        block.command = Some(cmd.clone());
    } else if let Some(last) = self.blocks.last_mut() {
        // 133;D の後に 133;E が届いた (ptylenz bash integration)
        if last.command.is_none() {
            last.command = Some(cmd.clone());
        }
    }
    // いずれも当てはまらない場合は無視 (初回プロンプト前の 133;E)
}
```

### 5.3 vt100 シャドウグリッド戦略

#### 問題

alt-screen を使用する TUI アプリ (claude, vim, less, mc, htop 等) はカーソル位置指定 ESC シーケンスで画面を構築する。生バイト列を `strip_ansi()` で処理しても意味のあるテキストが得られない。例:

```
\e[?1049h\e[2J\e[H\e[1;1HHELLO\e[2;1HWORLD\e[5;40HGOODBYE
```

これを strip_ansi すると "HELLOWORLDGOODBYE" になり、レイアウトが完全に失われる。

#### 解決策: リアルタイムサンプリング

各ブロックに `vt100::Parser` をアタッチし、alt-screen が有効な間すべてのバイトをシャドウパーサーへ流す。`alternate_screen()` が `true` の間、毎バイト処理後に `screen().contents()` を取得し `last_alt_snapshot` に保存する。

```
feed_output(bytes):
  for chunk in osc_parser.parse(bytes):
    match chunk:
      Bytes(b) → append_clean(b)
      Event(e) → handle_event(e)

append_clean(bytes):
  block.output.extend(bytes)              // 生バイト列の保持 (scrollback 用)
  cached_line_count += count('\n', bytes)
  vt_parser.process(bytes)               // シャドウグリッド更新
  if vt_parser.screen().alternate_screen():
    snap = normalize_vt_snapshot(screen.contents())
    if snap not empty:
      block.rendered_text = Some(snap)   // ライブ更新
      last_alt_snapshot = Some(snap)     // finalize 用保持
```

#### CommandEnd でスナップショットを取れない理由

TUI アプリの典型的な終了シーケンス:
```
\e[?1049l   (alt-screen 終了)
\e]133;D;0\a (CommandEnd)
\e]133;E;vim\a (CommandText)
```

`\e[?1049l` の時点で `alternate_screen()` は `false` に戻る。CommandEnd ハンドラが `vt_parser.screen().alternate_screen()` を確認しても `false` であるため、その時点でスナップショットを撮っても空 (プライマリスクリーン) が返る。

これが **alt-screen が有効な間、全フレームをサンプリングし続ける** 理由である。

#### 全白フレームのスキップ

mc は `\e[?1049l` の直前に `\e[2J` (画面クリア) を送る。このフレームをスナップショットとして保存すると、ユーザーには空白のブロックが表示される。`normalize_vt_snapshot()` の最後にトリムした結果が空文字列の場合はスナップショットを更新しない:

```rust
if !snap.is_empty() {
    block.rendered_text = Some(snap.clone());
    last_alt_snapshot = Some(snap);
}
```

#### normalize_vt_snapshot

`vt100::Screen::contents()` はすべての行をパーサーの列幅 (`term_cols`) でパディングして返す。これをそのまま ratatui の `Paragraph` に渡すと、オーバーレイがターミナルより狭い場合に各行が折り返されて行数が2倍になる。

```rust
fn normalize_vt_snapshot(s: &str) -> String {
    let mut lines: Vec<&str> = s.split('\n').map(|l| l.trim_end()).collect();
    while lines.last().map_or(false, |l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}
```

処理: 各行末の空白を除去 → 末尾の空行を除去 → `\n` で再結合。

#### output_text() のフォールバック優先順位

1. `rendered_text` が `Some` → vt100 スナップショットを返す (alt-screen TUI 用)
2. `rendered_text` が `None` → `strip_ansi(String::from_utf8_lossy(&output))` (通常コマンド用)

通常コマンドでは `rendered_text = None` のまま保つことで、生バイト列をソースとするスクロールバックを保持する。vt100 シャドウグリッドの行数 (2000 行) を超える長い出力も損失なく表示できる。

### 5.4 claude_feeder ポーリング戦略

#### Claude Code のログ構造

```
~/.claude/
  projects/
    -home-opa-work-myproject/       ← cwd_slug("/home/opa/work/myproject")
      <session-uuid-1>.jsonl         ← 古いセッション
      <session-uuid-2>.jsonl         ← 最新セッション (ptylenz が監視)
```

`cwd_slug(path)` は `path.to_string_lossy().replace('/', "-")` で計算する。

#### ポーリングループ

```
loop:
  if !dir.exists(): sleep(500ms); continue

  newest = find newest .jsonl file by mtime

  if same as active:
    tail_once(path, &mut offset, &tx)
  else if newer file found:
    tx.send(SessionStarted { session_id, path })
    offset = file_len (新しいファイルの既存内容は skip)
    active = (new_path, offset)

  sleep(400ms)
```

**なぜ既存内容をスキップするか**: ptylenz 起動前の Claude セッション履歴を全部ブロックとして注入すると、数百ブロックが一瞬で生成されてノイズになる。ptylenz 起動後の新しいターンのみを対象とする。

**tail_once の動作**:
1. ファイルをオープンし `len < offset` なら offset = 0 にリセット (ローテーション検出)
2. `len == offset` なら変化なし、早期リターン
3. `SeekFrom::Start(offset)` でシーク
4. `BufReader` で 1 行ずつ読み、`\n` で終わらない行 (部分行) で停止
5. 完全な行を `decode_line()` でパース、`ClaudeEvent` を tx.send()
6. `offset` を読んだ分だけ進める

#### JSONL デコード

```rust
fn decode_line(line: &str) -> Option<ClaudeEvent>
```

- `serde_json` で `RawEntry` にデシリアライズする。未知フィールドは `#[serde(flatten)]` で吸収する
- `type_` が `"user"` または `"assistant"` でない場合は `None` を返す
- `message.content` が文字列なら直接テキストとして使用
- `message.content` が配列なら `type: "text"` ブロックを結合し、`type: "tool_use"` ブロックを `ToolUse` に変換する
- `tool_result`, `thinking` 等は現在無視する (Detail View での表示は将来計画)

#### tool_use の表示

`render_turn()` は各 `ToolUse` を 1 行で表示する:

```
→ Bash({"command":"ls -la"})
→ Read({"file_path":"/etc/hosts"})
```

ツール入力 JSON は最大 500 バイトで切り詰める (`append_truncated`)。切り詰め時は末尾に `…` を追加する。ZWJ 結合絵文字など複数バイトの書記素クラスタは分割しない (`unicode-segmentation` クレートで保護)。

### 5.5 append_truncated の実装

単純な `str.chars().take(n).collect()` ではなく、書記素クラスタ単位で切り詰める:

```rust
fn append_truncated(out: &mut String, s: &str, max_bytes: usize) {
    if s.len() <= max_bytes {
        out.push_str(s);
        return;
    }
    let mut taken = 0;
    for g in s.graphemes(true) {  // unicode-segmentation
        if taken + g.len() > max_bytes {
            break;
        }
        out.push_str(g);
        taken += g.len();
    }
    out.push('…');
}
```

これにより `👨‍👩‍👧` のような 18 バイトの ZWJ シーケンスが途中で切断されない。

---

## 6. API / 外部境界 (API / External Boundaries)

### 6.1 CLI インターフェース

**v0.1 の実装**:

```
$ ptylenz
```

引数なし。`$SHELL` 環境変数の値を shell パスとして使用する。`$SHELL` が未設定の場合は `/bin/bash` にフォールバックする。

**計画中のオプション** (Phase 3 以降):

```
ptylenz [OPTIONS]

オプション:
  --shell <PATH>      起動するシェルのパス (デフォルト: $SHELL)
  --no-integrate      shell integration を注入しない
                      (外部で OSC 133 を設定済みのシェル向け)
  --export <FILE>     ptylenz を起動せず JSON エクスポートして終了
                      (既存セッション JSON の再エクスポートに使用)
  --version           バージョン文字列を表示して終了
  -h, --help          ヘルプを表示して終了
```

### 6.2 公開 Rust API

ptylenz はライブラリクレートを提供しない。全機能はバイナリクレートとして実装される。ただし、モジュール構造は以下の通りで、内部テスト可能性のために `pub` アクセスを適切に管理する。

```
src/
  main.rs          エントリポイント: $SHELL 取得、App::new().run()
  block.rs         BlockEngine, Block, OscParser, BlockSource
  pty.rs           PtyProxy (openpty, fork, execvp, read/write)
  tui_app.rs       App, Mode, PtylenzView, DetailState, Selection
  claude_feeder.rs ClaudeEvent, ToolUse, spawn_watcher, decode_line
```

### 6.3 環境変数インターフェース

ptylenz が **読み取る** 環境変数:

| 変数 | 説明 |
|------|------|
| `$SHELL` | 起動するシェルのパス |
| `$HOME` | `~/.bashrc` のパス、`~/.claude/projects/` のベース |
| `$TMPDIR` / `os::temp_dir()` | ラッパー rcfile の書き出し先 |

ptylenz が **設定する** 環境変数 (子プロセスへ):

| 変数 | 値 | 説明 |
|------|----|------|
| `PTYLENZ` | `1` | 子プロセスが ptylenz 内で動作していることを検出可能にする |
| `PTYLENZ_VERSION` | `env!("CARGO_PKG_VERSION")` | バージョン情報 |

### 6.4 PTY インターフェース

ptylenz と子 bash の間の通信プロトコルは PTY バイトストリームのみである。

**ptylenz → bash** (stdin に書き込む):
- ユーザーのキーボード入力をバイトレベルでそのまま転送
- 唯一の例外: `Ctrl+]` (0x1d) は転送せず ptylenz のモード切り替えとして消費する

**bash → ptylenz** (stdout から読み取る):
- 全バイトを受信し OscParser に通す
- OSC 133 マーカーを消費し BlockEngine へ通知する
- それ以外のバイトを clean ストリームとして端末へ転送する

**端末サイズ同期**:
- 初期サイズ: `TIOCGWINSZ` で読み取り `openpty` に渡す
- SIGWINCH 受信時: `TIOCSWINSZ` で PTY ウィンドウサイズを更新し、`SIGWINCH` を子プロセスへ転送する

### 6.5 OSC 133 プロトコル仕様

ptylenz が消費する OSC 133 シーケンス:

| コード | ペイロード | 意味 |
|--------|-----------|------|
| `A` | なし | プロンプト表示開始 |
| `C` | なし | コマンド実行開始 |
| `D` | `;N` (終了コード) | コマンド終了 |
| `E` | `;text` (コマンドテキスト) | コマンドの文字列表現 |

ターミネータ: BEL (`\x07`) と ST (`\x1b\`) の両方を受理する。

ptylenz が透過的に通過させる OSC:

| コード | 用途 |
|--------|------|
| `0`, `1`, `2` | ウィンドウ/タブタイトル設定 |
| `8` | ハイパーリンク |
| `10`, `11`, `12` | 前景色/背景色クエリ (ncurses 等が依存) |
| `52` | クリップボード |
| `133;B` | プロンプト終了 (ptylenz は使用しない、透過) |
| その他 | 透過 |

**重要**: 非 133 OSC を無音でドロップする実装は不正である。`mc` 等の ncurses アプリはカラークエリ (OSC 10/11) への応答を受け取らないと描画が崩れる。

### 6.6 mpsc チャネルインターフェース

`claude_feeder::spawn_watcher()` は `Receiver<ClaudeEvent>` を返す。メインループは `try_recv()` で非ブロッキングにポーリングする。チャネルが切断された場合 (feeder スレッド終了) はエラーとして処理しない (サイレントに break)。

```rust
loop {
    match claude_rx.try_recv() {
        Ok(ev) => proxy.blocks_mut().ingest_claude_event(ev),
        Err(TryRecvError::Empty) => break,
        Err(TryRecvError::Disconnected) => break,
    }
}
```

---

## 7. UI (ratatui TUI Overlay)

### 7.1 レンダリングバックエンド

```
ratatui (0.29)
  └── CrosstermBackend<io::Stdout>
        └── crossterm::execute!(EnterAlternateScreen / LeaveAlternateScreen)
```

alt-screen は Ptylenz モードの間のみ有効。Normal モードへ戻ると `LeaveAlternateScreen` で元のターミナル表示 (シェルの scrollback を含む) が復元される。

### 7.2 レイアウト構造

```rust
Layout::default()
    .direction(Direction::Vertical)
    .constraints([Constraint::Min(1), Constraint::Length(1)])
    .split(area)
```

- `chunks[0]`: メインコンテンツ (List View または Detail View)
- `chunks[1]`: ステータスバー (1 行)

List View で検索バー表示中はさらに内側を分割:

```rust
Layout::default()
    .direction(Direction::Vertical)
    .constraints([Constraint::Length(3), Constraint::Min(1)])
    .split(chunks[0])
```

- `inner[0]`: 検索バー (Borders::ALL を含む 3 行)
- `inner[1]`: ブロックリスト

### 7.3 List View 詳細

`draw_blocks()` 関数が担当する。

```
┌─ ptylenz · blocks ──────────────────────────────────────────────────┐
│ [S] ▸   #1   10:23:44  ·  0 lines  ·  ok  ·  (unknown)             │
│ [S] ▾   #2   10:23:45  ·  2 lines  ·  ok  ·  echo hello            │
│       hello                                                          │
│       (end)                                                          │
│ [C] ▸ 📌 #3   10:23:50  ·  5 lines  ·  user  ·  claude user #1      │
│ [C] ▾   #4   10:23:51  ·  12 lines  ·  assistant  ·  claude ass #2  │  ← selected
│       ▶ assistant                                                    │
│       I'll help you with that.                                       │
│       → Bash({"command":"ls"})                                       │
└─────────────────────────────────────────────────────────────────────┘
```

**`build_list_item()` の実装**:

ヘッダー行のフォーマット:
```
[tag] fold pin  #id   HH:MM:SS  ·  N lines  ·  status  ·  command
```

- `tag`: Shell ブロック = `S` (DarkGray)、ClaudeTurn ユーザー = `C` (Magenta)、ClaudeTurn アシスタント = `C` (Cyan)
- `fold`: `▸` (collapsed=true) / `▾` (collapsed=false)
- `pin`: `📌` (pinned=true) / `  ` (pinned=false)
- Shell ブロックの status 色: exit_code=0→Green, exit_code≠0→Red, None→Yellow

展開状態の出力表示:
- `output_text()` の各行を `"      " + trim_line(line, 200)` でインデント + トリム
- 最大 `EXPAND_MAX_LINES` (200) 行
- 超過分は `"      … (N more lines — press e to export)"` (DarkGray)

`ListState::select(Some(selected.min(all.len()-1)))` でハイライト + スクロールを ratatui に委ねる。ハイライトスタイル: `bg(DarkGray) + BOLD`。

#### キーバインド

| キー | 動作 | 実装 |
|------|------|------|
| `j` / `↓` | 次のブロック | `selected = min(selected+1, max)` |
| `k` / `↑` | 前のブロック | `selected = selected.saturating_sub(1)` |
| `g` | 先頭ブロック | `selected = 0` |
| `G` | 末尾ブロック | `selected = block_count - 1` |
| `Enter` | 展開/折り畳み | `toggle_collapse(block.id)` |
| `v` | Detail View | `view = Detail(DetailState{block_id, ...})` |
| `/` | 検索入力開始 | `search_input = Some("")` |
| `n` | 次のヒット | `jump_search(+1)` |
| `N` | 前のヒット | `jump_search(-1)` |
| `y` | クリップボードコピー | `copy_to_clipboard(block.output_text())` |
| `e` | JSON エクスポート | `export_json()` → ファイル書き出し |
| `p` | ピン留め切り替え | `toggle_pin(block.id)` |
| `q` / `Esc` | Normal モードへ | `leave_ptylenz()` |
| `Ctrl+]` | Normal モードへ | `leave_ptylenz()` (最優先) |

### 7.4 Detail View 詳細

`draw_detail()` 関数が担当する。1 ブロックの全出力をカーソルと選択ハイライト付きで表示する。

タイトルバー:
```
 #3 · ls -la /home · exit 0 · 42 lines
```

#### 自動スクロール計算

```rust
let scroll_top = if detail.cursor_row < viewport_h {
    0
} else {
    detail.cursor_row + 1 - viewport_h
};
// さらに lines.len() 超えをクランプ
let scroll_top = scroll_top.min(lines.len().saturating_sub(viewport_h.max(1)));
```

`cursor_row` が常に viewport 内に収まることを保証する。

#### セル描画

各行をコード ポイント単位で走査し、3 種類のスタイルをセルに適用する:

```rust
let style = if is_cursor {
    Style::default().bg(Color::White).fg(Color::Black)  // カーソル
} else if in_selection {
    Style::default().bg(Color::Blue).fg(Color::White).add_modifier(BOLD)  // 選択
} else {
    Style::default().fg(Color::Gray)  // 通常
};
```

`max_col` を `chars.len().max(cursor_col + 1 if cursor_in_row)` に設定することで、行末より後ろにカーソルがある場合も表示できる (空白文字で埋める)。

#### キーバインド

| キー | 動作 |
|------|------|
| `h` / `←` | `cursor_col = cursor_col.saturating_sub(1)` |
| `j` / `↓` | `cursor_row = min(cursor_row+1, row_count-1)` + col クランプ |
| `k` / `↑` | `cursor_row = cursor_row.saturating_sub(1)` + col クランプ |
| `l` / `→` | `cursor_col = min(cursor_col+1, line_len(cursor_row))` |
| `g` | `cursor_row=0; cursor_col=0` |
| `G` | `cursor_row = row_count-1` + col クランプ |
| `0` | `cursor_col = 0` |
| `$` | `cursor_col = line_len(cursor_row)` |
| `Ctrl+d` | `cursor_row = min(cursor_row+10, row_count-1)` + col クランプ |
| `Ctrl+u` | `cursor_row = cursor_row.saturating_sub(10)` + col クランプ |
| `v` | Linewise 選択トグル |
| `Ctrl+v` | Blockwise 選択トグル |
| `y` | 選択範囲ヤンク (選択なし → ブロック全体) |
| `Y` | ブロック全体ヤンク (常に) |
| `Esc` | 選択解除、なければ List View へ |
| `q` | List View へ |

#### 選択モデル詳細

**Linewise** (`v`):

```
anchor_row=1, cursor_row=3 の場合:
  row 1: [0, line_len(1)) が選択
  row 2: [0, line_len(2)) が選択
  row 3: [0, line_len(3)) が選択
  row 0, 4以降: 非選択
```

ヤンク時: `lines[lo..=hi].join("\n")` を返す。

**Blockwise** (`Ctrl+v`, vim の `Ctrl-v`):

```
anchor=(1,3), cursor=(4,8) の場合:
  rows 1-4 の columns [3, 9) が選択
  各行の [3, min(9, line_len)) を切り出す
```

ヤンク時: 各行のセグメントを `\n` で結合して返す。アンカーとカーソルのどちらが上/左でも `sort_pair()` で正規化する:

```rust
fn sort_pair(a: usize, b: usize) -> (usize, usize) {
    if a <= b { (a, b) } else { (b, a) }
}
```

`selection_range_for_row()` が Linewise/Blockwise の行ごとの列範囲を計算する。

### 7.5 ステータスバー

```
[ptylenz] <help or status>   blocks: N
```

スタイル: `fg(Black).bg(Cyan)`

表示優先順位:
1. `status_message` が `Some(msg)` → `" [ptylenz] {msg}"`
2. Detail View 中 → `"{sel} · h/j/k/l move · g/G · v line · ^v block · y yank · Y all · q back · row {r}/col {c}"`
3. 検索入力中 → `"type query · Enter run · Esc cancel"`
4. 最後の検索があり → `"/{query} · n/N ({cur}/{total}) · j/k · Enter fold · ..."`
5. その他 → `"j/k move · Enter fold · v detail · / search · y copy · e export · p pin · g/G · q back"`

### 7.6 キーデコーダー

`decode_keys(bytes)` は生バイト列を `Vec<(Key, bool)>` に変換する。`bool` は Ctrl 修飾フラグ。

```rust
enum Key {
    Char(char), Up, Down, Left, Right,
    Enter, Backspace, Esc, Tab, Unknown,
}
```

主要なマッピング:

| バイト | Key | Ctrl |
|-------|-----|------|
| `0x1b [ A` | Up | false |
| `0x1b [ B` | Down | false |
| `0x1b [ C` | Right | false |
| `0x1b [ D` | Left | false |
| `0x1b` (単体) | Esc | false |
| `0x0d` / `0x0a` | Enter | false |
| `0x7f` / `0x08` | Backspace | false |
| `0x09` | Tab | false |
| `0x1d` | Char(']') | **true** (Ctrl+]) |
| `0x01`–`0x1a` | Char('a'–'z') | true (Ctrl+A–Z) |
| `0x20`–`0x7e` | Char(c) | false |

`Ctrl+d` (0x04) → `(Char('d'), true)`, `Ctrl+u` (0x15) → `(Char('u'), true)` として Detail View のページ送りに使用する。

---

## 8. 設定 (Configuration)

### 8.1 設定ファイルなし

v0.1 は設定ファイルを持たない。ユーザー設定可能なパラメータは存在しない。将来的に以下を設定可能にする計画:

- `EXPAND_MAX_LINES` のカスタマイズ
- auto-collapse の閾値 (現在は 50 行ハードコード)
- モードスイッチキーのカスタマイズ (現在は Ctrl+] のみ)
- ブロック上限数/最大メモリ使用量

### 8.2 bash integration (自動注入)

ptylenz が `$TMPDIR/ptylenz-rc-<PID>.sh` に書き出す内容:

```bash
# ptylenz wrapper rcfile — auto-generated, safe to delete
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"

# ptylenz shell integration — do not edit
__ptylenz_precmd() {
    local __ptylenz_ec=$?
    printf '\e]133;D;%d\a' "$__ptylenz_ec"
    local __ptylenz_last
    __ptylenz_last=$(HISTTIMEFORMAT='' history 1 2>/dev/null \
        | sed -E 's/^[[:space:]]*[0-9]+[[:space:]]*//')
    if [ -n "$__ptylenz_last" ]; then
        printf '\e]133;E;%s\a' "$__ptylenz_last"
    fi
}
PROMPT_COMMAND='__ptylenz_precmd'
PS0='\[\e]133;C\a\]'
case "$PS1" in
  *'133;A'*) ;;
  *) PS1='\[\e]133;A\a\]'"$PS1" ;;
esac
```

bash は `--rcfile <wrapper> -i` で起動される。`--rcfile` は `/etc/profile` と `~/.bash_profile` をスキップするが、ラッパー内で `~/.bashrc` を明示的にソースするため問題ない。

### 8.3 zsh integration (手動スニペット)

`~/.zshrc` に追加するスニペット:

```zsh
# ptylenz shell integration for zsh
__ptylenz_precmd() {
    printf '\e]133;D;%d\a' "$?"
    local last_cmd
    last_cmd=$(fc -ln -1 2>/dev/null | sed 's/^[[:space:]]*//')
    [ -n "$last_cmd" ] && printf '\e]133;E;%s\a' "$last_cmd"
}
__ptylenz_preexec() {
    printf '\e]133;C\a'
}
precmd_functions+=(__ptylenz_precmd)
preexec_functions+=(__ptylenz_preexec)
# Prompt start — 既存の PROMPT に先頭追加
PROMPT=$'\e]133;A\a'"$PROMPT"
```

注意事項:
- zsh には PROMPT_COMMAND がない。代わりに `precmd_functions` / `preexec_functions` 配列への追加を使用する
- `preexec` フックの `$1` にコマンドテキストが直接渡されるため、history ルックアップ不要
- `fc -ln -1` は zsh で `history 1` に相当する

### 8.4 fish integration (手動スニペット)

`~/.config/fish/config.fish` に追加するスニペット:

```fish
# ptylenz shell integration for fish
function __ptylenz_prompt_start --on-event fish_prompt
    printf '\e]133;A\a'
end

function __ptylenz_preexec --on-event fish_preexec
    printf '\e]133;C\a'
    printf '\e]133;E;%s\a' "$argv"
end

function __ptylenz_postexec --on-event fish_postexec
    printf '\e]133;D;%d\a' "$status"
end
```

注意事項:
- fish の `fish_preexec` は `$argv` にコマンドテキストを持つため `133;C` と `133;E` を同時に発行できる
- `fish_postexec` の `$status` が直前コマンドの終了コードを持つ
- `133;E` が `133;C` と同時に届く (iTerm2 モデル)。BlockEngine の CommandText ハンドラは `current` が開いている場合にアタッチするため正常動作する

### 8.5 tmux との組み合わせ設定

ptylenz を外側に、tmux を内側に配置する場合:

```bash
# ~/.tmux.conf に追加
set -g set-clipboard on   # OSC 52 クリップボードを有効化
```

tmux 3.3+ では OSC 133 は透過されるため、tmux 内のシェルから ptylenz への OSC 133 通知が届く。ptylenz を外側に置く構成では問題ない。

ptylenz を tmux の内側で実行する場合、OSC 133 が tmux にインターセプトされる可能性がある。この構成は推奨しない。

### 8.6 SSH セッションでの動作

OSC 133 は通常の ANSI エスケープシーケンスであり、SSH を経由しても転送される。特別な設定は不要。

OSC 52 クリップボードは SSH 経由で機能する場合とそうでない場合がある (ターミナルエミュレータの設定に依存する)。OSC 52 が機能しない場合は `xclip`/`pbcopy` へのフォールバックも失敗するが、ptylenz はサイレントに無視する。

---

## 9. 依存関係 (Dependencies)

### 9.1 ランタイム依存 (`[dependencies]`)

| クレート | バージョン | 用途 | 代替採用しなかった理由 |
|---------|-----------|------|----------------------|
| `ratatui` | 0.29 | TUI レンダリングフレームワーク | — |
| `crossterm` | 0.28 | alt-screen 制御、ratatui バックエンド | `termion`: Unix 専用で macOS サポートが不安定 |
| `nix` | 0.29 (term, process, signal, poll, fs) | `openpty`, `fork`, `execvp`, `waitpid`, `kill`, TIOCSWINSZ | `libc` 直接: unsafe が増える |
| `libc` | 0.2 | `tcgetattr/setattr`, `cfmakeraw`, `ioctl`, `fcntl`, `read`, `signal` | nix でカバーできない低レベル操作のため必要 |
| `polling` | 3 | epoll/kqueue による stdin + PTY master の多重待機 | `tokio`/`async-std`: 非同期ランタイム全体は重すぎる |
| `anyhow` | 1 | エラーコンテキスト連鎖 (`context()`) | `thiserror`: 公開 API がないので anyhow で十分 |
| `chrono` | 0.4 | `DateTime<Local>`, RFC 3339 フォーマット | `time` クレート: chrono の方が生態系が広い |
| `regex` | 1 | `regex::bytes::Regex` (フォールバックプロンプト検出用) | 現在未使用だが将来の正規表現検索で必要 |
| `unicode-width` | 0.2 | 文字幅計算 (ratatui の幅計算と連携) | — |
| `unicode-segmentation` | 1 | 書記素クラスタ境界 (`append_truncated` の ZWJ 保護) | — |
| `serde` | 1 (derive) | derive マクロ (`Deserialize`) | — |
| `serde_json` | 1 | JSONL デコード、エクスポート用 Value | — |
| `vt100` | 0.15 | vt100 シャドウグリッドパーサー | `alacritty_terminal`: 依存グラフが大きすぎる |

### 9.2 開発依存 (`[dev-dependencies]`)

| クレート | バージョン | 用途 |
|---------|-----------|------|
| `tempfile` | 3 | テスト用一時ファイル/ディレクトリ |

### 9.3 意図的に除外した依存

| クレート | 除外理由 |
|---------|---------|
| `notify` (inotify/kqueue) | SSH マウント homedir での非動作リスク。400ms ポーリングで十分。`docs/decisions/notify-dead-dep.md` に詳細 |
| `tokio` / `async-std` | 非同期ランタイム全体: PTY 読み書きは同期 epoll で充足。依存グラフを軽量に保つ |
| `clap` / `argh` | CLI パーサー: v0.1 はオプションなし。将来追加予定 |
| `base64` | base64 エンコード: OSC 52 クリップボード用途のみ。300 バイトの自前実装で十分 |
| `log` / `tracing` | ロギングフレームワーク: デバッグは `RUST_BACKTRACE=1` で対応。将来必要になれば追加 |
| `dirs` / `home` | ホームディレクトリ検出: `std::env::var_os("HOME")` で十分 |

### 9.4 バイナリサイズの傾向

`cargo build --release` での概算バイナリサイズ: 3–6 MB (Linux x86_64)。主な貢献クレート: `ratatui` (大), `crossterm` (中), `regex` (中), `vt100` (小)。`unicode-segmentation` のデータテーブルが数百 KB を占める。

---

## 10. 非機能要件 (Non-Functional Requirements)

### 10.1 透過性 (Transparency)

Normal モードの ptylenz は **完全透過プロキシ** でなければならない。

**入力の透過**:
- `Ctrl+]` (0x1d) 以外の全バイトを変更なしに子 bash へ転送する
- マウスシーケンス、UTF-8 マルチバイト、CSI シーケンスを含む全バイトを透過する

**出力の透過**:
- PTY ストリームから OSC 133 シーケンス (4 種) のみを除去する
- それ以外の全バイトを変更なしに端末へ転送する:
  - ANSI 色 (CSI SGR)
  - カーソル制御 (CSI CUP, ED, EL, DECSTBM 等)
  - OSC タイトル設定 (OSC 0/1/2)
  - OSC ハイパーリンク (OSC 8)
  - OSC カラークエリ (OSC 10/11/12) ← ncurses アプリが依存
  - OSC クリップボード (OSC 52)
  - DCS, PM, APC 等の他の制御シーケンス

**検証方法**: `test_osc_parser_passthroughs_non_133` テストが OSC 0/8/11 の透過を保証する。

### 10.2 パフォーマンス要件

| 操作 | 要件 | 実装 |
|------|------|------|
| `line_count()` | O(1) | `cached_line_count` フィールドを append 時に更新 |
| 通常コマンドの `output_text()` | O(n) n=output bytes | `strip_ansi()` (regex なし、シングルパス) |
| alt-screen の `output_text()` | O(1) | `rendered_text` を直接返す |
| 検索 `search(query)` | O(total output bytes) | 線形スキャン、UI 操作時のみ呼ばれる |
| 80ms 以内のメインループ反復 | ユーザー入力の遅延 < 80ms | `poller.wait(Some(80ms))` でタイムアウト |

過去に発生したパフォーマンス問題:
- **旧実装**: `line_count()` が毎回 `output.iter().filter(b == '\n').count()` を実行。mc/claude が数 MB の出力を蓄積するとリスト表示が数秒かかった。
- **現在の実装**: `cached_line_count` により O(1)。

### 10.3 メモリ使用量

v0.1 では上限を設けていない。ベンチマーク対象のシナリオ:

| シナリオ | 概算メモリ |
|---------|-----------|
| 100 ブロック × 平均 1 KB 出力 | ~100 KB |
| `find / -type f` (100万行) | ~100 MB |
| 長時間 claude セッション (1000 ターン) | ~50 MB (テキストのみ) |

vt100 シャドウパーサーは `vt100::Parser::new(rows, cols, 2000)` で生成される。内部バッファは `rows * cols * セルサイズ + 2000 * cols * セルサイズ` 程度。80列 × (24 + 2000) 行 × ~10 バイト/セル ≈ 1.6 MB / ブロック (ブロック終了時に `vt_parser = None` で解放)。

### 10.4 CJK 全角文字の既知制限

`unicode-width` クレートは CJK 文字に対して `width = 2` を正しく計算する。しかし ratatui の `Paragraph` / `List` ウィジェットは内部的に `unicode-width` を参照してセル幅を計算しており、理論上は正しい。

実際の問題は Detail View のカーソル列計算にある。`detail.cursor_col` は **コードポイント数** (chars().count()) ベースであるが、全角文字は端末上で 2 セル占有する。全角文字が多い行では `cursor_col` と実際の端末カーソル位置がずれる。

Phase 4 での修正計画:
- `cursor_col` をコードポイント数ではなく **表示列幅** (unicode-width の累積値) で管理する
- `selection_range_for_row()` も幅ベースの計算に変更する

### 10.5 Windows 非対応

以下の依存関係が Windows に存在しない:

| シンボル | 理由 |
|---------|------|
| `openpty(2)` | Windows には ConPTY API があるが nix クレートは対応しない |
| `fork(2)` | Windows にはない |
| `setsid(2)` | Windows にはない |
| `TIOCSCTTY` | Windows にはない |
| `cfmakeraw()` | Windows にはない (termios 自体がない) |

Windows サポートには `windows-sys` / `winapi` クレートを使った ConPTY ベースの完全な代替実装が必要であり、本仕様の対象外。

### 10.6 シグナル処理の制約

ptylenz はシグナルハンドラを最小限に設定する:

| シグナル | 処理 |
|---------|------|
| SIGWINCH | `AtomicBool::RESIZED.store(true)` のみ。次のメインループ反復で処理 |
| SIGHUP | デフォルト (子プロセス終了時に Shell が ptylenz へ送る場合あり) |
| SIGTERM | デフォルト (ptylenz 終了) |
| SIGCHLD | 使用しない。`waitpid(WNOHANG)` ポーリングで子プロセス終了を検出 |
| SIGPIPE | デフォルト (stdout が閉じられた場合) |

シグナルハンドラ内では async-signal-safe な操作のみを行う。`AtomicBool` への `store(true, Relaxed)` は safe。

### 10.7 再入不可性とスレッド安全性

`BlockEngine` と `OscParser` は `Sync` / `Send` を実装しない (`&mut self` でのみ操作)。これらはメインスレッドから排他的に操作される。

スレッド境界:
- メインスレッド: PTY 読み書き、BlockEngine、TUI 描画
- claude_feeder スレッド: JSONL ポーリング。`Sender<ClaudeEvent>` 経由でのみメインスレッドと通信

---

## 11. テスト戦略 (Test Strategy)

### 11.1 テスト方針

- **ユニットテスト**: 各モジュールの `#[cfg(test)]` ブロックに配置
- **E2E テスト**: `pty.rs` に実際の bash をフォークするエンドツーエンドテスト
- **プロパティテスト**: v0.1 では採用しない (将来 proptest を検討)
- **統合テスト**: `tests/` ディレクトリは v0.1 では使用しない

目標テスト数: 30

### 11.2 block.rs のテスト (19 テスト)

#### OscParser テスト (5)

| テスト名 | 検証内容 |
|---------|---------|
| `test_osc_parser_detects_command_start` | BEL 終端 133;C を検出し、"hello" と "world" が clean バイトに残ること |
| `test_osc_parser_detects_command_end` | 133;D;0 から exit_code=0 が取れること |
| `test_osc_parser_detects_command_text` | 133;E;ls -la からコマンドテキストが取れること |
| `test_osc_parser_passthroughs_non_133` | OSC 0 (タイトル)、OSC 8 (ハイパーリンク)、OSC 11;? (カラークエリ) が clean バイトとして全バイト再送されること |
| `test_osc_parser_consumes_133_with_st_terminator` | ESC `\` 終端の 133;C が完全消費され、clean バイトに `\` が残らないこと |

#### BlockEngine テスト (5)

| テスト名 | 検証内容 |
|---------|---------|
| `test_block_engine_lifecycle` | 133;A → C → E → D の完全シーケンスで 1 ブロックが生成され、command/exit_code/line_count が正しいこと |
| `test_search` | "world" を検索して正しい (block_id, line_num, line_text) を返すこと |
| `test_export_common_model_json` | エクスポート JSON に "agent":"ptylenz", "messages", "role":"user", "text":"ls", "role":"assistant", "exit_code":0 が含まれること。`,\n  ]` がないこと (trailing comma なし) |
| `test_json_escape_handles_quotes_and_newlines` | `"`, `\n`, `\t`, `\\` が正しくエスケープされること |
| `long_line_copy_does_not_get_wrap_newlines` | 40 列の狭い端末で 400 文字の行がスプリットされないこと (vt100 シャドウグリッドの折り返しが raw バイト列に影響しないこと) |

#### vt100 / ANSI テスト (3)

| テスト名 | 検証内容 |
|---------|---------|
| `test_strip_ansi` | `\e[32mgreen\e[0m plain \e[1;31mred\e[0m` → `"green plain red"` |
| `test_alt_screen_block_uses_vt_snapshot` | `?1049h` → コンテンツ → `?1049l` の流れで `rendered_text` が設定され、output_text に "HELLO TUI" と "GARBAGE" が含まれること |
| `test_plain_command_skips_vt_snapshot` | alt-screen なしコマンドで `rendered_text=None`、output_text が "file1\nfile2" であること |

#### ClaudeTurn テスト (2)

| テスト名 | 検証内容 |
|---------|---------|
| `test_ingest_claude_turn_creates_sibling_block` | SessionStarted + Turn×2 で 2 ブロック生成、command ラベルが "claude user #1" / "claude assistant #2"、tool_use 行 "→ Bash" が含まれること |
| `test_shell_and_claude_blocks_coexist` | Shell ブロック 1 個 + ClaudeTurn ブロック 1 個が `blocks[0].source=Shell`, `blocks[1].is_claude_turn()=true` で共存すること |

#### append_truncated テスト (4)

| テスト名 | 検証内容 |
|---------|---------|
| `append_truncated_short_passes_through` | 短い文字列は変更なしに通過 |
| `append_truncated_ascii_cuts_at_limit_with_ellipsis` | "abcdefghij", max=4 → "abcd…" |
| `append_truncated_multibyte_never_splits_codepoint` | 'ル' (3 バイト) が max=2 で空+`…`、max=3 で "ル…"、max=5 で "ル…" |
| `append_truncated_keeps_zwj_sequence_intact` | 👨‍👩‍👧 (18 バイト) が max=10 で `"…"` に (部分的な ZWJ シーケンスを出力しない)、max=18 で family+`"…"` |

### 11.3 pty.rs の E2E テスト (3)

実際に `/bin/bash` を fork する。CI 環境では `/bin/bash` の存在を前提とする。`--test-threads=1` が必要 (`fork` との組み合わせのため)。

| テスト名 | 検証内容 |
|---------|---------|
| `spawn_bash_and_detect_blocks` | `echo hello-ptylenz; false; exit` を送り、完了ブロックが 1 個以上あり、"echo hello-ptylenz" を含むブロックが存在すること |
| `alt_screen_command_produces_rendered_text` | `printf '\e[?1049h...' ; sleep 0.4; printf '\e[?1049l'` を送り、"TUI-MARKER-XYZZY" を含むブロックの `rendered_text` が設定されること |
| `plain_command_captures_visible_output` | `echo LINE-ONE; echo LINE-TWO; echo LINE-THREE` を送り、output_text に 3 行すべてが含まれること、`cached_line_count` が実際の `\n` 数と一致すること |

### 11.4 claude_feeder.rs のテスト (5)

| テスト名 | 検証内容 |
|---------|---------|
| `slug_replaces_slashes_with_dashes` | `/home/opa/work/ptylenz` → `-home-opa-work-ptylenz` |
| `decode_user_turn_with_string_content` | content が文字列形式のターン: role="user", text="hello", session_id="sess1", tool_uses=[] |
| `decode_assistant_turn_with_text_and_tool_use` | content が配列 (text + tool_use): role="assistant", text="looking…", tool_uses[0].name="Bash", input_json に "ls" が含まれること |
| `decode_skips_non_turn_lines` | "permission-mode" と "file-history-snapshot" エントリが None を返すこと |
| `decode_malformed_is_none` | "not json" と "" が None を返すこと |

### 11.5 tui_app.rs のテスト (3)

| テスト名 | 検証内容 |
|---------|---------|
| `linewise_range_covers_whole_lines_in_anchor_to_cursor_span` | anchor=1, cursor=3 の Linewise 選択で row=2 が (Some(0), Some(10))、row=4 が (None, None)、row=1 が (Some(0), Some(7)) |
| `blockwise_range_clamps_to_line_length` | anchor=(0,3), cursor=(2,8) の Blockwise で長行が (Some(3), Some(9))、短行が (Some(3), Some(5))、範囲外行が (None, None) |
| `blockwise_range_handles_reversed_anchor` | カーソル (0,2) < アンカー (4,7) の場合に sort_pair が正規化し row=2 が (Some(2), Some(8)) |

### 11.6 テスト実行

```bash
# 全テスト
cargo test

# 標準出力を表示
cargo test -- --nocapture

# 名前でフィルタ
cargo test osc_parser
cargo test block_engine

# E2E テストをシングルスレッドで (fork の安全性のため)
cargo test -p ptylenz --test-threads=1

# リリースビルドでテスト
cargo test --release

# カバレッジ (cargo-tarpaulin が必要)
cargo tarpaulin --out Html
```

### 11.7 CI 設定 (計画)

GitHub Actions で以下のマトリックスを実行:

```yaml
strategy:
  matrix:
    os: [ubuntu-latest, macos-latest]
    rust: [stable]

steps:
  - cargo test --test-threads=1
  - cargo clippy -- -D warnings
  - cargo fmt --check
```

---

## 12. デプロイ / 運用 (Deployment / Operations)

### 12.1 ビルド要件

| 項目 | バージョン/値 |
|------|--------------|
| Rust toolchain | stable (1.75+) |
| 対象プラットフォーム | x86_64-unknown-linux-gnu, aarch64-apple-darwin, x86_64-apple-darwin |
| ビルドコマンド | `cargo build --release` |
| 成果物 | `target/release/ptylenz` (シングルバイナリ) |
| 外部コンパイル依存 | `libgcc` (Linux, 通常インストール済み) |

### 12.2 リリース手順

```bash
# バージョン更新
# Cargo.toml の version を更新

# ビルド
cargo build --release

# テスト
cargo test --test-threads=1

# タグ付け
git tag v0.1.0
git push origin v0.1.0
```

GitHub Actions (計画):
1. タグプッシュをトリガーに `cargo build --release` を Linux / macOS で実行
2. バイナリをアーティファクトとして保存
3. GitHub Release を作成し `ptylenz-linux-x86_64`, `ptylenz-macos-aarch64`, `ptylenz-macos-x86_64` をアップロード

### 12.3 インストール

**バイナリ直接ダウンロード**:
```bash
curl -LO https://github.com/opaopa6969/ptylenz/releases/latest/download/ptylenz-linux-x86_64
chmod +x ptylenz-linux-x86_64
sudo mv ptylenz-linux-x86_64 /usr/local/bin/ptylenz
```

**install.sh スクリプト** (リポジトリ同梱):
```bash
curl -fsSL https://raw.githubusercontent.com/opaopa6969/ptylenz/main/install.sh | bash
```

`install.sh` は OS とアーキテクチャを自動検出し、対応するバイナリをダウンロードして `~/.local/bin/` または `/usr/local/bin/` へインストールする。

**ソースからビルド**:
```bash
git clone https://github.com/opaopa6969/ptylenz
cd ptylenz
cargo build --release
cp target/release/ptylenz ~/.local/bin/
```

### 12.4 起動と終了

**起動**:
```bash
ptylenz
```

子 bash が起動し、ptylenz が透過プロキシとして動作し始める。シェルプロンプトが表示されれば正常動作。

**動作確認**:
```bash
echo "test"
# Ctrl+] でオーバーレイを開き、ブロックが表示されることを確認
# q で戻る
```

**終了**:
```bash
exit   # 子 bash を終了
# または Ctrl+D
```

終了時メッセージ:
```
[ptylenz] Session ended. 42 blocks captured.
```

### 12.5 ログとデバッグ

v0.1 はログを出力しない。デバッグ手順:

```bash
# クラッシュ時のスタックトレース
RUST_BACKTRACE=1 ptylenz

# デバッグビルドで実行 (最適化なし)
cargo build
./target/debug/ptylenz

# OSC 133 統合の手動確認
printf '\e]133;C\a'; echo "test output"; printf '\e]133;D;0\a'; printf '\e]133;E;test\a'
# Ctrl+] → "test" ブロックが表示されれば正常
```

### 12.6 アップグレード

バイナリ上書きでアップグレード。セッション間の互換性は不要 (in-memory のみ)。アップグレード前にエクスポートが必要なブロックは `e` キーで JSON に書き出す。

### 12.7 アンインストール

```bash
rm $(which ptylenz)
# または
rm ~/.local/bin/ptylenz
```

ptylenz は設定ファイル・データベース・キャッシュをホームディレクトリに書かない (ラッパー rcfile は `$TMPDIR` に書き出され OS のクリーンアップで削除される)。

### 12.8 既知の起動問題と対処法

| 症状 | 原因 | 対処 |
|------|------|------|
| `Failed to open PTY pair` | `/dev/ptmx` のパーミッション不足 | `ls -l /dev/ptmx` で確認、通常は `crw-rw-rw-` |
| ブロックが生成されない | PROMPT_COMMAND が上書きされた | `echo $PROMPT_COMMAND` で確認。`__ptylenz_precmd` が含まれていれば正常 |
| alt-screen アプリが崩れる | 初期 PTY サイズが 0×0 | `TIOCGWINSZ` が失敗している場合。ターミナルエミュレータを確認 |
| Ctrl+] が機能しない | 端末が 0x1d を別用途に使用 | tmux/screen の設定を確認 |
| `exit` 後にシェルに戻れない | 親シェルの termios が崩れた | `TermiosGuard::drop()` が実行されているはず。`reset` コマンドで復元 |

---

*この仕様書は ptylenz v0.1 の実装 (`src/` 以下 3280 行) に基づき記述された。*
*Phase 2–4 の詳細仕様は各フェーズの着手時に追記する。*
