# Linux 実機での SSIM 測定 実行手順書（#55）

TASK-37（37.h1）/ MS-1 / ビヘイビア `RENDER-5`・`MEAS-3` 対応。Issue #55 の
実機作業（担当: 人間）の手順書である。**この文書は手順の草案であり、測定結果ではない**。
実機での撮影・測定・合否判定はオーナーが行い、結果は #55 に記録する。

ハーネスの仕様（引数・スキーマ・セキュリティ上の注意）は [README.md](README.md) が
正本で、本書は「実機で何をどの順に実行するか」だけを扱う。

## 1. 目的と合否基準

`RENDER-5` の判定手順に従い、代表 5 サイト以上で Servo と Chromium の PNG を撮って比べる。

| 指標 | 合格基準 |
| ---- | -------- |
| SSIM | サイトごとに 0.90 以上 |
| 境界ボックス | 主要レイアウト要素の 80% 以上が、Chromium 側の位置から ±5%（viewport 寸法比）以内 |
| サイト数 | SSIM と境界ボックスの両方を満たすサイトが 5 以上（`--min-sites 5`） |

`measure_ssim.py` の `verdict` は閾値との機械的な比較に過ぎない。最終合否は、
本書の記録テンプレートを埋めたうえで #55 に記載する（README の「±5%・80% の解釈」も確認する）。

## 2. 前提

| 項目 | 内容 |
| ---- | ---- |
| OS | Linux x86_64（TASK-36 / #50 で検証に使った Linux 実機と同じ環境が望ましい） |
| GPU | なし。Mesa の llvmpipe（ソフトウェア GL）で描画する。フォント差などで SSIM が下がり得る点に注意 |
| ネットワーク | 代表サイト（`sites.json`）へ HTTPS で到達できること。到達不能なサイトは `skipped` / `failed` になり、5 サイト未満だと終了コード 1 になる |
| ツール | `python3`（標準ライブラリのみで動作）、Rust stable（`rust-toolchain.toml`）、`cmake` と C/C++ ビルド環境（Servo のネイティブ依存に必要。PoC-6 の記録による） |
| ディスク | Servo の release ビルドで `target/` が数 GB（PoC-6 の macOS 実測で約 2.4GB。Linux での診断版ビルドは約 4 分。#50 報告） |

撮影前に `python3 -m unittest discover -s harness/render-screenshot -p 'test_*.py' -v`
を実行し、ハーネス自体が実機の Python で動くことを確認する（偽エンジンによるオフライン
テストであり、実機撮影の検証ではない）。

## 3. Chromium の導入

`capture_screenshots.py` は `chromium` / `chromium-browser` / `google-chrome` の順で自動検出する。
どれも無ければディストリビューションのパッケージで導入し、`--chromium-bin` に明示してもよい。

```bash
# 例（Debian / Ubuntu 系。パッケージ名はディストリビューションで異なる）
sudo apt-get install -y chromium
chromium --version   # 結果記録用にバージョンを控える
```

root 実行やコンテナでは `--no-sandbox` が必要になることがある。その場合は既定テンプレートに
勝手に足さず、`--chromium-cmd` で明示的に指定する（README「Chromium の既定テンプレート」）。
UA の上書きや anti-bot 回避のフラグは付けない（SEC 系）。

## 4. Servo 側の撮影コマンドの用意

### 4.1 背景（#50 の報告）

TASK-36（#50）の実機検証では、PoC-6 の `servo-embed` を改変せずに実行すると、8 回すべて
60 秒でタイムアウトし PNG を取得できなかった。原因は、埋め込み側の `WebViewDelegate` が
`notify_new_frame_ready` を実装せず `WebView::paint()` を呼んでいないため、スクリーンショットの
callback が呼ばれないことと推定されている（Servo 0.3.0 の描画モデル）。`paint()` を呼ぶ
4 行を足した診断用コピーでは 7/7 回、1280x800 の PNG を 0.1〜0.8 秒で取得できた。
**Servo 本体（`servo` クレート）は改変していない**（licensing 規約: Servo のソースを改変しない）。

### 4.2 診断版 servo-embed の作り方

改変するのは埋め込み側のサンプルコード（`servo-embed`）だけで、Servo 本体は触らない。

1. `servo-embed` を作業用ディレクトリへ複製する。ソースは private な spec リポの
   `03-poc/rendering-layer-servo/servo-embed/`（アクセス権のある環境では `docs/spec` submodule 配下）。
   `docs/spec` 配下は本リポから編集しないので、**必ず複製して使う**。複製には `Cargo.lock` も含める。
2. 複製先の `src/main.rs` の `impl WebViewDelegate for Delegate` に、次のメソッドがあることを確認する。
   無ければ追加する（#50 の診断差分と同じ内容）。

   ```rust
   // 新フレーム通知で paint() を呼ぶ（Servo 0.3.0 の描画モデル）。
   fn notify_new_frame_ready(&self, webview: WebView) {
       webview.paint();
   }
   ```

   spec 側の最新版には既に同等の記述が入っている可能性がある。その場合は追加不要。
3. 本リポの依存管理とは無関係の独立ビルドとして、別の `CARGO_TARGET_DIR` でビルドする。

   ```bash
   cd /path/to/servo-embed-copy
   CARGO_TARGET_DIR=/path/to/servo-target cargo build --release
   ```

   本リポの `Cargo.toml` / `Cargo.lock` は変更しない（依存は `=x.y.z` 固定・承認制。
   複製側の依存は PoC の `Cargo.lock` に従う）。
4. 単体で PNG が出ることを確認してから、ハーネスに繋ぐ。

   ```bash
   /path/to/servo-target/release/servo_embed_poc page.html out.png
   file out.png   # PNG image data, 1280 x 800, 8-bit/color RGBA を確認
   ```

   無表示環境で GL 初期化に失敗する場合は、#50 で成功した条件を使う:
   `LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe`、`EGL_PLATFORM=surfaceless`、
   または `xvfb-run -a -s "-screen 0 1280x800x24"`。

### 4.3 servo-embed の入出力仕様と制約

- 引数は `servo_embed_poc <html_path> <out.png>` の位置引数 2 つのみ（`--proxy` 等のオプションは無い）。
- 出力寸法は 1280x800 固定。`sites.json` の viewport（1280x800）と一致するので追加指定は不要。
- 入力は HTML ファイル（`data:` URL として読み込む）。ハーネスの `{html_path}` placeholder が
  この用途にあたり、撮影直前に対象 URL の HTML をスナップショットとして 1 回取得して渡す。
  Chromium は `{url}` を直接開くため、**両エンジンの入力条件が一致しない**
  （スナップショットは外部 CSS・画像・スクリプトを含まない HTML 本体のみ）。
  この差が SSIM に与える影響は測定時に必ず記録する（5 章の記録テンプレート）。
- `servo-embed` はプロキシ引数を持たない。ハーネスは `{proxy}` を含まない `--servo-cmd` を
  既定で拒否するため、Servo 側の撮影は **`--allow-unproxied-engine` を付けて実行する**
  （オーナー判断 2026-10-10・#55）。
  これはネットワーク全通信をローカル転送プロキシ越しにフィルタする安全策を Servo 側で外すことを意味する。
  採用理由は、実機測定が一度きりであること。**対象は `sites.json` の公開サイトのみに限り、
  認証情報・社内/ローカルのアドレスを含む URL では使わない**。

## 5. 撮影（`capture_screenshots.py`）

作業用の出力先を決め、まず `--dry-run` で展開後のコマンドを確認する。

```bash
OUT=/path/to/render-out
SERVO_BIN=/path/to/servo-target/release/servo_embed_poc

python3 harness/render-screenshot/capture_screenshots.py \
  --out-dir "$OUT" \
  --engines servo,chromium \
  --servo-cmd "[\"$SERVO_BIN\", \"{html_path}\", \"{out}\"]" \
  --allow-unproxied-engine \
  --dry-run
```

内容に問題がなければ `--dry-run` を外して実行する。主な引数（正本は README の「CLI 引数」）:

| 引数 | 用途 | 既定 |
| ---- | ---- | ---- |
| `--out-dir` | 出力先（必須）。`servo/` `chromium/` `snapshots/` と `capture-result.json` を作る | なし |
| `--engines` | `servo,chromium`（RENDER-5 の判定には両方が必要） | `servo,chromium` |
| `--servo-cmd` | Servo 側コマンドテンプレート（JSON 配列文字列。`{html_path}` `{url}` `{out}` `{width}` `{height}` `{proxy}` 等が使える） | なし（必須） |
| `--chromium-bin` / `--chromium-cmd` | Chromium の実行ファイル / テンプレートの上書き | 自動検出 / 既定テンプレート |
| `--timeout-sec` | 1 回の撮影のタイムアウト | 90 |
| `--settle-ms` | 描画の待ち時間 | 5000 |
| `--min-sites` | 両エンジンで共通して `ok` になるべき最低サイト数 | 5 |
| `--allow-unproxied-engine` | `{proxy}` 無しのテンプレートを許可（4.3 の決定: Servo 側撮影で使用） | 無効 |

終了コードは、両エンジンで共通して `ok` のサイト数が `--min-sites` 以上なら 0、未満なら 1、
引数不正なら 2。`capture-result.json` の `captures[].status`（`ok` / `failed` / `timeout` /
`skipped`）と `stderr_tail` で失敗要因を確認し、結果記録に残す。

## 6. SSIM・境界ボックスの算出（`measure_ssim.py`）

```bash
python3 harness/render-screenshot/measure_ssim.py \
  --capture-dir "$OUT" \
  --min-sites 5
```

`$OUT/measure-result.json` にサイトごとの SSIM 値と境界ボックス一致率が出る。終了コードは
基準を満たすサイトが `--min-sites` 以上なら 0、未満なら 1、入力不正なら 2。

SSIM は Chromium を reference、Servo を target とした 7x7 一様窓の mean SSIM で、
他ツール（scikit-image 等）の値とは一致しない可能性がある（README「SSIM の算出パラメータ」）。

### 6.1 境界ボックスの入力（要判断）

`measure_ssim.py` は境界ボックスの**抽出**を行わない。エンジンごとに
`$OUT/<engine>/<site_id>.bboxes.json`（`png_sha256`・`elements` を持つ。契約は README の
「境界ボックスの入力契約」）を用意する必要があり、無いサイトは `not_measured` で不合格になる。

- 1 サイトあたり主要要素 5〜10 件（Chromium 側が 5 件未満だと不合格）。要素の選定基準は未定。
- Chromium 側は CDP の `DOM.getBoxModel` 等で取得できる見込みだが、取得スクリプトは本リポに無い。
- Servo 側は、`servo-embed` に境界ボックスを出す API / 手段が無い（TASK-36 / TASK-38 の成果次第）。

**境界ボックスの取得方法（Servo 側の手段、対象要素の選定基準）は未決定（オーナー判断待ち）**。
取得できない間は SSIM のみを先に測定し、境界ボックスは「未測定」として記録する（合格を
装わない。REPAIR-3）。

## 7. 結果記録テンプレート

Issue #55 へ貼る。数値は `measure-result.json` から転記する。

```markdown
## 測定環境

| 項目 | 値 |
| ---- | -- |
| 測定日 | YYYY-MM-DD |
| OS / カーネル | |
| CPU / メモリ | |
| GL | llvmpipe（Mesa x.y.z） |
| Chromium | バージョン |
| Servo | 0.3.0（servo-embed 診断版。paint() 追加の有無: あり / なし） |
| ハーネス | コミット（git rev-parse --short HEAD） |
| 実行コマンド | capture_screenshots.py / measure_ssim.py の引数一式 |
| viewport / settle | 1280x800 / 5000ms |

## 結果

| site_id | 撮影(servo) | 撮影(chromium) | SSIM | bbox 一致 (matched/total) | bbox 率 | 合格 |
| ------- | ----------- | -------------- | ---- | ------------------------- | ------- | ---- |
| a1-wikipedia | ok | ok | 0.00 | 0/0 | 0% | 否 |
| ... | | | | | | |

- 両基準を満たしたサイト数: N / 5（基準 5 以上）
- 入力条件の差（Servo: スナップショット HTML / Chromium: URL 直接）の影響: 記述
- 失敗・skipped のサイトと stderr_tail の要点: 記述
- 境界ボックスの取得方法と要素選定: 記述（未測定の場合はその旨）

## 判定

RENDER-5: 達成 / 未達 / 一部のみ測定（理由）
```

## 8. 判定後の分岐

| 結果 | 次の対応 |
| ---- | -------- |
| 達成（5 サイト以上で両基準） | 結果を #55 に記録して受け入れ条件を満たす。TASK-40（#57）は「不要」と記録する方向（判断はオーナー） |
| 未達 | #55 に未達と原因（フォント差・入力条件差・描画差など）を記録し、#57（TASK-40: レンダリング層 MVP スコープの代替方針）へ進む。Blitz の追加評価（#56 の評価コメント参照）を含む |
| 一部のみ測定（境界ボックス未取得など） | 測定できた範囲と未測定の理由を記録し、残りの扱いをオーナーが決める |
