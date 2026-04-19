# ptylenz スタートガイド

> [English](getting-started.md) · 日本語

> **対応プラットフォーム**: Linux および macOS のみ。Windows は未対応。

---

## 前提条件

- Linux x86_64 または macOS (Apple Silicon / Intel)
- bash 4 以上 (ほとんどの Linux ディストリビューションではデフォルト; macOS は bash 3.2 同梱 — Homebrew でのアップグレードは任意)
- クリップボードサポート (Linux): `xclip` (任意)

ソースからビルドする場合はさらに:

- Rust 1.76 以降 (`rustup.rs`)

---

## インストール: ビルド済みバイナリ (推奨)

[Releases ページ](https://github.com/opaopa6969/ptylenz/releases/latest) から使用プラットフォームのバイナリを取得:

```bash
# Linux x86_64
curl -L https://github.com/opaopa6969/ptylenz/releases/latest/download/ptylenz-linux-x86_64 \
     -o ptylenz
chmod +x ptylenz
sudo mv ptylenz /usr/local/bin/

# macOS Apple Silicon
curl -L https://github.com/opaopa6969/ptylenz/releases/latest/download/ptylenz-macos-aarch64 \
     -o ptylenz
chmod +x ptylenz
sudo mv ptylenz /usr/local/bin/

# macOS Intel
curl -L https://github.com/opaopa6969/ptylenz/releases/latest/download/ptylenz-macos-x86_64 \
     -o ptylenz
chmod +x ptylenz
sudo mv ptylenz /usr/local/bin/
```

確認:

```bash
ptylenz --version
```

---

## インストール: ソースから

```bash
git clone https://github.com/opaopa6969/ptylenz.git
cd ptylenz
./install.sh
```

`install.sh` は `cargo install --path . --force` を実行し、`~/.cargo/bin/ptylenz` にバイナリを置く。このディレクトリが `PATH` にない場合は追加する:

```bash
# ~/.bashrc または ~/.zshrc に追記
export PATH="$HOME/.cargo/bin:$PATH"
```

更新する場合:

```bash
cd ptylenz && git pull && ./install.sh
```

---

## 初回起動

```bash
ptylenz
```

シェルが ptylenz の内部で起動する。ターミナルの見た目はこれまでと完全に同じ — バナーなし、プロンプト変更なし、遅延なし。Normal モードの ptylenz は不可視。

いくつかコマンドを普通に実行する:

```bash
ls -la
echo "hello ptylenz"
cargo test 2>&1 | head -20
```

**`Ctrl+]`** を押して Ptylenz モードに入る。

ブロックリストが画面にオーバーレイされるはず。実行した各コマンドがタイムスタンプ・行数・終了ステータス・コマンドテキストとともにブロックとして表示される。

`j`/`k` (または矢印キー) でナビゲート。`Enter` でブロックを展開/折り畳み。`q`・`Esc`・または再度 `Ctrl+]` で Normal モードに戻る。

---

## 基本的なワークフロー

### ブロックのナビゲート

| キー | 動作 |
|-----|------|
| `j` / `↓` | 次のブロック |
| `k` / `↑` | 前のブロック |
| `g` | 先頭ブロックへジャンプ |
| `G` | 末尾ブロックへジャンプ |
| `Enter` | 選択中のブロックを展開 / 折り畳み |

### 検索

`/` で検索バーを開く。クエリを入力して `Enter`。`n`/`N` で次/前のマッチへジャンプ。

### コピー

`y` を押すと選択中のブロックの出力をクリップボードにコピーする。

細かい選択は `v` で Detail ビューを開いてから:
- `v` で行選択
- `Ctrl+v` で矩形選択
- `y` で選択範囲をコピー

### エクスポート

`e` でカレントディレクトリに全ブロックを JSON ファイルへエクスポートする。JSON は [claude-session-replay](https://github.com/opaopa6969/claude-session-replay) 共通ログモデル形式に準拠。

---

## シェル統合

ptylenz はラッパー rcfile 経由で OSC 133 マーカーを bash に自動注入する。bash では設定不要。

その他のシェル (zsh、fish) については [docs/shell-integration-ja.md](shell-integration-ja.md) を参照。

---

## 任意: シェル起動時に自動で入る

`~/.bashrc` の末尾に追記すると、ターミナルを開くたびに自動で ptylenz が起動する:

```bash
# 非対話シェル (scp / rsync / ssh host cmd 等) では何もしない
case $- in *i*) ;; *) return ;; esac
[ -z "$PTYLENZ" ] && command -v ptylenz >/dev/null && exec ptylenz
```

**注意**: ptylenz がクラッシュすると新しいシェルがすべて起動に失敗する。安定性を十分に確認してから有効にすること。リカバリは `bash --norc`。

ptylenz が子 bash にセットする `$PTYLENZ=1` 環境変数が二重起動を防ぐ。

---

## クリップボードサポート

**macOS**: `pbcopy` を自動使用 — 設定不要。

**Linux**: ptylenz はまず OSC 52 を試みる (tmux で `set-clipboard` が有効な場合に動作)、次に `xclip` にフォールバックする。tmux を使っていない場合は xclip をインストール:

```bash
# Debian / Ubuntu
sudo apt install xclip

# Fedora
sudo dnf install xclip

# Arch
sudo pacman -S xclip
```

---

## アップデート

### バイナリインストール

Releases ページから新しいバイナリをダウンロードし、初回インストールと同じ手順で置き換える。

### ソースインストール

```bash
cd ptylenz
git pull
./install.sh
```

---

## アンインストール

```bash
# /usr/local/bin にインストールした場合
sudo rm /usr/local/bin/ptylenz

# cargo install でインストールした場合
cargo uninstall ptylenz
```

追加した自動起動スニペットを `~/.bashrc` から削除することも忘れずに。

---

## トラブルシューティング

### ptylenz がすぐに終了する

シェルパスが bash でない可能性がある。ptylenz は `$SHELL` を読んで起動するシェルを決定する。`$SHELL` が zsh や fish を指している場合、ptylenz は起動するがシェル統合が注入されず (OSC 133 マーカーが発行されず)、ブロックが検出されない。

各シェルの統合が完成するまでの回避策:

```bash
SHELL=/bin/bash ptylenz
```

### コマンドを実行してもブロックリストが空のまま

シェル統合が読み込まれていない可能性がある。`~/.bashrc` にラッパー rcfile が source した後に実行される `PROMPT_COMMAND` 代入がないか確認する — それが ptylenz の `PROMPT_COMMAND` を黙って上書きしてしまう。詳細は [docs/shell-integration-ja.md](shell-integration-ja.md) を参照。

### Linux でクリップボードが動作しない

`xclip` をインストール (上記参照)。tmux セッション内の場合は `tmux set-option -g set-clipboard on` を実行して OSC 52 パススルーを有効にする。

### Detail ビューで CJK 文字の位置がずれる

既知の表示問題。CJK (中国語・日本語・韓国語) 文字は全角 (ターミナルセル 2 つ分)。ratatui オーバーレイパネルがキャプチャしたターミナルより狭い場合、CJK 文字を多く含む行がはみ出したり位置がずれたりする。現時点での回避策はない。

### 自動起動ループでシェルが起動しなくなった

```bash
bash --norc
```

その後 `~/.bashrc` から自動起動スニペットを削除する。
