# シェル統合

> [English](shell-integration.md) · 日本語

ptylenz はブロック境界を正確に検出するために **OSC 133** を使用する — iTerm2・Warp・VS Code Terminal が採用しているのと同じシェル統合プロトコル。

> **対応プラットフォーム**: Linux および macOS のみ。Windows は未対応。

---

## 仕組み

ptylenz が bash を起動するとき、ラッパー rcfile を `$TMPDIR` に書き出し、`bash --rcfile <wrapper>` で渡す。ラッパーは:

1. まず `~/.bashrc` を source する — 既存のプロンプト・エイリアス・補完はそのまま保たれる。
2. `__ptylenz_precmd` 関数を定義し、`PROMPT_COMMAND`・`PS0`・`PS1` に組み込む。

発行される 4 つの OSC 133 シーケンス:

| シーケンス | 発行タイミング | ソース |
|----------|-------------|--------|
| `\e]133;A\a` | プロンプトが表示される直前 | `PS1` 先頭付加 |
| `\e]133;C\a` | コマンド実行の直前 | `PS0` |
| `\e]133;D;N\a` | コマンド終了後 (終了コード N) | `PROMPT_COMMAND` |
| `\e]133;E;text\a` | コマンドテキスト (`history 1` から取得) | `PROMPT_COMMAND` |

bash では自動。設定不要。

---

## bash

### 自動注入

すべて ptylenz が処理する。`~/.bashrc` への追加は不要。

### ptylenz が注入するもの

```bash
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

### PROMPT_COMMAND の上書きについて

`PROMPT_COMMAND` は追記ではなく直接代入される。つまり、ptylenz がラッパー内で `~/.bashrc` を source した**後**に実行される `PROMPT_COMMAND` 代入があると ptylenz の値が上書きされ、ブロック検出が機能しなくなる。

よくある問題: `~/.bashrc` の末尾付近に `PROMPT_COMMAND="something"` が含まれているケース。ptylenz はまず `~/.bashrc` を source してから `PROMPT_COMMAND='__ptylenz_precmd'` をセットするので、その順序では正しく動作する。しかし、`~/.bashrc` が末尾で別のファイル (会社の dotfile など) を source し、そのファイルが `PROMPT_COMMAND` を再設定すると ptylenz の設定が上書きされる。

対処法: 上書きではなく連結形式を使う。

```bash
PROMPT_COMMAND="${PROMPT_COMMAND:+${PROMPT_COMMAND};}your_function"
```

`${PROMPT_COMMAND:+${PROMPT_COMMAND};}` は既存の値がある場合のみ `値;` に展開されるため、ptylenz の有無に関わらず安全に使用できる。

詳細な設計判断は [docs/decisions/prompt-command-strategy.md](decisions/prompt-command-strategy.md) を参照。

### DEBUG トラップではなく PROMPT_COMMAND を使う理由

ptylenz は意図的に `DEBUG` トラップを避けている。`DEBUG` トラップは関数内のすべての単純コマンドの直前 (サブシェル呼び出しやコマンド置換を含む) に発火する。`__ptylenz_precmd` の中で `history 1` を実行すると、そのサブコマンドでも `DEBUG` トラップが発火し、ブロック途中に偽の `133;C` イベントが生成される。`PROMPT_COMMAND` はプロンプト間にのみ発火するため、このようなネストは起きない。

---

## zsh

ptylenz は現在 zsh への自動注入に対応していない。手動セットアップ:

`~/.zshrc` に以下を追加:

```zsh
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
# プロンプト開始 — 既存の PROMPT または PS1 に追加:
PROMPT=$'\e]133;A\a'"$PROMPT"
```

注意点:
- zsh の `precmd`・`preexec` フックが正しい手段。`PROMPT_COMMAND` に相当するものは zsh にはない。
- `preexec` はコマンド文字列を `$1` として受け取るため、履歴を読まずに `133;E;$1` を直接発行できる。
- 動作確認: `printf '\e]133;C\a'; echo hello; printf '\e]133;D;0\a'` を実行してシーケンスが ptylenz に届くか確認する。

---

## fish

ptylenz は現在 fish への自動注入に対応していない。手動セットアップ:

`~/.config/fish/config.fish` に以下を追加:

```fish
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

注意点:
- fish のイベント `fish_prompt`・`fish_preexec`・`fish_postexec` が正しいフック。
- `fish_preexec` の `$argv` にコマンド文字列が入るため、履歴参照不要。
- 動作確認: `echo test` を実行し ptylenz でブロックが表示されるか確認する。

---

## OSC 133 リファレンス

| コード | ペイロード | 意味 |
|------|---------|------|
| `A` | (なし) | プロンプトが表示されようとしている |
| `B` | (なし) | プロンプト表示完了 (プロンプト末尾) — ptylenz は未使用 |
| `C` | (なし) | コマンド実行が開始されようとしている |
| `D` | `;N` (終了コード) | コマンド終了 |
| `E` | `;text` (コマンド) | ブロックタイトル用コマンドテキスト |

ターミネータは BEL (`\a`、`\x07`) と ST (`\e\\`) の両方に対応。

### シーケンスの順序

iTerm2 モデルでは `133;E` はプロンプト表示時 (`133;A` と同時) に届く。ptylenz の bash インテグレーションでは `133;E` は `PROMPT_COMMAND` (つまり `133;D` の後) から届く。ブロックエンジンは両方の順序を処理する: `133;E` 到着時に `current` が開いていればコマンドテキストをオープンブロックに付加し、`current` が `None` なら最後に閉じたブロックにパッチする。

---

## 統合が有効かどうかを確認する

ptylenz セッション内から:

```bash
printf '\e]133;C\a'; echo "テスト出力"; printf '\e]133;D;0\a'; printf '\e]133;E;test\a'
```

`Ctrl+]` を押してブロックリストを開く。統合が機能していれば "テスト出力" を含む `test` というブロックが表示されるはず。

ブロックが表示されない場合、OSC 133 シーケンスが ptylenz に届いていない — シェル統合が読み込まれているか、`PROMPT_COMMAND` が上書きされていないか確認する。

---

## tmux および多重化ツールとの互換性

ptylenz が tmux セッション内で動作している場合:

- OSC 52 クリップボードには `~/.tmux.conf` に `set-clipboard on` が必要
- OSC 133 シーケンスは tmux 3.3 以降では透過的にパススルーされる

tmux が ptylenz セッション内で動作している場合 (ptylenz が外側のレイヤー):

- tmux 内部のシェルからの OSC 133 シーケンスは tmux に横取りされ ptylenz には届かない
- `xclip`/`pbcopy` 経由のクリップボードは引き続き動作する

推奨構成: ptylenz を外側、tmux を内側 (セッション永続化やペイン分割が必要な場合のみ)。
