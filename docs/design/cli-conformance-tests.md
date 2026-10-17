# CLI 基本操作コマンドのコンフォーマンステスト仕様書

対応 `TASK-47`（47.7）/ `TASK-49` / `MS-5` / ビヘイビア `CLI-1`（関連 `CLI-3`・`MEAS-5`・`XOS-7`・`XOS-10`・`PROF-1`）（[spec-reference](../../.claude/rules/spec-reference.md)）。

- 状態: TASK-47.2〜47.6 の実装（`launch`・`navigate`・`query`・`screenshot`・`--profile-dir`）に対する仕様。実機での実施と判定は人間担当の TASK-49 が行う。本書の期待値は Linux 上のデバッグビルドで観測した値と、`crates/fandhe-browser-cli` のソースの固定文言から引き写した（macOS・Windows の実測は未実施。未検証）
- 暫定形式: 出力形式（`status=… url=…`・JSON 文字列リテラルを 1 行 1 値・`width=… height=… bytes=… url=…`）は `CLI-3` の確定時に見直す暫定仕様（REPAIR-3）。変わったら本書も更新する

## 1. 目的・範囲

`CLI-1` は、基本操作コマンドが 3 OS で同じ構文・挙動を持つことを求める。検証件数は TASK-47 の確定事項により、5 コマンド × 3 OS = 最低 15 ケース。

「同じ構文・挙動」の判定基準:

- 同じ argv（パス引数の OS 表記の違いだけ許す）で、終了コードが同じ
- stdout の形式が同じ（可変部は 2 章の規則で正規化してから比べる）
- stderr の固定文言が同じ

OS 間で違ってよいのは、spec が OS 固有と定める点だけ。具体的には Windows でプロファイルルートを `%LOCALAPPDATA%` 配下に限る制約（`XOS-7`・`XOS-10`）。

範囲外（スタブ。実装済みを装わない）:

| 項目 | 状況 |
| ---- | ---- |
| `--port`・ポート 0 | 実装済み（`CLI-3`・#823）。`launch --port <PORT>`（既定 9333・0 は OS 割り当て）。待ち受け IP は `127.0.0.1` 固定（`SEC-4`）。通知行の出力形式は暫定。期待値は末尾の補足節 |
| `<cmd> --help` | 未対応。help は `--help`・`-h`・`help` のトップレベルのみ |
| `screenshot --full-page` | v1 ホスト未対応。usage エラーで拒否（補助ケース） |
| プロファイルデータの実利用 | TASK-116・`PROF-7` |
| 名前でのプロファイル選択 | `XOS-9`・TASK-62 |
| ページ JS の実行 | 対象外 |

## 2. 共通の前提・定義

### 2.1 対象バイナリと描画ホスト

- Linux・macOS は `fandhe-browser`、Windows は `fandhe-browser.exe`。ビルドは `cargo build --release -p fandhe-browser-cli`（既定 feature）
- 結果には commit SHA・OS バージョン・アーキテクチャを記録する
- `screenshot` には描画ホスト `fandhe-browser-servo-host`（`servo-host/` の別 workspace）が要る。環境変数 `FANDHE_BROWSER_RENDER_HOST` で渡すか、本体と同じディレクトリに置く。用意できない OS では `CT-SHOT-*` を「前提未充足」と記録する（合格にしない）

### 2.2 終了コードと stderr

| 終了コード | 意味 |
| ---------- | ---- |
| 0 | 成功（help を含む） |
| 1 | 実行時の失敗（取得失敗・描画失敗・プロファイルを開けない・ロック競合・bind 失敗など） |
| 2 | usage エラー（未知のオプション・引数の不足や過剰・同時指定不可・`--profile-dir` の字句検証エラー） |

stderr は `error: <固定文言>`。usage エラーは末尾に `; run 'fandhe-browser --help' for usage` が付く。根拠: `commands/mod.rs`（`CommandError::exit_code`・`USAGE_EXIT_CODE`）・`main.rs`。

### 2.3 取得先 URL（FIXTURE_URL）

既定の SSRF 拒否（loopback・プライベートアドレスを拒否。緩める手段はない）により、ローカルの fixture サーバーは使えない。公開 URL を 1 つ、パラメータとして定義する。変更が要るときは 1 か所だけ直す。

- `FIXTURE_URL = https://example.com/`（userinfo を含めない）
- 事前確認（実施前に必ず行う）: 外部の HTTP クライアントで、status 200・`<title>` が `Example Domain`・`<html lang="en">`・本文に `<p>This domain is for use in documentation examples …` があることを確かめる。期待値と食い違ったら実施せず本書を直す
- 注意: 2026-10 の時点で `example.com` に `<h1>` と `<a>` は無い。そのため要素取得は `title`・`p`・`html` を使う（Linux で `navigate`・`query` の各ケースを実測して確認済み）

### 2.4 コマンドラインの表記と正規化

- argv は要素ごとのリストで示す。シェルのクォート差を判定から外すため、bash と PowerShell の例を併記する
- 正規化: launch のブラウザ ID は `[0-9A-F]{32}`、screenshot の `bytes` は `[0-9]+`（実ファイルのサイズと一致すること）。Windows でも stdout は `\n` 区切りで比べる（CRLF が混じったら正規化して比べ、差分として記録する）

### 2.5 環境の隔離（実ユーザーのプロファイルに書かない）

| 項目 | Linux | macOS | Windows |
| ---- | ----- | ----- | ------- |
| 隔離 | `HOME`・`XDG_DATA_HOME` を `mktemp -d` へ | `HOME` を `mktemp -d` へ | `LOCALAPPDATA` は `...\AppData\Local` で終わる必要がある（`local_root.rs` の `REASON_BAD_LOCALAPPDATA`）ため差し替えない。実際の `%LOCALAPPDATA%` 配下を使う。このため **専用の Windows ユーザー（または `%LOCALAPPDATA%\fandhe-browser` が存在しない新規環境）で実施する**ことを前提とし、実ユーザーの既存プロファイルと重ならない・ロック競合しないことを実施前に確認する。後片付けは本ケースで新規作成した領域だけに限る |
| 既定プロファイルルート | `$XDG_DATA_HOME/fandhe-browser`（未設定時 `$HOME/.local/share/fandhe-browser`） | `$HOME/Library/Application Support/fandhe-browser` | `%LOCALAPPDATA%\fandhe-browser` |
| `--profile-dir` の場所 `<P>` | `<TMP>/p` | `<TMP>/p`（`<TMP>` は下記注の実体パス） | `%LOCALAPPDATA%\fandhe-ct-<id>\p`（必ず配下） |
| 一時出力先 `<TMP>` | `mktemp -d` 配下 | `mktemp -d` 配下 | `$env:TEMP` 配下の新規ディレクトリ |
| 終了操作（launch） | SIGINT（Ctrl+C） | 同左 | Ctrl+C / `Stop-Process` |

macOS の注: `mktemp -d` は `/var/folders/...` を返すが `/var` は `/private/var` への symlink で、`ExplicitStore` は canonicalize せず `Profile::open` は祖先の symlink を拒否する。`<TMP>` は `TMP=$(cd "$(mktemp -d)" && pwd -P)` のように実体パスへ解決してから `<P>=<TMP>/p` を組み立てる。解決せずに渡すと `invalid profile directory: a path component is a symlink; specify the real path without symlinks (on macOS, /tmp and /var are symlinks: use /private/tmp or /private/var)` で終了コード 1 になる（境界は緩めず、拒否時に実体パスを案内する方針。#1089）。

根拠: `crates/fandhe-browser-profile/src/store.rs` の `APP_DIR_NAME = "fandhe-browser"`。

## 3. 必須 15 ケース（5 コマンド × 3 OS）

ID は `CT-<コマンド>-<LINUX|MACOS|WINDOWS>`。3 OS とも argv の形は同じで、パス表記だけが違う。TASK-49 のレポートはこの ID で結果を引く。

### 3.1 起動（`CT-LAUNCH-*`）

根拠: `commands/launch.rs`・`server.rs`（`DEFAULT_ADDR`・`start_server`）。

| ID | OS |
| -- | -- |
| `CT-LAUNCH-LINUX` | Linux |
| `CT-LAUNCH-MACOS` | macOS |
| `CT-LAUNCH-WINDOWS` | Windows |

- 前提: `127.0.0.1:9333` が空いている。2.5 の隔離済み
- argv: `["launch"]`（bash: `fandhe-browser launch`、PowerShell: `.\fandhe-browser.exe launch`）。続けて引数なし `[]` でも同じ出力になることを確認する（`run_default` が同じ起動処理を通る）
- 期待 stderr: 起動後に 1 行 `DevTools listening on ws://127.0.0.1:9333/devtools/browser/<[0-9A-F]{32}>`。プロセスは常駐を続ける（stdout は空）
- 補助確認: `GET http://127.0.0.1:9333/json/version` が 200 の JSON を返し、`webSocketDebuggerUrl` が stderr の URL と一致する
- 副作用: 既定プロファイルルート（2.5）に `profile.lock` と `cookies`・`storage`・`cache`・`history` ができる
- 終了操作の後: ポート 9333 が解放される（プロセスの終了コードは判定対象にしない）
- 後片付け: 隔離用ディレクトリを削除する（Windows は `%LOCALAPPDATA%\fandhe-browser` のうち本ケースで新規作成した分だけ。実施前から存在したものは削除しない）

### 3.2 ナビゲーション（`CT-NAV-*`）

根拠: `commands/navigate.rs`（`NavigateOutput::write_line`）。

| ID | OS |
| -- | -- |
| `CT-NAV-LINUX` | Linux |
| `CT-NAV-MACOS` | macOS |
| `CT-NAV-WINDOWS` | Windows |

- argv: `["navigate", "https://example.com/"]`
- 期待 stdout: `status=200 url=https://example.com/`（1 行）。stderr は空。終了コード 0
- 副作用: `--profile-dir` を付けなければ既定プロファイルに触れない（ファイルを作らない）

### 3.3 要素取得（`CT-QUERY-*`）

根拠: `commands/query.rs`（`QueryOutput::write_lines`・JSON 文字列リテラル）。

| ID | OS |
| -- | -- |
| `CT-QUERY-LINUX` | Linux |
| `CT-QUERY-MACOS` | macOS |
| `CT-QUERY-WINDOWS` | Windows |

- argv: `["query", "https://example.com/", "title"]`
- 期待 stdout: `"Example Domain"`（1 行）。stderr は空。終了コード 0
- 副作用: なし
- 同じケースの中で次も確認する（正規化は不要で完全一致）

| argv | 期待 stdout |
| ---- | ----------- |
| `["query", "--attr", "lang", "https://example.com/", "html"]` | `"en"` |
| `["query", "--attr", "src", "https://example.com/", "script"]` | `"/s.js"` |
| `["query", "--all", "https://example.com/", "p"]` | `"This domain is for use in documentation examples without needing permission. This is not a service; avoid relying on it for testing and monitoring purposes."`（1 行） |

### 3.4 スクリーンショット取得（`CT-SHOT-*`）

根拠: `commands/screenshot.rs`（`ScreenshotOutput::write_line`）・`servo-host/src/engine.rs` の `VIEWPORT = (1280, 800)`。

| ID | OS |
| -- | -- |
| `CT-SHOT-LINUX` | Linux |
| `CT-SHOT-MACOS` | macOS |
| `CT-SHOT-WINDOWS` | Windows |

- 前提: 描画ホストを解決できる（2.1）。`<TMP>` があり、`<TMP>/shot.png` はまだ無い
- argv: `["screenshot", "https://example.com/", "<TMP>/shot.png"]`（Windows は `<TMP>\shot.png`）
- 期待 stdout: `width=1280 height=800 bytes=<N> url=https://example.com/`（1 行）。stderr は空。終了コード 0
- 副作用: `shot.png` ができる。先頭 8 バイトが PNG シグネチャ `89 50 4E 47 0D 0A 1A 0A`、IHDR が 1280×800、ファイルサイズが `<N>` と一致する
- 描画ホストが無い場合: stderr `error: rendering is not available (render host not found; set FANDHE_BROWSER_RENDER_HOST)`・終了コード 1（Linux で実測）。これは前提未充足のシグネチャとして記録し、合格にはしない

### 3.5 プロファイル指定（`CT-PROFILE-*`）

根拠: `commands/profile.rs`・既存テスト `prof1_profile_dir_creates_profile_and_runs_navigate`（`tests/command_dispatch.rs`）。

| ID | OS |
| -- | -- |
| `CT-PROFILE-LINUX` | Linux |
| `CT-PROFILE-MACOS` | macOS |
| `CT-PROFILE-WINDOWS` | Windows |

- argv: `["navigate", "--profile-dir", "<P>", "about:blank"]`（`<P>` は 2.5 の表）
- 期待 stdout: `url=about:blank`（1 行）。stderr は空。終了コード 0。ネットワークには出ない
- 副作用: `<P>/profile.lock` と `cookies`・`storage`・`cache`・`history` ができる。Linux・macOS ではルート `<P>` のモードが `0700`（Linux で実測）
- OS 差異: Windows は `<P>` を `%LOCALAPPDATA%` 配下に置く（`XOS-7`・`XOS-10`）。これは spec が定める OS 固有の条件で、差分とは数えない

## 4. 補助ケース（異常系。15 件の数には含めず、3 OS とも実施を推奨）

ID は `CT-<コマンド>-E<n>`（3 OS 共通。結果は OS ごとに記録する）。終了コードと stderr は Linux で実測した値。

| ID | argv | 終了コード | 期待 stderr（先頭に「error:」と空白 1 つ） |
| -- | ---- | ---------- | ------------------------------- |
| `CT-CMD-E1` | `["nope"]` | 2 | `unknown command 'nope'; run 'fandhe-browser --help' for usage` |
| `CT-CMD-E2` | `["--help"]` | 0 | （stdout が `Usage: fandhe-browser [COMMAND] [ARGS]...` で始まり、`launch`・`navigate`・`query`・`screenshot`・`help` と `--profile-dir PATH` を含む） |
| `CT-LAUNCH-E1` | `["launch", "--bogus"]` | 2 | `unknown option '--bogus'; run 'fandhe-browser --help' for usage` |
| `CT-LAUNCH-E2` | 9333 を別プロセスが使用中に `["launch"]` | 1 | `failed to bind listener` |
| `CT-NAV-E1` | `["navigate"]` | 2 | `missing required argument 'URL'; run 'fandhe-browser --help' for usage` |
| `CT-NAV-E2` | `["navigate", "http://127.0.0.1/"]` | 1 | `navigation to private or loopback address is not allowed` |
| `CT-NAV-E3` | `["navigate", "file:///etc/passwd"]`（Windows は `file:///C:/Windows/win.ini`） | 1 | `unsupported URL scheme (only http, https and about:blank are allowed)` |
| `CT-QUERY-E1` | `["query", "--all", "--attr", "href", "https://example.com/", "a"]` | 2 | `options '--all' and '--attr' cannot be used together; run 'fandhe-browser --help' for usage` |
| `CT-QUERY-E2` | `["query", "about:blank", "h1"]` | 1 | `query requires a fetched document (about:blank has no content)` |
| `CT-QUERY-E3` | `["query", "https://example.com/", "h1"]` | 1 | `no element matches the selector` |
| `CT-SHOT-E1` | 出力先が既にあるファイルで `["screenshot", "https://example.com/", "<既存>"]` | 1 | `output file already exists`（描画ホストの解決より前に失敗し、既存ファイルを変更しない） |
| `CT-SHOT-E2` | `["screenshot", "--full-page", "https://example.com/", "<TMP>/n.png"]` | 2 | `option '--full-page' is not supported yet; run 'fandhe-browser --help' for usage` |
| `CT-SHOT-E3` | `["screenshot", "about:blank", "<TMP>/n.png"]` | 1 | `screenshot requires a fetched document (about:blank has no content)` |
| `CT-PROFILE-E1` | `["navigate", "--profile-dir"]` | 2 | `missing required argument 'PATH'; run 'fandhe-browser --help' for usage` |
| `CT-PROFILE-E2` | `["navigate", "--profile", "p", "about:blank"]` | 2 | `unknown option '--profile'; run 'fandhe-browser --help' for usage`（`--profile` は予約済みで、`--profile-dir` だけを受け付ける） |
| `CT-PROFILE-E3` | `launch --profile-dir <P>` を常駐させたまま `["navigate", "--profile-dir", "<P>", "about:blank"]` | 1 | `profile is already in use` |
| `CT-PROFILE-E4`（Windows のみ） | `%LOCALAPPDATA%` の外を `<P>` にして `["navigate", "--profile-dir", "<外>", "about:blank"]` | 1 | `on Windows, the profile root must be located under %LOCALAPPDATA% (XOS-7, XOS-10)`（`server.rs` の `UNSUPPORTED_OS_MESSAGE`。Windows で未実測）。Linux・macOS では同じ argv が成功するのが期待値 |
| `CT-PROFILE-E5`（unix） | symlink を辿った後に `..` を含む `<P>`（既存テストと同等） | 2 | `invalid profile directory: …`。ディレクトリを作らない |
| `CT-PROFILE-E6`（unix。任意） | symlink の祖先を含む `<P>`（macOS は `/tmp/x` で再現できる）で `["navigate", "--profile-dir", "<P>", "about:blank"]` | 1 | `invalid profile directory: a path component is a symlink; …`（文言は `profile.rs` の `SYMLINK_COMPONENT_REASON` から引き写した。リンク先にディレクトリを作らない。#1089） |

補足:

- `CT-QUERY-E3` は `example.com` に `<h1>` が無いことへの依存を使う。事前確認（2.3）で `<h1>` が現れていたら別のセレクタ（存在しないもの、例 `table`）に差し替える
- `CT-LAUNCH-E2` と `CT-PROFILE-E3` の文言は、ソースの `Display` 実装（`server.rs`・`profile.rs`）から引き写した。Linux で未実測の場合は TASK-49 で観測値を記録する

## 5. 判定と結果記録（TASK-49 向け）

合格条件: 必須 15 ケースがすべて期待どおりで、同じケースの 3 OS の結果（正規化後）が一致する。前提未充足は合格に数えない。

記録表の列:

| 列 | 内容 |
| -- | ---- |
| ケース ID | `CT-NAV-LINUX` など |
| OS・版・アーキテクチャ | |
| commit SHA | |
| 実行日 | |
| 終了コード | |
| stdout（正規化後）・stderr | |
| 副作用の確認 | ファイル・モード・ポート解放 |
| 判定 | 合格 / 不合格 / 前提未充足 |
| 差分メモ | |

不合格・差分の扱い: 原因を特定して Issue に起票する（関連する TASK-n・ビヘイビア ID を併記）。spec の変更が要る場合は spec リポ側の課題として報告する。

## 6. 安全上の注意

- 実施者は `--profile-dir` に一時ディレクトリか `%LOCALAPPDATA%` 配下だけを使う。実ユーザーのプロファイルに書かないよう、2.5 の隔離と後片付けを守る
- SSRF の既定拒否を緩める手順は存在せず、本書でも追加しない。拒否は `CT-NAV-E2` で固定する
- `launch` は loopback（`127.0.0.1:9333`）だけで待ち受ける。非 loopback での公開を試みない
- Cookie・トークン・実在のプロファイルデータを検証に使わない。UA など識別面を変える手順は含めない（`SEC-2`）

## 補足: `launch --port`（`CLI-3`・#823）

| 引数 | 期待 |
| ---- | ---- |
| `["launch", "--port", "0"]` | stderr の 1 行が `DevTools listening on ws://127.0.0.1:<1-65535>/devtools/browser/<[0-9A-F]{32}>`。`/json/version` の `webSocketDebuggerUrl` と一致する |
| `["launch", "--port", "65536"]` | 終了コード 2。`invalid value for option '--port'; run 'fandhe-browser --help' for usage` |
| `["launch", "--port"]` | 終了コード 2。`missing required argument 'PORT'; run 'fandhe-browser --help' for usage` |
