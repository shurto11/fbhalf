# fbhalf

Linux フレームバッファ用の画像ビューア。画面を左右・四分割した領域に画像（PNG/JPEG など）を表示し、tmux ペイン位置の自動追跡にも対応しています。

## 機能

- フレームバッファ (`/dev/fb0`) の指定領域に PNG を描画
- `--display` オプションで外部モニターに DRM 直接出力
- ズーム・パン操作
- 複数ファイルの切り替え
- `auto` モード: tmux ペインのセル座標をピクセル座標に変換して自動配置
- フレームバッファが上書きされたことを検出して自動再描画

## インストール

```sh
cargo build --release
sudo install -m 755 target/release/fbhalf /usr/local/bin/
```

外部ディスプレイ出力を使う場合は drm-output ヘルパーもビルド・インストールします（[必要なもの](#外部ディスプレイ-display-モード) 参照）:

```sh
make
sudo install -m 755 fbhalf-drm-output /usr/local/bin/
```

man ページを使う場合:

```sh
sudo install -m 644 man/fbhalf.1 /usr/local/share/man/man1/
sudo mandb
```

## 使い方

```
fbhalf [region] <file.png|file.jpg> [file2.png ...]
fbhalf                        # カレントディレクトリの *.{png,jpg} を auto モードで開く
fbhalf auto image.png         # tmux ペイン位置に合わせて表示
fbhalf left a.png b.png       # 画面左半分に表示
fbhalf tr screenshot.png      # 画面右上 1/4 に表示
fbhalf --display full a.png   # 外部ディスプレイ全体に表示
```

### region 指定

| region | 別名 | 説明 |
|--------|------|------|
| `full` | — | 画面全体 |
| `left` | — | 左半分 |
| `right` | — | 右半分 |
| `topleft` | `tl` | 左上 1/4 |
| `topright` | `tr` | 右上 1/4 |
| `bottomleft` | `bl` | 左下 1/4 |
| `bottomright` | `br` | 右下 1/4 |
| `auto` | — | tmux ペイン位置を自動検出 |

region を省略した場合、または認識できない引数が先頭にある場合は `auto` として扱われます。

## 外部ディスプレイ (`--display`) モード

`--display[=CONNECTOR]` を指定すると、`/dev/fb0` の代わりに DRM 経由で外部モニターに直接描画します（コネクタ省略時は `DP-1`）。

```sh
fbhalf --display full image.png         # DP-1 全画面に表示
fbhalf --display=DP-2 left a.png b.png   # DP-2 の左半分に表示
```

### 必要なもの

- `gcc`、`libdrm` (DRM 出力ヘルパー `fbhalf-drm-output` のビルド用)

```sh
make                                   # fbhalf-drm-output をビルド
sudo install -m 755 fbhalf-drm-output /usr/local/bin/
```

### 仕組み

`--display` 指定時、fbhalf は共有メモリ `/tmp/fbhalf-ext` の info ファイルを確認し、なければ `fbhalf-drm-output` ヘルパーを `sudo` 経由で自動起動します（パスワード入力が求められる場合があります）。ヘルパーは指定したコネクタに対して DRM の dumb buffer を確保し、共有メモリの内容を ~60fps でモニターへ転送し続けます。fbhalf 自身は通常モードと同じ描画コードで、書き込み先を `/dev/fb0` から共有メモリに切り替えるだけです。

ヘルパーは fbhalf と同じディレクトリ、または `PATH` 上から検索されます。見つからない場合はビルド方法を案内するエラーで終了します。

### キー操作

| キー | 動作 |
|------|------|
| `n` / `Space` | 次の画像 |
| `p` / `b` / `Backspace` | 前の画像 |
| `+` / `=` | ズームイン |
| `-` | ズームアウト |
| `0` | ズーム・パンをリセット |
| `h` / `←` | 左にパン |
| `l` / `→` | 右にパン |
| `k` / `↑` | 上にパン |
| `j` / `↓` | 下にパン |
| `:N` `Enter` | N ページ目へジャンプ（1 始まり）|
| `:p` `Enter` | 現在のページ番号を表示 |
| `q` / `Esc` | 終了 |
| `Ctrl-c` | 終了 |

## auto モードの仕組み

`$TMUX` が設定されている環境では、`tmux display-message` でペインのセル座標を取得し、フレームバッファの解像度とウィンドウサイズ（文字数）から 1 セルあたりのピクセル数を計算して描画領域を決定します。

また、500 ms ごとにペイン座標を再取得し、ペインサイズが変わったり別のプロセスがフレームバッファを上書きした場合は自動的に再描画します。

## 依存クレート

| クレート | 用途 |
|----------|------|
| [`image`](https://crates.io/crates/image) | PNG/JPEG を含む画像デコード |
| [`crossterm`](https://crates.io/crates/crossterm) | ターミナル raw モード・キーイベント |

## ライセンス

MIT
