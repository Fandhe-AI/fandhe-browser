# V8 統合後バイナリサイズ測定レポート

- **対応ビヘイビア**: JS-3・PERF-1（いずれも spec 上「検討中」。`docs/spec/04-behavior/js-engine.md`・`perf-targets.md`）。判定基準は CORE-2（`core-dom.md`）。関連: JS-1・RENDER-2
- **対応タスク**: TASK-31（サブ 31.1・31.2）
- **マイルストーン**: MS-3
- **基準コミット**: ローカル計測は `3a298f3`（`origin/main` HEAD）。3 OS の既定ビルド値は CI run `37133473120`（`main` の `43987c4`・2026-10-03）
- **測定日**: 2026-10-05（ローカル計測）
- **測定環境（ローカル）**: Linux 7.0.0-34-generic / `x86_64-unknown-linux-gnu` / rustc 1.98.1（`48a229cea`）/ cargo 1.98.1

## 目的・前提

JS-3・PERF-1 が「検討中」に留まる理由は、V8 統合後の実バイナリでサイズを再計測していないことである。
本レポートはその実測記録で、TASK-31.1（Issue #469・PR #667）の計測ハーネスと CI の
`binary-size` ジョブの結果を使う。

- 判定対象は既定ビルド（V8 のみ同梱）のリリースバイナリ。基準は CORE-2 の
  「Chromium 本体 457.4MB（PoC-2 実測）比 80% 以上削減」、つまり 91,480,000 B 以下
  （`Makefile` の `BINARY_SIZE_LIMIT_BYTES` と同値）
- 軽量ビルド（boa のみ）・エンジンなしビルドは参考値
- MB は 10 進（10^6 B）
- ルート `Cargo.toml` の `[profile.release]` は `panic = "abort"` のみで、CORE-2 の前提
  （`opt-level = "z"`・`lto = true`・`codegen-units = 1`・`strip = true`）は未適用
  （#146・#368 で追跡）。本レポートの値は正規構成より大きい側（保守側）に出る

## 測定方法

```bash
# strip なし（既定。`--release` のままのバイナリ）
make measure-js-binary-size

# strip あり（ハーネスが `strip` したコピーのサイズを測る）
make measure-js-binary-size JS_BINARY_SIZE_STRIP=1
```

ハーネスは `harness/binary-size/measure-js-engine-configs.sh`。詳細は
`harness/binary-size/README.md` を参照。

| config | cargo の feature 指定 | 同梱エンジン |
| ------ | --------------------- | ------------ |
| `default` | 既定 feature | V8 |
| `boa` | `--no-default-features --features js-boa` | boa |
| `none` | `--no-default-features` | なし |

3 OS の既定ビルドは CI の `binary-size` ジョブ（`make check-binary-size` 相当）の出力
`binary-size: host=... bytes=...`（strip なし）を出典とする。

## 結果

### 既定ビルド（3 OS・CI 実測・strip なし）

出典: CI run `37133473120`（`main` の `43987c4`・2026-10-03・success）。

| OS（host） | bytes | MB | Chromium 比削減率 | 上限との差（B） | 結果 |
| ---------- | ----: | -: | ----------------: | --------------: | ---- |
| Linux（`x86_64-unknown-linux-gnu`） | 70,576,624 | 70.58 | 84.57% | 20,903,376 | pass |
| macOS（`aarch64-apple-darwin`） | 63,072,656 | 63.07 | 86.21% | 28,407,344 | pass |
| Windows（`x86_64-pc-windows-msvc`） | 47,401,472 | 47.40 | 89.64% | 44,078,528 | pass |

### 3 構成の実測（Linux・ローカル）

出典: 上記 2 コマンドの `js-binary-size:` 行。全て exit 0。同梱エンジンの陽性対照
（`default` は V8、`boa` は boa、`none` はエンジンなし）も一致した。

| config | strip なし（bytes） | strip なし（MB） | strip あり（bytes） | strip あり（MB） |
| ------ | ------------------: | ---------------: | ------------------: | ---------------: |
| `default`（V8） | 70,613,360 | 70.61 | 52,721,656 | 52.72 |
| `boa` | 19,624,088 | 19.62 | 15,645,272 | 15.65 |
| `none` | 6,148,072 | 6.15 | 4,863,104 | 4.86 |

CI の Linux 値（70,576,624 B）とローカルの `default`（70,613,360 B）は基準コミットが
異なる別の計測で、36,736 B の差がある。同一コミットの値としては扱わない。

### macOS・Windows の軽量・エンジンなし構成

未測定。実行環境がなく、推定値・外挿値は載せない。再現は各 OS で上記 2 コマンドを実行する。

## CORE-2 の達成可否

| OS | 判定 | 根拠 |
| -- | ---- | ---- |
| Linux | met | 70,576,624 B（削減率 84.57%）が上限 91,480,000 B 以下 |
| macOS | met | 63,072,656 B（削減率 86.21%）が上限以下 |
| Windows | met | 47,401,472 B（削減率 89.64%）が上限以下 |

strip なし・最適化設定未適用の値で基準内に収まっている。参考として Linux の strip あり
`default`（52,721,656 B）の削減率は 88.47%。

## 机上算定・PoC 実測との比較

比較対象（PoC-15 の机上算定・PoC-3 の増分・PoC-2 の基盤）は strip 後の値のため、
ローカルの strip あり計測と比べる。

### 構成別

| config | 実測（strip あり） | 机上算定 | 差 | 比 |
| ------ | -----------------: | -------: | -: | -: |
| `default`（V8） | 52.72MB | 約 42.92MB | +9.80MB | 1.23 倍 |
| `boa` | 15.65MB | 約 10.05MB | +5.60MB | 1.56 倍 |
| `none` | 4.86MB | 約 2.11MB（PoC-2 基盤） | +2.75MB | 2.30 倍 |

### エンジン増分

| 増分 | 実測（strip あり） | PoC-3 実測 | 差 | 比 |
| ---- | -----------------: | ---------: | -: | -: |
| `default − none`（V8） | 47,858,552 B（47.86MB） | +40.81MB | +7.05MB | 1.17 倍 |
| `boa − none`（boa） | 10,782,168 B（10.78MB） | +7.94MB | +2.84MB | 1.36 倍 |

### 差について言えること

- 実測は机上算定・PoC をいずれも上回った（V8 構成で 1.23 倍、軽量構成で 1.56 倍）
- 事実として言える条件差:
  - release プロファイルの最適化設定（`opt-level = "z"`・LTO 等）が未適用
  - PoC-2 の `core-proto` と現行 cli では含む機能が異なる
  - PoC は macOS arm64、ローカル計測は Linux x86_64
- 各要因が差にどれだけ寄与したかは未分解。プロファイル適用後の再計測（#146・#368）で
  確認する

## 限界・残課題

- release プロファイルの CORE-2 前提が未適用（#146・#368）。適用後に再計測する
- macOS・Windows の軽量・エンジンなし構成が未測定
- strip あり計測はローカル Linux のみ
- 両エンジン同梱構成は対象外
- `harness/binary-size/README.md` の「3 OS での実測は #470 で手動実行」という記述は、
  本レポートが macOS・Windows の非既定構成を測っていないため実態と合わない

## spec への示唆

JS-3・PERF-1 の「検討中」を「確定」に変えるか、目標値を見直すかは spec リポ側の
ユーザー判断事項。本レポートは実測値を提供するにとどめ、結論は出さない。

## 受け入れ条件との対応

| 条件 | 状況 | 参照 |
| ---- | ---- | ---- |
| 既定ビルドの実バイナリで CORE-2 の達成可否を記録 | 達成（3 OS とも met） | 「CORE-2 の達成可否」 |
| 軽量ビルド・エンジンなしのサイズを参考値で記録 | 部分達成（Linux のみ。macOS・Windows は未測定） | 「3 構成の実測」 |
| PoC-15 の机上算定・PoC-3 の増分との比較を記録 | 達成（strip あり値どうしで比較） | 「机上算定・PoC 実測との比較」 |

## 参照

- `docs/spec/04-behavior/js-engine.md`（JS-1・JS-3）・`perf-targets.md`（PERF-1）・`core-dom.md`（CORE-2）、`05-tasks.md`（TASK-31）
- [`harness/binary-size/README.md`](../../harness/binary-size/README.md)
- [`render-feature-gate.md`](./render-feature-gate.md)（RENDER-2 の OS 別実測）
