# binary-size

feature 無効（既定）でリリースビルドしたバイナリのサイズを計測し、上限を
超えたら非ゼロ終了する検査。TASK-34.2（Issue #466）で導入。対応するビヘイビア
ID は `RENDER-2`（基準は `CORE-2` と同じ「Chromium 比 80% 以上削減」）。ID から
SSOT（`docs/spec` の `04-behavior/`）を参照すること
（[spec-reference](../../.claude/rules/spec-reference.md)）。

親 Issue #48（TASK-34）。兄弟 Issue は #465（TASK-34.1・依存グラフ検査）、#467（TASK-34.3・CI 組込み）、#468（TASK-34.4・確認記録）の 3 件。CI（GitHub Actions）への組込みは #467（TASK-34.3）で完了し、`.github/workflows/ci.yml` の `binary-size` ジョブ（3 OS）が実行する。`make ci` には組み込まない（下記「`make ci` に含めない理由」参照）。

## 計測対象と fail-closed 契約

既定 package `fandhe-browser-cli` は workspace に存在する（TASK-41.5・
Issue #174・#616 で追加済み）。package が workspace に無い場合は、既定値かどうかに
関係なく exit 2（入力・使用エラー）にする。cli の削除・リネームや
`--package` / `BINARY_SIZE_PACKAGE` の誤記を「未導入のためスキップ」として
黙って通過させないためである（REPAIR-5・#633）。計測できない状態
（`cargo build` 失敗・実行ファイル不在等）もすべて exit 2 になる。

## 上限値と根拠

`BINARY_SIZE_LIMIT_BYTES`（`Makefile`）の既定値は `91480000`（約 91.48MB）。

- `CORE-2` / `RENDER-2` の判定基準は「Chromium 比 80% 以上削減」。spec が基準に
  する Chromium 実測 457.4MB（PoC-2）の 20% は 91.48MB
- MB/MiB の曖昧さは安全側（小さい方）に倒し、10 進の 91,480,000 バイトを採用
  する
- PoC-6 実測の 424KB（`println!` だけの空に近い cli。依存ほぼゼロ・Servo 非
  リンク）は参考下限であり、ゲートにはしない。fetch・TLS・HTML パーサー・
  CDP サーバーを含む実バイナリでは到達しない水準のため。参考値として PoC-2 の
  プロトタイプ実測は 2.11MB
- JS エンジン: 現状の既定ビルド（`fandhe-browser-js` の `default = []`）は
  `js-v8` 無効で V8 を含まない。V8 を同梱した場合の増分は PoC-3 実測
  +40.81MB（TASK-27 の記述）で、同梱しても上限内に収まる見込み
- release プロファイル: 現状はルート `Cargo.toml` の `[profile.release]` が
  `panic = "abort"` のみで、`CORE-2` の前提（`opt-level = "z"`・`lto = true`・
  `codegen-units = 1`・`strip = true`）が未適用。計測値は正規構成より大きく
  出る（保守側）。この不整合はスコープ外として別途報告済み（下記「現状の
  限界」参照）
- 実バイナリができた後に、より厳しい回帰予算へ見直すかどうかは #468
  （TASK-34.4）が実測を見てから判断する

`BINARY_SIZE_LIMIT_BYTES ?= ...`（`?=`）のため、環境変数や `make` 引数
（`make check-binary-size BINARY_SIZE_LIMIT_BYTES=...`）で上書きできる。
現時点では OS ごとに上限を分ける必要はなく（全 OS 共通の `CORE-2` 水準）、
`binary-size` ジョブも 3 OS で同じ値を使う。分ける必要が出た場合は #468
（TASK-34.4）の実測後に検討する。

## 出力形式の契約（#467・#468 が読み取る）

判定した各バイナリにつき、次の形式の行を標準出力へ 1 行出す。

```text
binary-size: host=<triple> package=<pkg> bin=<basename> bytes=<N> limit=<M> result=<pass|fail>
```

`host` / `package` / `bin` の値はそれぞれ `^[A-Za-z0-9._-]{1,64}$` に一致する
ことを検証する（不一致は exit 2）。CI ログへの出力が GitHub Actions のワーク
フローコマンド（`::...`）として誤解釈されるのを防ぐためで、
[`harness/compat-regression/README.md`](../compat-regression/README.md) の
`cat` 検証（PR #452 の指摘）と同じ考え方。

## CLI

```bash
# 判定モード（既存ファイルを直接判定。self-test 用）
harness/binary-size/check-binary-size.sh \
  --file <path> \
  --limit <bytes> \
  [--host <triple>] \
  [--bin <name>]

# package モード（metadata 確認 → リリースビルド → 判定）
harness/binary-size/check-binary-size.sh \
  --package <name> \
  --limit <bytes> \
  [--host <triple>]
```

- `--file` / `--package` は排他（どちらか一方が必須）
- `--limit`: 上限バイト数（必須）。`^[0-9]{1,15}$` かつ 1 以上（桁数を制限し、
  bash の算術比較がオーバーフローしないようにするため）
- `--host`: 出力行の `host` に使う target triple。省略時は `rustc -vV` の
  `host:` 行から取得する
- `--bin`: 判定モードの `bin` ラベル。省略時は `--file` のベース名
- package モードは既定 feature のまま `cargo build --release -p <name>` を
  実行する（`--features` / `--all-features` は付けない。`RENDER-2` の
  「feature 無効」に当たる）。1 つの package に bin target が複数あれば
  すべて判定し、1 つでも上限を超えれば exit 1 にする
- 実行ファイルパスは `target/release/<name>` をハードコードせず、
  `cargo build --message-format=json-render-diagnostics` の出力を jq で読み
  `reason == "compiler-artifact"` かつ `.executable != null` の値を使う
  （`CARGO_TARGET_DIR` 環境変数・Windows の `.exe` 拡張子・bin 名未確定を
  まとめて吸収するため）
- Windows（Git Bash）対策: jq の出力は `tr -d '\r'` で CR を除去し、
  `cygpath` があれば `cygpath -u` で POSIX パスへ変換する

## 終了コード

| exit | 意味 |
| ---- | ---- |
| 0 | 合格（すべてのバイナリが上限以下） |
| 1 | 不合格（1 つ以上のバイナリが上限を超えた） |
| 2 | 入力・使用エラー（引数不正・`--limit`/`--host`/`--bin`/`--package` の形式不正・ファイル不在・`cargo`/`jq`/`rustc` 未導入・`cargo metadata`/`cargo build` 失敗・package が workspace に無い・bin target 不在・実行ファイル抽出結果 0 件） |

サイズが上限ちょうど（`bytes == limit`）は合格（「超えたら」fail）。

## ローカル実行

```bash
make check-binary-size
```

内部で `self-test.sh`（合成ファイルによる判定モードの自己テスト。
`head -c N /dev/zero` で生成。macOS 標準に `truncate` が無いため使わない）を
実行してから、`check-binary-size.sh --package "$(BINARY_SIZE_PACKAGE)" --limit
"$(BINARY_SIZE_LIMIT_BYTES)"` を実行する。`jq` が未導入の場合は
`check-compat-regression` と同じ方針で fail-closed にする（silent skip に
しない）。

## CI（`.github/workflows/ci.yml` の `binary-size` ジョブ）

TASK-34.3（#467）で導入。3 OS（ubuntu/macos/windows）の各ネイティブランナーで
実行し、`make` は使わない（windows-latest に `make` がある保証がないため。
`compat-regression`・`bench-record-selftest` と同じ方針でスクリプトを直接
呼ぶ）。cache は使わない（windows-latest には cache prune の既知問題があるため。
cache 導入は別途検討する）。

上限値・package 名は `Makefile` の `BINARY_SIZE_LIMIT_BYTES` /
`BINARY_SIZE_PACKAGE` を単一真実源とし、ジョブ側では値を重複定義しない。
行頭固定の正規表現でちょうど 1 件だけ抽出し、0 件（削除・書式変更）・2 件
以上（曖昧）はいずれも `::error::` を出して fail-closed にする。

self-test（`self-test.sh`）を実判定の前に毎回実行し、上限超過（合成
201 bytes > limit 200）で確実に exit 1 になることをジョブログへ証跡として
残す。実判定の出力（`binary-size: ...` 行）はジョブログとステップサマリー
（表形式）の両方に出す。

branch protection の必須チェックに加える場合は OS 数分
（`binary-size (ubuntu-latest)` / `(macos-latest)` / `(windows-latest)` の
3 件）を登録する必要がある（ユーザー作業）。

## `make ci` に含めない理由

`make ci`（集約ターゲット）の依存には `check-binary-size` を追加していない。
`ci:` を実行するたびにリリースビルド（`cargo build --release`）が走るとコスト
が大きいため（判断済み）。CI での継続的なゲートは上記 `binary-size` ジョブが
担い、ローカルでは必要に応じて `make check-binary-size` を個別に実行する。

## 現状の限界

- release プロファイルが `CORE-2` の前提（`opt-level = "z"`・`lto = true`・
  `codegen-units = 1`・`strip = true`）を満たしていない（ルート `Cargo.toml`
  の `[profile.release]` は `panic = "abort"` のみ）。workspace 全体に影響する
  変更のためユーザー判断のうえ別 Issue で扱う（関連: TASK-27 #146、
  TASK-94.1 #368）
- spec（`js-engine.md`・TASK-27）は「既定ビルド = V8 のみ同梱」とするが、
  `fandhe-browser-js` は `default = []`（`js-v8` は既定で無効）。spec と実装の
  食い違いはユーザーへ報告済み
