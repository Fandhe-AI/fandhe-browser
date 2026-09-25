# レンダリング feature（`rendering`）有効時のサイズ増分・ビルド時間

対応 `TASK-35` / `MS-1` / ビヘイビア `RENDER-3`（[spec-reference](../../.claude/rules/spec-reference.md)）。

## 目的

feature `rendering`（`Servo` 組込）を有効化した場合のバイナリサイズ増分・ビルド時間を数値化し、
`fandhe-browser-render` crate をオプトイン配布（別バイナリ／別コンテナイメージ）にする設計判断の材料とする。
本ドキュメントは新規の実測を行うものではなく、既存 PoC（PoC-6）の実測値を出典明記のうえ整理したものである。
リポジトリに `fandhe-browser-render` crate・`fandhe-browser-cli` crate が未作成のため、
本リポジトリ自身での `cargo build --release --features rendering` 実測はまだ実施できない。

## バイナリサイズ増分

| 構成 | サイズ | 出典 |
| ---- | ------ | ---- |
| 二層構成 `cli`（feature `rendering` 無効） | 424KB | PoC-6 §3 実測（`docs/spec/03-poc/rendering-layer-servo/README.md`、RENDER-2 と同一値） |
| `servo-embed`（`Servo` 0.3.0 を実際に埋め込んだバイナリ、feature 有効相当） | 130MB（`136,414,768` バイト、macOS arm64、strip 未設定） | PoC-6 §1 実測 |
| 差分（130MB − 424KB） | 約 130MB | 上記 2 値の単純差分（**参考値**。別サンプル間の差分であり RENDER-3 が求める「feature 有効時と無効時のバイナリサイズ差分」の実測値ではない） |

- **両者は別サンプルでの実測**である。二層構成 `cli` に feature `rendering` を有効化したフルビルドは PoC-6 では未実施であり（`servo` 本体のフルビルドが必要になるため実施を見送った）、有効時のサイズは `servo-embed` 実測（130MB 水準）で代替している。二層構成 `cli` の feature 有効時実測は本リポジトリで `fandhe-browser-render` crate 実装後にあらためて行う必要がある
- `servo-embed` の初回ビルド（`println!` のみの Hello World、`Servo` API 未使用）も奇しくも 424KB になっているが、これはリンカが未使用コードを除去し `Servo` が実際にはリンクされていないためであり、二層構成 `cli` の feature 無効時サイズ（RENDER-2）とは無関係の値である。したがって初回ビルドの 3 分 23 秒（下記）は「`Servo` を実際にリンクしたビルド時間」ではない点に注意する
- 参考: 本リポジトリの想定既定 CLI（V8 同梱）の実サイズは約 42.92MB（`docs/spec/spec.md`、JS-3・PERF-1 の照合対象）であり、PoC-6 の 424KB スタブより大きい。`fandhe-browser-render` crate 実装後の実際の増分倍率は、この約 42.92MB を基準にすると PoC-6 の相対比（424KB → 130MB、約 306 倍＝約 30,600% 増）より小さくなる見込みである

## ビルド時間

PoC-6 実測（macOS 26.5.1 arm64、Rust 1.96.0、`cmake` 導入済み）。

| ビルド | 所要時間 | 備考 |
| ------ | -------- | ---- |
| 初回ビルド（Hello World、`Servo` API 未使用） | 3 分 23 秒 | `Servo` は未リンク（上記参照） |
| 埋込コードへの書き換え後の再ビルド（`image`・`url`・`dpi`・`servo-embedder-traits`・`servo-paint-api`・`rustls` 追加、レジストリキャッシュ温存） | 1 分 49 秒 | 実際に `Servo` をリンクした最初のビルド |
| インクリメンタルビルド | 23.89 秒／7.24 秒 | 2 回計測 |

- 前提条件: `cmake`（`Servo` のネイティブ依存 `mozjs_sys`・`harfbuzz-sys` 等のビルドに必須。未導入時は `cmake not found` でビルド不可）
- `target/` ディレクトリ総サイズ: 初回ビルド時点で約 2.4GB（release ビルド 1 系統のみ）

## Linux 実測（RENDER-4・受け入れ条件 3）

**現状: 未達・TASK-36（Issue #50）実測待ち。** Issue #50（TASK-36: Linux 実環境での `Servo` ヘッドレススクリーンショット取得検証）は本ドキュメント作成時点（2026-09-25）で OPEN のままであり、Linux 実環境でのメモリ増分・レンダリング時間の実測値はまだ存在しない。

これまでに判明している未達の実績は次の 2 件（`docs/spec/04-behavior/rendering-layer.md` RENDER-4、`measurement-infra.md` MEAS-3）。

- **PoC-6（macOS サンドボックス）**: `servo-embed` の埋込コードでヘッドレススクリーンショット取得を試みたが、`data:` URL・`about:blank` いずれも 60 秒タイムアウトで `LoadStatus::Complete` に到達しなかった（全 45 スレッドが `crossbeam_channel` の待機状態、CPU 使用率ほぼ 0）。原因は環境固有（GPU/WindowServer アクセス制限等）の可能性が高いが未確定
- **PoC-13（Docker Linux、`docs/spec/03-poc/browser-landscape-2026/README.md`、2026-09-23）**: `simonw/research` の `servo-shot`（別実装ソース、MPL-2.0、`servo` 0.1.0）を `rust:1-bookworm`（`linux/arm64` ネイティブ）でビルドしたところビルド自体は 3 分 15 秒・151.6MB で成功したが、ヘッドレス PNG 取得は `Error: timed out waiting for a post-load frame (saw 2)` で失敗した

PoC-13 は本ドキュメントが対象とする PoC-6 の `servo-embed` 実装（`servo` 0.3.0）とは別ソース・別バージョンであるため、サイズ・ビルド時間の数値としては RENDER-3 の実測に含めない（参考情報として記載するに留める）。macOS・Docker Linux の 2 環境で症状は異なるが同種の「ヘッドレス環境でのピクセル読み出しが安定しない」問題が確認されているため、TASK-36 は単純な再実行ではなく `servo-shot` 方式そのものの原因切り分けが必要とされている（`docs/spec/06-roadmap.md`）。

Issue #50 がクローズされ実測が確定した際は、本セクションを実測値で更新する。

## 将来の追記予定

メモリ増分・ページあたりレンダリング時間は TASK-82（`docs/spec/05-tasks.md`「レンダリング有効時のメモリ増分・レンダリング時間の数値化」、TASK-36 とは別タスク）で Linux 実機測定のうえ本ドキュメントへ追記される予定。現時点ではプレースホルダとする。

- メモリ増分: 未測定（TASK-82 待ち）
- ページあたりレンダリング時間: 未測定（TASK-82 待ち）

## オプトイン配布方式の比較

RENDER-3 が求める設計判断材料として、`fandhe-browser-render` crate（`Servo`、MPL-2.0）を配布する 3 方式を比較する。Servo（MPL-2.0）は `fandhe-browser-render` crate 配下に隔離する方針（[licensing](../../.claude/rules/licensing.md)）を前提とする。コンテナは `RENDER-3` の対象外と `docs/spec/04-behavior/container-cloud.md` が明記しており、比較表の「別コンテナイメージ」列はその前提を踏まえた設計候補として参考記載する。

| 方式 | サイズへの影響 | ビルド時間への影響 | 配布の複雑さ | MPL-2.0 隔離との整合 |
| ---- | -------------- | ------------------ | ------------ | --------------------- |
| 単一バイナリ同梱（既定ビルドに `rendering` feature を常時含める） | 全利用者が約 130MB 増分を負担する | 全ビルドで `Servo` の依存解決・リンクが発生し、CI 時間が伸びる | 配布物は 1 種類で単純だが、レンダリング不要な利用者にも肥大化したバイナリを配る | crate 分離自体は保てるが、既定ビルドの依存グラフに `Servo`（MPL-2.0）が常時混入し RENDER-1 の要求（feature 無効時に依存グラフへ出現しない）と両立しない |
| 別バイナリ（`--features rendering` を有効化した別ビルド成果物を用意） | 既定バイナリは本リポジトリの想定既定 CLI（V8 同梱）サイズである約 42.92MB 水準を維持できる見込み。レンダリング対応バイナリの増分・配布物サイズは**未測定**であり、`servo-embed`（別サンプル、feature 有効時相当）の実測 130MB 水準を参考サンプル値として挙げるに留める（PoC-6 の 424KB も二層構成 CLI という別サンプルの参考値であり、いずれも既定バイナリ・レンダリング対応バイナリ双方の見積もりに直接用いない） | 既定ビルドの CI は軽量なまま維持できる見込みだが、レンダリング対応ビルドの追加 CI 時間は未測定 | 配布物が 2 種類に増え、利用者はどちらを取得するか選択する必要がある | RENDER-1（feature gate による依存分離）とそのまま整合する。Servo の隔離も crate 境界のまま維持できる |
| 別コンテナイメージ（既定コンテナと Servo 同梱コンテナを分ける） | イメージ単位でサイズを分離でき、既定コンテナは軽量ビルド・互換ビルドのまま維持できる | 既定コンテナのビルドは変わらず、レンダリング対応コンテナのみ追加ビルドが必要 | イメージが 2 種類に増えるが、コンテナ運用（Kubernetes 等）では既存のイメージタグ切替の仕組みで吸収しやすい | `container-cloud.md` がレンダリング層を対象外と明記しており、本方式の詳細設計はコンテナ側のドキュメントで別途扱う（本書は候補として記載するに留める） |

現時点の参考サンプル実測（二層構成 `cli` 424KB・`servo-embed` 130MB、いずれも別サンプルでの実測でありレンダリング対応バイナリそのものの増分ではない点に注意。ビルド時間は `servo-embed` の 1 分 49 秒〜3 分 23 秒）を踏まえると、既定ビルド・既定コンテナの軽量性（RENDER-1・RENDER-2・PERF-1〜2）を損なわない「別バイナリ」または「別コンテナイメージ」がオプトイン配布の有力候補であり、「単一バイナリ同梱」は既定ビルドの軽量性方針と矛盾するため採用しない方向性が妥当と考えられる。ただし二層構成 `cli` に feature `rendering` を実際に有効化した場合のサイズ増分・配布物サイズ・ビルド時間は本ドキュメント作成時点で**未測定**であり、`fandhe-browser-render` crate 実装後の実測値をもって最終確認する。最終決定は TASK-38（レンダリング層本実装、TASK-36 の結論を踏まえる）で行う。

## 受け入れ条件との対応

- feature 有効時と無効時のバイナリサイズ差分: **未達**。二層構成 `cli` に feature `rendering` を実際に有効化したビルドは本ドキュメント作成時点で未実施であり、同一バイナリでの差分実測値は存在しない。424KB（二層構成 `cli`、feature 無効）と 130MB（`servo-embed`、別サンプル）の差分・約 130MB は**参考値**として記載するに留まる。`fandhe-browser-render` crate 実装後、同一構成での feature 有効・無効のビルドを比較して実測するまで本受け入れ条件は未達とする
- feature 有効時のビルド時間: **未達**。列挙した 3 分 23 秒（`Servo` 未リンクの Hello World ビルド）・1 分 49 秒（依存キャッシュを温存した別コードへの再ビルド）・23.89 秒／7.24 秒（インクリメンタル）はいずれも `servo-embed`（別サンプル）の**参考測定**であり、対象の二層構成 `cli` に feature `rendering` を有効化した場合のビルド時間はまだ測定していない。同一構成での実測を待って本受け入れ条件を達成とする
- TASK-36 の実測結果（成否）の反映: **部分達成**。Issue #50（TASK-36）が本ドキュメント作成時点で OPEN のため、Linux 実環境でのヘッドレススクリーンショット取得・メモリ増分・レンダリング時間は「未達・実測待ち」として現状（PoC-6 macOS 未達・PoC-13 Docker Linux 未達）を記載するに留めた。Issue #50 クローズ後に本ドキュメントを更新する

## 出典

- `docs/spec/05-tasks.md` TASK-35・TASK-36・TASK-82
- `docs/spec/06-roadmap.md` MS-1
- `docs/spec/04-behavior/rendering-layer.md` RENDER-1〜7
- `docs/spec/04-behavior/measurement-infra.md` MEAS-3
- `docs/spec/04-behavior/container-cloud.md`（コンテナは RENDER-3 対象外の明記）
- `docs/spec/spec.md`（既定ビルドの実サイズ参考値、JS-3・PERF-1）
- `docs/spec/03-poc/rendering-layer-servo/README.md`（PoC-6 実測本体）
- `docs/spec/03-poc/browser-landscape-2026/README.md`（PoC-13、参考記載）
