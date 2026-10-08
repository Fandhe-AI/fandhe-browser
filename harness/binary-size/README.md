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
- JS エンジン: 既定ビルドは `fandhe-browser-cli` の `default = ["js-v8"]`
  （TASK-30.2・#160）により V8 を同梱する。V8 の増分は PoC-3 実測
  +40.81MB（TASK-27 の記述）で、上限内に収まるかは `make check-binary-size`
  の実測で判定する
- release プロファイル: ルート `Cargo.toml` の `[profile.release]` は #368
  （TASK-94.1）で `opt-level = "z"`・`lto = true`・`strip = true` を適用済みだが、
  `codegen-units = 1` は未適用（下記「現状の限界」参照）。#368 以前の計測値は
  適用前の値で、大きめ（保守側）に出ている
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

## JS エンジン構成別サイズ計測（TASK-31.1・#469）

`measure-js-engine-configs.sh` が JS エンジン構成ごとのリリースバイナリサイズを
計測する（`JS-1` ビルド構成表・`JS-3`・`PERF-1`・MS-3）。出力は後続の Issue #470
（TASK-31.2・測定レポート）が読む。**計測のみでゲートではない**（閾値判定は
Issue #470、既定ビルドの上限ゲートは上記 `check-binary-size.sh`）。

| config | cargo 引数 | features ラベル | 期待エンジン |
| ------ | ---------- | --------------- | ------------ |
| `default` | （なし） | `default` | `v8` |
| `boa` | `--no-default-features --features js-boa` | `no-default-js-boa` | `boa` |
| `none` | `--no-default-features` | `no-default` | `none` |

### 出力形式の契約

構成ごとに bin target 1 つあたり 1 行を標準出力へ出す（Markdown 表等の補助出力は
契約外）。

```text
js-binary-size: config=<default|boa|none> features=<label> engines=<v8|boa|none> os=<uname -s> host=<triple> target=<triple> rustc=<release> rustc_commit=<hash> cargo=<release> profile=release strip=<none|symbols> package=<name> bin=<name> bytes=<N> mb=<X.XX>
```

- `mb` は 10 進（10^6 bytes）で小数第 2 位まで
- `host` と `target` は常に `rustc -vV` の host（`--target` は渡さない。各 OS の
  ネイティブビルドのみ。クロスコンパイルは対象外）
- `strip` は既定 `none`。`--strip`（`make ... JS_BINARY_SIZE_STRIP=1`）で
  `CARGO_PROFILE_RELEASE_STRIP=symbols` を `cargo build` にだけ渡す（`Cargo.toml`
  は編集しない。profile が変わるため全体の再ビルドが走る。spec の strip 後の
  参考値と比べるための任意モード）
- 全フィールドは `^[A-Za-z0-9._-]{1,64}$` で検証してから出力する（ワークフロー
  コマンド誤解釈の防止）

### 終了コード

| exit | 意味 |
| ---- | ---- |
| 0 | 3 構成すべてを計測でき、陽性対照も一致した |
| 1 | 陽性対照の不一致（feature 連鎖 cli → core → js が壊れ、期待外のエンジンが同梱された） |
| 2 | 使用エラー・計測不能（引数不正・`cargo` / `jq` / `rustc` 未導入・build / metadata 失敗・package / bin 不在・実行ファイル 0 件） |

陽性対照は `cargo build` の JSON から `v8` / `boa_engine` の lib artifact の有無を
見て、構成と同梱エンジンの対応を確かめる。不一致でも残りの構成は計測してから
exit 1 にする（全体像をログに残すため）。3 構成は同じ `target/release/<bin>` を
上書きするため、ビルド直後に逐次計測する。

### ローカル実行

```bash
make measure-js-binary-size                          # 自己テスト → 3 構成の計測
make measure-js-binary-size JS_BINARY_SIZE_STRIP=1   # strip=symbols で計測
```

V8 を含むビルドは時間がかかる。`make ci` と CI には含めない（コストが大きく、
ゲートでもないため）。3 OS での実測は #470 のレポート作成時に各 OS で手動実行する。
macOS の bash 3.2・Windows の Git Bash 向けに bash 4 系機能は使っていないが、
本リポジトリのローカル環境（Linux）以外での動作は未確認。

## 現状の限界

- release プロファイルのうち `codegen-units = 1` が未適用
  （`opt-level = "z"`・`lto = true`・`strip = true` は #368 で適用済み）。
  workspace 全体に影響する変更のためユーザー判断のうえ別 Issue で扱う
  （関連: TASK-27 #146、TASK-94.1 #368）
- spec（`js-engine.md`・TASK-27）の「既定ビルド = V8 のみ同梱」は、
  `fandhe-browser-cli` の `default = ["js-v8"]`（TASK-30.2・#160）で解消済み。
  `fandhe-browser-js` / core の `default = []` は意図どおり（feature 統合で
  軽量ビルドから V8 を外せなくなるのを防ぐため）
