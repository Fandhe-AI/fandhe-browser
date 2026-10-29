# cli-conformance

CLI 基本操作コマンド（`launch`・`navigate`・`query`・`screenshot`・`--profile-dir`）のコンフォーマンステストを実行し、結果を仕様書 5 章の記録表の列で出力するハーネス（TASK-49・`CLI-1`。関連 `CLI-3`・`MEAS-5`・`XOS-7`・`XOS-10`・`PROF-1`）。
仕様は [`docs/design/cli-conformance-tests.md`](../../docs/design/cli-conformance-tests.md)（以下「仕様書」）。期待値は仕様書 3・4 章から引き写してあり、仕様書を直したら `run.sh` の固定値も同時に直す。

TASK-49 は「3 OS で実施して判定する」人間担当のタスク。本ハーネスは計測の仕組みで、**合否・OS 間の一致の判定は人間が行う**（出力は判定の材料）。3 OS の実測は手動 workflow `.github/workflows/xos-measure.yml`（`suite=cli-conformance`）が GitHub ホステッドランナーで行う（オーナー承認 2026-10-29）。

## ファイル

| ファイル | 役割 |
| -------- | ---- |
| `run.sh` | 実行スクリプト。必須 5 ケース（`CT-<コマンド>-<OS>`）と補助ケース（`CT-<コマンド>-E<n>`）を実行して JSONL と Markdown 表を出す |
| `self-test.sh` | `run.sh` のオフライン自己テスト（偽バイナリ・偽 curl。pass / fail / 前提未充足の 3 分岐、CRLF 正規化、引数エラー） |
| `fake_cli.mjs` | 自己テスト用の偽 `fandhe-browser`（node 製。実バイナリの固定文言を模倣。`FAKE_MODE` で挙動を崩す） |

## 使い方

```bash
cargo build --release -p fandhe-browser-cli
bash harness/cli-conformance/self-test.sh     # 判定ロジックの自己テスト（make check-cli-conformance）
bash harness/cli-conformance/run.sh \
  [--bin P] [--render-host P] [--out-jsonl P] [--out-md P] [--commit SHA] \
  [--outside-dir DIR] [--timeout SEC] [--only ID[,ID...]] [--strict]
CLI_CONFORMANCE_ARGS="--render-host <path>" make measure-cli-conformance   # 自己テスト → 実行
```

| オプション | 既定 | 意味 |
| ---------- | ---- | ---- |
| `--bin` | `<repo>/target/release/fandhe-browser`（Windows は `.exe`） | 対象バイナリ |
| `--render-host` | なし（環境変数 `FANDHE_BROWSER_RENDER_HOST` も外す） | 描画ホスト `fandhe-browser-servo-host`。`CT-SHOT-*` に要る |
| `--out-jsonl` / `--out-md` | `<repo>/target/cli-conformance/results.{jsonl,md}` | 結果の出力先 |
| `--commit` | `GITHUB_SHA`、無ければ `git rev-parse HEAD` | 記録する commit SHA |
| `--outside-dir` | `RUNNER_TEMP` | Windows の `CT-PROFILE-E4` 用。`%LOCALAPPDATA%` の外にあるディレクトリ（`%TEMP%` は配下なので使えない） |
| `--timeout` | 60 | コマンド 1 回の上限秒（screenshot は 2 倍） |
| `--only` | なし | 指定したケース ID だけ実行（再実行・デバッグ用） |
| `--strict` | 無効 | 必須ケースに不合格または前提未充足があれば終了コード 1 |

終了コード: `0` 記録完了（不合格があっても 0。成否は計測結果）、`1` `--strict` で必須ケースが非合格、`2` 入力・環境エラー（バイナリ不在・Windows で既定プロファイルルートが既にある等）。

## 判定の 3 値

| 値 | 意味 |
| -- | ---- |
| `pass`（合格） | 終了コード・正規化後の stdout・stderr・副作用がすべて期待どおり |
| `fail`（不合格） | いずれかが期待と異なる。理由は差分メモ（`note`）に `FAIL: ...` で残る |
| `unmet`（前提未充足） | **合格に数えない**。理由は `UNMET: ...` |

前提未充足になる条件:

- `CT-SHOT-*`: 描画ホストを解決できない固定シグネチャ（`error: rendering is not available (render host not found; set FANDHE_BROWSER_RENDER_HOST)`・終了コード 1）。それ以外の失敗は `fail`
- `CT-LAUNCH-*`・`CT-LAUNCH-E2`・`CT-PROFILE-E3`: `127.0.0.1:9333` が使用中（`launch` は 9333 固定のため）。`CT-LAUNCH-E2` は加えて node が無いとき
- `CT-NAV-*`・`CT-QUERY-*`・`CT-SHOT-*`・`CT-QUERY-E3`: `FIXTURE_URL`（`https://example.com/`）の事前確認（仕様書 2.3）が期待と食い違うとき。`--attr src` の確認だけは `<script src=/s.js>` が無いときに前提未充足
- `CT-PROFILE-E5`・`CT-PROFILE-E6`: Windows（symlink を作る unix 専用ケース）

## 出力

- JSONL: 1 行目がメタ（`commit`・`os`・`os_version`・`arch`・`bin`・`render_host`・`fixture_ok` 等）、以降は 1 ケース 1 行（`id`・`required`・`exit_code`・`argv`・`stdout`・`stderr`・`side_effects`・`verdict`・`note`）。`CT-LAUNCH-*` は常駐するため `exit_code` が `null`
- Markdown 表: 仕様書 5 章の列（ケース ID・OS・版・アーキテクチャ・commit SHA・実行日・終了コード・stdout（正規化後）・stderr・副作用の確認・判定・差分メモ）
- バイナリのパスはリポジトリ配下ならリポ相対で記録する

正規化（仕様書 2.4）: `\r` は落として LF で比べ、混じっていたら差分メモに `CRLF found in output` を残す。ブラウザ ID は `[0-9A-F]{32}`、`bytes` は実ファイルのサイズと一致を確認する。

## 隔離（仕様書 2.5・6 章）

- Linux・macOS: ケースごとの一時ディレクトリに `HOME`・`XDG_DATA_HOME` を向ける。一時ディレクトリは `pwd -P` で実体パスへ解決する（macOS の `/var` は symlink で、`--profile-dir` は祖先の symlink を拒否するため）。`FANDHE_BROWSER_CONFIG` は外す
- Windows: `LOCALAPPDATA` は `...\AppData\Local` で終わる必要があり差し替えられないため、実際の `%LOCALAPPDATA%` を使う。**実行前に `%LOCALAPPDATA%\fandhe-browser` が無いことを確認し、あれば終了コード 2**（専用の Windows ユーザーか新規環境で実施する。使い捨ての GitHub ホステッドランナーは該当する）。`--profile-dir` の `<P>` は `%LOCALAPPDATA%\fandhe-ct-<pid>-<乱数>\p` で、後片付けは本スクリプトが作ったものだけに限る
- 外部アクセスは `example.com` と `127.0.0.1` のみ。SSRF の既定拒否を緩める手順・UA 等の識別面を変える手順は含まない
- `launch` の停止は SIGINT → SIGTERM → SIGKILL の順に試す（使えた手段は副作用欄に `stopped with ...` で残る）。非対話 bash のバックグラウンドプロセスは SIGINT が無視されうるため、SIGTERM になることがある。Windows は最後に `taskkill` へフォールバックする（終了コードは判定対象外。ポート解放を確認する）

## 既知の限界

- 3 OS の結果の突き合わせ（同じケースの正規化後の一致）は本ハーネスの外。OS ごとの JSONL を比べる
- Windows の `launch` 停止・`%LOCALAPPDATA%` 配下の挙動は、ハーネス側が Windows で未実測（初回の `xos-measure.yml` 実行で確かめる）
- `CT-LAUNCH-*` の ID は実行した OS の分だけ出る（`CT-LAUNCH-MACOS` を Linux で実行することはない）
