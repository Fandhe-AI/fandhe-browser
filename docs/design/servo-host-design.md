# Servo ホスト（別 workspace・別プロセス）の構成とプロセス間通信の設計

対応 `TASK-33` / `TASK-34` / `TASK-38` / `MS-4` / ビヘイビア `RENDER-1`〜`RENDER-4` / `OSS-2` / `OSS-3` / `PLUG-1` / `SEC-2`（[spec-reference](../../.claude/rules/spec-reference.md)）。

- 関連 Issue: #935（本設計）・#915（構成・採用版・ライセンス判断。本書は `Refs` で参照し、閉じない）・#914（試作）・#230〜#237（TASK-38 の実装 Issue）
- 状態: **確定**（オーナー判断 2026-10-15・#228）。実装は TASK-38 の issue（#228 配下）で進行中
- 範囲: 本書の作成時点では設計文書のみ。実装・依存追加・既存 Issue の本文書き換え・追加起票・ruleset 変更は行わない（設計レビュー後に別途行う）
- 数値の出典: #914 の試作（[servo-out-of-process-prototype.md](./servo-out-of-process-prototype.md)）と #915 の記録。計測は Linux x86_64 のみで、macOS・Windows・実ページ・IPC 実装は**未検証**（musl・glibc の配布形態は #1001 で実測済み。§7.1）
- spec の確認元: `docs/spec` の記録コミット f7cb79e（`RENDER-1`〜`RENDER-3`・`OSS-2`・`OSS-3` 注記、`TASK-33`・`TASK-34`・`TASK-38`）

## 0. 決定済み事項と本書の推奨

| 区分 | 内容 | 出典 |
| ---- | ---- | ---- |
| 決定済み | Servo は本体 workspace に入れず、別 workspace・別 `Cargo.lock` の子プロセス（Servo ホスト）とする | #915（2026-10-12） |
| 決定済み | 採用版は `servo =0.6.0`（0.6 LTS 系列） | #915 |
| 一時緩和（オーナー承認 2026-10-28） | 現行版は `servo =0.7.0`（LTS ではない月次版）。上流 MDN のハングの修正（servo/servo#45915・修正 #47693 は 0.7.0 に含まれ 0.6 LTS には戻らない。#1291）と IntersectionObserver の pref 有効化のため。緩和するのは LTS の条件だけで、隔離（`RENDER-1`）と MPL の扱い（`OSS-2`・`OSS-3`）は変えない。月次版にはセキュリティ修正が戻らないため、修正を含む次の LTS が出たら LTS 系列へ戻す | #1297・#1291・TASK-38 |
| 決定済み | Servo ホスト側 `deny.toml` だけで `MPL-2.0`・`MPL-2.0 AND BSD-3-Clause` を許可し、`CDLA-Permissive-2.0` は `webpki-roots` 1 件に限る。本体の `deny.toml` は変えない | #915・[licensing](../../.claude/rules/licensing.md) |
| 決定済み | FreeType は FTL を選び、`NOTICE` に帰属表示を載せる | #915 |
| 決定済み | 配置は `servo-host/`、通信は stdio + 長さ接頭辞 JSON | 本書 §1・§2（実装済み: #1005・#1021） |
| 決定済み | feature `rendering` を廃止し、設定でホストパスを与えたときだけ起動する | オーナー判断 2026-10-15（#228。本書 §3） |
| 決定済み | v1 は URL のみをホストへ送り、ホストが自分で取得する | オーナー判断 2026-10-15（#228。本書 §3） |
| 決定済み | core の `Renderer` にページ指定メソッドを追加する | オーナー判断 2026-10-15（#228。本書 §3） |
| 決定済み | CI は明示ジョブ 3 本を追加する（ジョブ 16 → 19、required 合計 20 → 23）。ruleset はジョブ追加 PR のマージ時に更新する | オーナー判断 2026-10-15（#228。本書 §6） |
| 決定済み | ホストのビルド cache は ubuntu のみ | オーナー判断 2026-10-15（#228。本書 §6.3） |
| 決定済み | ページの読み込みは許可リストで検査する（主フレーム・iframe・サブリソース・Worker）。WebSocket は隔離の有無によらず拒否する | オーナー判断 2026-11-17（本書 §2.5・§13 項目 11） |
| 決定済み | 隔離できない unix（`best-effort` の降格）でも本体のプロキシを経由させる。プロキシの起動に失敗したら起動しない。Windows は隔離なしの起動を拒否する | オーナー判断 2026-11-17（本書 §2.5・§2.6・§13 項目 12・13） |
| 保留 | egress 方針（Servo 内部通信の制限）は GA 前に判断する | #1003（本書 §2.5。比較設計と実測計画は [servo-host-egress-isolation.md](./servo-host-egress-isolation.md)） |
| 決定済み | crates.io のソース配布を先行する。9 個目の crate、対象は macOS・Linux（Windows は対象外）、crate は `MIT OR Apache-2.0`、`NOTICE` の MPL 適用範囲は不変、Servo `=0.7.0` のまま、版は他 crate と揃える | オーナー判断 2026-11-07（#1538。本書 §13 項目 10） |
| 保留 | コンテナの配布は spec の整理（`TASK-64`・`CTR-4`）後に起票する。バイナリ配布は未決 | 本書 §8.2・§10 |
| 確定（TASK-121） | 操作モードのプロトコル（`dom_snapshot`・`act`・`wait`・ページ識別子・上限・エラー kind・契約 fixture）。実装は TASK-122〜TASK-126 | 本書 §14・#1582 |

オーナー判断の結果は §13 に記録する。

## 1. Servo ホスト workspace の配置

### 1.1 構成

```text
servo-host/
├── Cargo.toml     # 自前の [workspace] を持つ単一 package（fandhe-browser-servo-host）
├── Cargo.lock     # 本体のものとは独立
├── deny.toml      # ホスト専用のライセンス・advisories 検査
├── src/           # main.rs・args.rs（起動引数の検査）・relay.rs（netns 内の中継。unix）・server.rs（stdio ループ）・engine.rs・page.rs・screenshot.rs・layout.rs
└── tests/         # 契約テスト・自己テスト
```

- パスはリポジトリ直下 `servo-host/`。`crates/*` のグロブに入らず、試作の提案（`crates/` 外）とも一致し、本体 `members` と物理的に分かれる。
- バイナリ名は `fandhe-browser-servo-host`。

### 1.2 本体 workspace に入れない方法

1. `servo-host/Cargo.toml` に `[workspace]` を置き、独立した workspace root にする。
2. ルート `Cargo.toml` に `exclude = ["servo-host"]` を足す（`crates/*` や `harness/*` のグロブ拡張から守る）。
3. 機械検証を足す。`cargo metadata --no-deps` の workspace members に `servo-host` が現れないこと、ルート `Cargo.lock` に `servo` / `mozjs` 系が無いことを `make check-workspace-manifest` 系に追加する（`RENDER-1`）。

### 1.3 toolchain・プロファイル

- `rust-toolchain.toml` はルートを単一真実源とし、`servo-host/` にコピーしない（rustup は親ディレクトリを遡って探す）。`servo-host/` 内で `rustup show active-toolchain` がルートの toolchain になることを検証手順に入れる。
- Servo の要求 MSRV が stable で満たされなくなった場合の扱いは**未検証**（要確認）。
- `[profile.release]` は本体と同じ（`panic = "abort"`・`opt-level = "z"`・`lto = true`・`strip = true`）。PR の CI 検証は dev プロファイル、release ビルドは main / release 前に限る（§6）。

### 1.4 依存

- `servo =0.7.0`、`default-features = false`、`features = ["baked-in-resources"]`（試作で起動に必須と確認済み）。版は #1297 で 0.6.0 から上げた一時的な緩和（§0 の一時緩和の行を参照）。
- 試作で直接必要になった `dpi` / `url` / PNG エンコード用の画像 crate 等は、Servo の `Cargo.lock` で解決される版に合わせて `=x.y.z` 固定する。
- これらの追加は本書の範囲では行わず、実装 Issue で承認事項として提示する（[dependency-policy](../../.claude/rules/dependency-policy.md)）。承認提示項目の雛形は付録 A。

### 1.5 ライセンス

- ホスト独自コードは `MIT OR Apache-2.0`。Servo のソースは無改変とし、改変が必要になったらユーザーに判断を仰ぐ（[licensing](../../.claude/rules/licensing.md)）。

## 2. プロセス間のやりとり

### 2.1 トランスポートとフレーミング

- トランスポートは stdio。ローカル TCP は外部公開リスクがあるため不採用（`PLUG-1`）。UDS / 名前付きパイプは大容量転送が必要になった場合の拡張候補とする。
- フレームは「`u32` リトルエンディアンの長さ接頭辞 + UTF-8 JSON 1 メッセージ」。js ワーカー（`crates/fandhe-browser-js/src/worker_protocol.rs`）と同流儀で、長さは確保前に上限検証する。JSON Lines は行長上限の検証がフレーム長より難しいため採らない。
- stdout 汚染対策: ホストは自コードから stdout に書かず、ログは stderr に出す。想定外のバイト列は不正な長さとして検出し、プロトコル違反として扱う。
- fd の付け替え（2026-11-19 のオーナー判断で変更。#1597）: 当初は「fd の付け替えは `unsafe` が要るため採らない」としていた。Servo 0.7.0 はページの `alert()` の文字列を stdout へ無条件に書き（`servo-script` の `Window::Alert`。止める設定は無い）、ホストが stdout と stderr のロックを持ち続けるとページのスクリプトが止まって `load` が時間切れになる。そこで Unix では、Servo を作る前に fd 1 を複製してフレーム専用にし、fd 1 自体は `/dev/null` へ付け替える（`rustix::stdio::dup2_stdout`。safe な関数で、自前の `unsafe` は足さない。servo 経由で既に入っている `rustix` を直接依存にした。版は RUSTSEC-2026-0486 を避けるため `=1.1.6`）。stderr はロックを持ち続けない。捨てる先にするのは、ページ由来の文字列をログにも残さないため。Windows は付け替えず stdout と stderr のロックも従来どおり持ち、`alert()` を呼ぶページの読み込みが時間切れになりうることを既知の制約にする（ホストの配布対象は macOS・Linux）。

### 2.2 メッセージ

- 要求: `{"v":1,"id":<u64>,"method":"...","params":{...}}`
- 応答: `{"v":1,"id":...,"result":{...}}` または `{"v":1,"id":...,"error":{"kind":"...","message":"..."}}`
- 要求の `id` が有効な `RequestId`（0 以上 2^53-1 以下の整数）として取り出せない場合（欠落・負数・文字列・範囲外）と、JSON として解釈できない場合、ホストは応答を返さずプロトコル違反として接続を閉じる（対応付けできない無関係な `id` の応答は返さない。fixture `request-id-*`・`request-truncated-json`）。`id` が有効なら、`v` 不一致・未知フィールド等も `invalid_request` を同じ `id` で返す
- `params` を省略した要求は空オブジェクトとして各 `Params` に照合する（必須項目を持つ `load`・`visibility`・`bounding_box` は `invalid_params`、`screenshot`・`shutdown` は許可）
- 応答は `result` か `error` のどちらか一方だけで、反対側のキーは `null` でも持たない
- 起動直後にホストが `hello` イベントを最初に送る。内容は `host_version`・`servo_version`・`protocol{min,max}`・`capabilities[]`・`limits{...}`。クライアントは `protocol` の互換性を検証し、不一致は接続を閉じて `ProtocolMismatch` 相当のエラーにする。

| メソッド | params | result |
| -------- | ------ | ------ |
| `load` | `url`, `timeout_ms` | `final_url`, `status?`（Servo 0.6.0・0.7.0 では省略） |
| `screenshot` | `full_page` | `format:"png"`, `width`, `height`, `data_base64` |
| `visibility` | `selector` | `visible`, `reason?` |
| `bounding_box` | `selector` | `x`, `y`, `width`, `height` |
| `shutdown` | なし | なし |
| `dom_snapshot`・`act`・`wait` | §14 | §14（操作モード。`--interactive` のホストだけ。TASK-121 で設計を確定し、実装は TASK-122・TASK-126） |

- 戻り値は将来拡張できる構造型（`#[non_exhaustive]`。`REPAIR-4`）。未知メソッドは `unsupported` エラー、応答側の未知フィールドはクライアントが無視する。
- 契約は `docs/design/render-host-protocol.schema.json`（`host-api.schema.json` と同流儀。作成は §10 の P1）で定義する。

### 2.3 識別面を変える API は載せない（`SEC-2`）

- `user_agent`・ヘッダ・Cookie・locale・タイムゾーン・TLS・proxy・画面サイズのパラメータを持たない。
- ビューポートは v1 では固定値（例 1280×800）とし API 化しない。画面サイズは識別面に当たりうるため、必要になったら [anti-bot-review](./anti-bot-review.md) の判断を経る。
- 起動引数は固定の許可リストのみで、クライアントからの任意引数は渡さない。未実装メソッドは成功を装わず `unsupported` を返す。
- 上記の「proxy のパラメータを持たない」はプロトコルの `*Params` の話である。起動引数の loopback ポート（§2.6）は OS 隔離の部品として別扱いで、クライアントからは指定できない（#1435）。

### 2.4 上限とタイムアウト（暫定値。実測後に確定）

| 項目 | 上限 / 値 |
| ---- | --------- |
| 要求フレーム | 64 KiB |
| 通常応答フレーム | 1 MiB |
| `hello` | 16 KiB |
| スクリーンショット応答フレーム | 24 MiB（PNG 16 MiB の base64 約 21.4 MiB + 余裕） |
| `url` | 2048 バイト |
| `selector` | 1024 バイト |
| エラーメッセージ | 512 バイト |
| 同時 in-flight | 1 |
| ハンドシェイク | 10 秒 |
| `load` | 既定 30 秒・最大 60 秒（post-load フレーム待機にも上限を付ける。`RENDER-4`） |
| `screenshot` | 20 秒 |
| 要素クエリ | 5 秒 |
| `shutdown` | 3 秒の猶予後に kill |

- 操作モードのメソッドの上限・時間切れは §14.6。
- バイナリは v1 では base64 inline で返す。一時ファイルパス返却はパス検証・TOCTOU・シンボリックリンクのリスクがあるため v1 では不採用で、サイズ実測後に再検討する。

### 2.5 URL・セレクタの検証

- URL はクライアントで送信前に検証し、ホストでも再検証する（二重化）。scheme は `http` / `https` のみ。`file:`・`data:`・`javascript:`・`about:` と userinfo は拒否。ループバック・プライベート IP・`localhost` は core の fetch と同じポリシー（`allow_private_network_access`、既定 false）に従う。
- 読み込み先の許可リスト（`RENDER-3`・`SEC-2`。オーナー判断 2026-11-17・§13 項目 11）: ページの全ての読み込み（主フレーム・iframe・サブリソース・Worker）を、Servo の `load_web_resource` で検査する。WebView 経由（`WebViewDelegate`）と、WebView に紐づかない要求（`ServoDelegate`。`engine.rs` が設定する）の両方を対象にする。許可するのは scheme が `http`・`https`・`data`・`blob` の読み込みと、`about:blank`・`about:srcdoc` の完全一致だけ。それ以外（`file:`・他の `about:`・`javascript:` 等。scheme なしを含む）は空の応答で終える（`cancel()` は使わない。撮影が止まらないため）。上の URL 検証（主フレームは `http` / `https` のみ）と違い、サブリソース・iframe は `data`・`blob`・`about:blank`・`about:srcdoc` を許可する。`blob:` は同じページのスクリプトが作る URL のため許可する。検査点に届く URL は最初の URL で、リダイレクト先は見えない。ディレクトリ一覧の生成（`network_local_directory_listing_enabled`）は無効にする。
- WebSocket は scheme が http(s) に書き換わって届くため URL では判別できない。ハンドシェイクの `Upgrade`・`Sec-WebSocket-Key` ヘッダを持つ要求を、隔離の有無によらず拒否する（Servo の WebSocket はプロキシを通らないため。`RENDER-3`）。
- **既知の制約（残存リスク）**: Linux（bwrap）と macOS（sandbox-exec）では、`[rendering] egress_isolation` で Servo ホストを OS 隔離し、通信を本体の許可リスト付きプロキシに限る（#1436。`best-effort` が既定、`required` は隔離できなければ起動しない）。Windows は隔離できず既知の制約（D）とする（オーナー判断 2026-11-02・#1003・TASK-38・`RENDER-3`）。2026-11-17 から、Windows は隔離もプロキシも無いため `best-effort` でも隔離なしの起動を拒否する（撮影できない。§13 項目 13）。隔離なし（`best-effort` で隔離できなかった unix）では、本体のプロキシ（loopback TCP）を経由させ、送信先を許可リストに限る。ただし遮断はサンドボックスではなく、ホストがプロキシ設定に従うことに依存する（§13 項目 12）。プロキシ経由の名前解決後の再検査（DNS リバインディング）は未測定（[servo-host-egress-isolation.md](./servo-host-egress-isolation.md) §6.5）。v1 ではファイルシステムを隔離しない。macOS では loopback のプロキシに同じユーザーの他プロセスも届き、DNS の経路（mDNSResponder）は観測・遮断していない。実ホストでの検証は `real_host` の `render3_egress_*` と xos-measure の suite `egress-host`（#1437）。方式の比較・実測・判断は [servo-host-egress-isolation.md](./servo-host-egress-isolation.md)（§8.1）を参照。
- 可視性・境界ボックスの取得に JS 評価を使う場合、セレクタは JSON エスケープした文字列リテラルとしてのみ埋め込み、連結しない。長さと文字種も検証する。取得手段（埋込 API のレイアウト取得か JS 評価か）は**未検証**で、実装 Issue の先頭ステップで確認する。

### 2.6 起動

- バイナリ解決の優先順位: 1. 設定（`render.host_path`）、2. 環境変数 `FANDHE_BROWSER_RENDER_HOST`、3. 本体実行ファイルと同じディレクトリの `fandhe-browser-servo-host[.exe]`。PATH 探索はしない。相対パスは拒否し、絶対パスのみ許可する。`canonicalize` 後に通常ファイルであることを確認し、Unix では他者書き込み可を拒否する。Windows の ACL 検査と署名・ハッシュ固定は v1 の範囲外。
- 未設定なら起動せず、`DisabledRenderer` 相当の「レンダリング層が無効」を返す。
- シェルを介さず引数配列で起動し、`env_clear` のうえ許可リストで環境変数を渡す（許可リストは Linux の `HOME`・fontconfig 関連、Windows の `SystemRoot` 等を実機で確認してから確定する。**未検証**）。
- 作業ディレクトリと Servo のユーザーデータは専用の一時ディレクトリ（0700、終了時に削除）で、永続しない。プロファイルごとに別のホストプロセスを使い、共有しない（PROF 境界）。
- stderr は末尾 8 KiB を上限付きでバッファし、失敗時のログに使う。URL のクエリや機密をログに出さない。
- **実装（TASK-38・#994・`fandhe_browser_render::host_process`）で決めた値**:
  - 本体 `HostProcess` が渡す起動引数は現在 `--stdio` のみ（`HOST_ARGS`）。ホスト側のサーバーモード（#993）はこの引数で stdio モードに入る。クライアントから任意引数は渡さない。
  - ホストは加えて固定形式の `--egress-proxy-port <1-65535>`（macOS の隔離中と、隔離なしの unix のプロキシ用。2026-11-17・§13 項目 12）・`--egress-relay-uds <絶対パス>`（Linux の隔離中の中継用）を `--stdio` の後ろに 1 組だけ受け付ける（`RENDER-3`・`SEC-2`・TASK-38・#1435。本体からの付与は #1436 で実装済み: 設定 `[rendering] egress_isolation`（`best-effort` 既定・`required`）で Linux は bwrap・macOS は sandbox-exec で包み、隔離できない unix は loopback TCP のプロキシを起動して `--egress-proxy-port` を渡し、隔離なしで起動する（2026-11-17・§13 項目 12）。Windows は隔離もプロキシも無いため起動を拒否する（§13 項目 13）。bwrap は `--unshare-net --unshare-pid --proc /proc` で包み、pid 名前空間によりホストから本体の `/proc/<ppid>` が見えない。ラッパーを kill すれば ns 内のホストも落ちる）。両方同時・重複・未知・値欠落・形式不正（範囲外ポート、相対パス、`.`・`..`・空の要素、100 バイト超、NUL）は失敗終了（fail-closed）で、URL や任意文字列は受け取らない。Servo の http・https proxy pref は `http://127.0.0.1:<port>` 固定（`no_proxy` は空）。`--egress-relay-uds` 指定時は netns 内で `127.0.0.1:0` を待ち受ける中継（`relay.rs`）を hello の前に起動し、各接続を UDS 1 本へ双方向転送する（同時 128・無通信 300 秒は暫定値で本体プロキシ #1440 と揃える）。`*_PROXY` を環境変数で渡さない方針は変えない。
  - egress 隔離の残余リスク（`RENDER-3`・`SEC-2`・TASK-38・#1451）: (1) 外部コマンド（bwrap・sandbox-exec）は固定の絶対パス候補から選び、root 所有・group / other 書き込み不可の通常ファイルだけを信頼する。(2) `best-effort` で隔離なしに落ちたときは、警告コールバックが無ければ stderr に 1 行出す。(3) macOS の loopback プロキシは同一マシンの全ユーザー・全プロセスから届く（認証なし）。宛先は許可リスト内に限られるので SSRF にはならないが、本体の送信元 IP を踏み台にされうる（UDS 化・接続トークンは別途検討）。(4) FS 全体は隔離しない（`--dev-bind / /`）。`/run`・`/tmp`・`/var/tmp`・`/dev/shm`・`$HOME`・WorkDir の親は tmpfs で隠し（#1485・#1492）、WorkDir と隠れる場所にあるホストの実行ファイルだけ戻す。それ以外の場所（`/dev/log`・`/var/lib/*`・`/var/snap/*` 等の既知のソケット）の UDS は迂回路として残る。(5) macOS の Seatbelt は `(allow default)` で、mach/XPC 経由の通信と DNS 経路は止めない。(6) ns 内の全滅は bwrap の `--die-with-parent` に依存し、確認したのは bwrap 0.11.1 だけ。(7) RSS 監視のホスト pid 解決は、ホストが子プロセスを作らない（Servo `multiprocess: false`）前提で各段 1 件のときだけ辿る。 補足: Linux のプローブは終了コードに加えて netns の分離（inode の比較）まで確かめる（#1459）。
  - 環境変数は `env_clear` 後に、workdir 配下を指す強制設定（Unix: `HOME`・`TMPDIR`、Linux はさらに `XDG_CONFIG_HOME`・`XDG_CACHE_HOME`・`XDG_DATA_HOME`。Windows: `TMP`・`TEMP`）と、親に値があるときだけ引き継ぐ変数（Linux: `FONTCONFIG_FILE`・`FONTCONFIG_PATH`、Windows: `SystemRoot`、macOS: なし）だけを渡す。`PATH`・`LANG`・`LC_*`・`TZ`・`*_PROXY`（識別面。SEC-2）、`LD_*`・`DYLD_*`（注入経路）は渡さない。実機確認の状態は**すべて未確認**（Windows の `SystemRoot` は js 子プロセスでは実績あり・Servo ホストでは未確認）。HOME を一時ディレクトリにした場合の fontconfig キャッシュ再生成コストも未確認。
  - 版の検査（`RENDER-3`・`SEC-2`・`REPAIR-4`・#1540・#1553）: cli はホストの解決・検証（と `required` の隔離判定）の後、起動前に `--version` を 1 回だけ実行して CLI の版（`CARGO_PKG_VERSION`）と完全一致するか比べる。不一致・起動不能・非 0 終了・時間超過・出力過多・形式違いはすべて起動エラー（fail-closed）で、両方の版と揃え方（`cargo install fandhe-browser-servo-host --version <CLI の版> --locked --force`）を示す。上限は時間 10 s・stdout 256 バイト・版トークン 64 バイト、環境は `env_clear` に引き継ぎ許可リストのみ（`HostProcess` と同じ）。この実行は `HostProcess` と同じ判定・同じ OS サンドボックスで包む（render の `run_host_version`。Linux は bwrap の `--unshare-net` のまま中継・UDS を渡さず、macOS はすべての送信を拒否する Seatbelt、Windows は隔離なし）。プロキシは起動せず、到達できる送信先を持たせない。`required` は隔離できなければホストを実行せず起動エラー、`best-effort` は警告して隔離なしで実行する（cli は `HostProcess` と通知先を共有し、警告は 1 回）。cwd と `HOME` は専用 WorkDir（0700・終了時に削除）で、利用者の cwd は継承しない。`best-effort` でも隔離の probe は起動時に行う（結果はプロセス内でキャッシュ）。`HostProcess` の起動引数は `--stdio` のまま変えない。ホストの出力は文法を通った版だけを表示する。利便のための整合確認で信頼の境界ではない。
  - 版の検査の既知の制約: (1) 検査の後に同じパスを `--stdio` で再び exec するため、検査と実行の間の差し替え（TOCTOU）は防げない（信頼の境界ではない）。(2) 版が一致しないと描画を使わない用途（`launch` 等）も止まり、起動時に最大 10 s の同期待ちがある。(3) 隔離なしの経路と macOS では時間切れのとき直接の子だけを止める（孫が残り得るが、時間上限と fail-closed は保つ）。Linux の bwrap 下では `--die-with-parent` と pid 名前空間で孫まで消える。
  - 版を上げる手順: `servo-host/Cargo.toml` は別 workspace のため `[workspace.package]` の version を継承しない。ルートの version を上げるときは `servo-host/Cargo.toml` の `[package] version` も同じ値に更新する（`Cargo.lock` も追従）。ずれると描画系コマンドと launch が全面的に起動拒否になるため、`make check-publish-ready`（`scripts/check-publish-ready.sh`）が両者の一致を検査する。
  - タイムアウトは handshake 10 s・load 既定 30 s（上限 60 s）・screenshot 20 s・visibility / bounding_box 5 s・shutdown 猶予 3 s。load はホスト自身の `timeout` 応答が先に届くよう、待ち時間に暫定 2 s を上乗せする。タイムアウト時は kill してホストを破棄する。

### 2.7 資源制限・クラッシュ・ライフサイクル

- 親側の RSS 監視と kill は js の `read_child_rss_bytes` 方式（`process_engine.rs`・`resource_limits.rs`）を踏襲する。子側の `RLIMIT_AS` は SpiderMonkey の仮想アドレス予約と衝突しうるため使わない（**未検証**）。Windows の Job Object は js の承認済み範囲を流用できるか実装時に確認し、新たな `unsafe` は承認事項とする。
- クラッシュ・タイムアウト・プロトコル違反は、当該呼び出しを `RenderError`（§3 の新 variant）で失敗させ、成功を装わない。ホストは破棄し、次回呼び出しで再起動する。サーキットブレーカーとして 60 秒内に 3 回失敗したら一定時間再起動しない。
- `hello` を受け取る前の終了（引数検査での即終了など、CLI とホストの版の不一致が典型）は `RenderError::HostExitedBeforeHello` として `HostUnavailable` と区別する（`RENDER-3`・#1539）。ブレーカーの数え方は変えない。CLI は版を揃えて入れ直す案内を出す。
- ホスト再起動後は、ロード済みページの状態をホストの起動世代にも対応づける。クライアントはナビゲーション世代に加えて「ページをロードしたホストの起動世代」を保持し、どちらかが現在値と異なれば現在のページを再ロードしてから描画する。これにより、同じページでのスクリーンショット再試行がクラッシュ後の新しいホストでも空ページにならない。
- 遅延起動（最初の描画要求時）とし、`AppState` / クライアントの drop 時に `shutdown`、猶予後に kill する。
- **実装で決めた値（TASK-38・#994）**: サーキットブレーカーの冷却時間は設計に値が無いため暫定 60 秒（半開き 1 回で、失敗なら再作動）。RSS 上限の既定は暫定 2 GiB（Servo の実測は TASK-82）。**Windows の RSS 監視と Job Object は未実装**（`windows-sys` への依存辺と新規 `unsafe` が要るため承認事項。プローブが無い OS では監視しない）。Linux は `/proc/<pid>/status`、macOS は `/bin/ps` で測る。

## 3. 本体側の境界

### 3.1 render crate の役割

`fandhe-browser-render` は Servo に依存しない IPC クライアント crate にする（`TASK-33`）。プロトコル型・コーデック・子プロセス管理・`HostRenderer`（core の `Renderer` の実装）を持つ。依存は core のみ（AGENTS.md の依存方向を維持）で、JSON には workspace 承認済みの `serde` / `serde_json` を使い、新規依存は作らない。

ホストはプロトコル型を render crate から借りない。render が core 経由で `ring` / `v8` 等を引き、ホストの `Cargo.lock` が肥大・競合するため。契約は次の 2 点で担保する。

- `docs/design/render-host-protocol.schema.json`
- ゴールデン JSON fixture を `crates/fandhe-browser-render/tests/fixtures/protocol/` に置き、クライアント側・ホスト側の契約テストの双方が読む（puppeteer 契約テストの先例）

### 3.2 feature `rendering` の扱い（決定 2026-10-15）

| 案 | 利点 | 欠点 |
| -- | ---- | ---- |
| A（推奨）: feature 廃止、設定でホストパスを与えたときだけ起動 | 別配布のホストを既定の cli から使える。クライアントは小さく Servo を含まず `RENDER-1`・`RENDER-2` を損なわない | 影響箇所の整理が要る（下記） |
| B: feature を残す | 現行の仕組みを維持 | feature が既定オフだと、別バイナリを入手しても既定 cli から使えない |

案 A の副作用と対応:

- `scripts/check-render-isolation.sh` の PATTERN `servo|fandhe-browser-render` は、render が常時依存になると cli の既定ツリーで常に一致する。パターンを `servo` 系のみに絞り、render 自体は許容する。
- `make lint-rendering` / `test-rendering`・`cargo-gate.sh` は feature 未定義なら skip する実装のため、そのまま残るか撤去できる。
- `workspace.dependencies` の path 参照と cli の `optional` を外す。CI の `--all-features` は無影響。

### 3.3 core の最小変更（実装 Issue で core-builder が行う）

現行の `Renderer`（同期・object-safe・`Send + Sync`）には「どのページを描画するか」を指定する手段が無い。

- 推奨: 既定実装付きメソッド `load(&self, page: &PageRef)`（既定は `RenderingDisabled`）を追加する。呼び出し側の `AppState` は `navigation().current_generation()` を比較し、変化していれば `load` を呼んでから描画系メソッドを呼ぶ。
- 代替: クライアントにナビゲーション世代の取得関数を注入する。`AppState` と `Renderer` が相互参照して循環するため採らない。
- `RenderError` に `Timeout` / `HostUnavailable` / `ProtocolMismatch` 等の variant を追加する（`#[non_exhaustive]`）。内部詳細は引き続き `Display` / `Debug` に出さない。

### 3.4 描画対象の取得（決定 2026-10-15）

v1 はホストが自分で fetch する（URL のみ送る）。Cookie・セッションは本体と共有しない（PROF 境界）。本体が取得済みの HTML との乖離が起きうることを制約とする。

### 3.5 cli

設定でホストパスが与えられたとき、`HostRenderer` を `AppState::new` に注入する。未設定は `with_disabled_renderer`。起動時にホストを自動探索して起動しない（例外: ホストが解決できるときだけ、短命の `--version` を egress 隔離の下で 1 回実行して版を検査する。§2.6・#1540・#1553）。

## 4. Servo ホスト側 `deny.toml`

- `servo-host/deny.toml` の `[licenses] allow` に、本体と同じ許可集合に加えて `MPL-2.0` を足す（`MPL-2.0 AND BSD-3-Clause` は `BSD-3-Clause` が既許可のため `MPL-2.0` だけで足りる）。
- `CDLA-Permissive-2.0` は `[[licenses.exceptions]]` で `crate = "webpki-roots"` に限る（版固定の併記案あり）。許可リストには入れない。
- `MPL-2.0` を crate ごとの例外にしない。83 件が対象で版更新のたびに壊れ、ホスト workspace 全体が MPL の範囲という決定とも合うため。
- 本体の `deny.toml` は変更しない（`OSS-2`・`OSS-3`）。
- `[bans]` に `mozangle`・`gstreamer` 系・`wgpu` 系・LGPL 系を明示し、`default-features = false` の効果の回帰を検出する。`[sources]` は crates.io のみとし git ソースを拒否する。`mozangle` は `wrappers = ["servo"]` で、Windows の `no-wgl`（ANGLE）に限り servo からの直接依存だけ許す（#999）。
- `[graph]` は `all-features = false`、`targets` は 3 OS の主要ターゲットに限定する（aarch64-windows / musl は検証待ち）。
- 試作の `cargo deny` は拒否 85 件（`MPL-2.0` 83・`MPL-2.0 AND BSD-3-Clause` 1・`CDLA-Permissive-2.0` 1）。FreeType 本体（FTL）と libpng の帰属は `cargo deny` で検出できないため手動確認項目とする（§5）。

### 4.1 2 workspace での実行

- `scripts/deny.sh` を 2 回呼ぶ形に拡張する（本体、および `--manifest-path servo-host/Cargo.toml` + ホストの `deny.toml`。フラグ位置は `cargo deny --help` で確認）。`make deny` は両方、`make deny-servo-host` はホストのみ。
- `make check-deny-license-reject`（canary）は、本体が従来どおり MPL を拒否すること、ホストが GPL / LGPL canary を拒否すること、ホストが `webpki-roots` 以外の `CDLA-Permissive-2.0` を拒否することを検証するよう拡張する。
- CI は `rust-lint`（ubuntu）に deny ステップを足す。新しい check-run を作らずに済み、OS 非依存。

### 4.2 分離検証の 2 workspace 化（`RENDER-1`・`TASK-34`）

- 本体側の既存検査 A / B は維持する（PATTERN は §3.2 のとおり調整）。
- C: 本体の `cargo metadata` に `servo-host` が含まれず、ルート `Cargo.lock` に `servo` / `mozjs` 系が無い。
- D: `servo-host` の依存ツリーに本体 workspace の package 名（`cargo metadata` で得た本体メンバーの集合。ホスト自身の `fandhe-browser-servo-host` は集合に含まれないため `fandhe-browser-*` の前方一致では判定しない）・`v8`・`boa_engine` が含まれない（逆方向の混入防止）。
- E: `servo-host` の依存ツリー（`cargo tree --target all` の出力全体）に `webgl|webgpu|gstreamer|lgpl` が含まれない（試作の検査パターン）。`mozangle` だけは Windows の `no-wgl` に限り許可し、非 Windows ターゲット（`deny.toml` の `[graph] targets` と一致を検査）ごとの `cargo tree --target` で禁止する（#999）。
- 自己テスト（合成した `cargo tree` 出力）を追加し、大量一致時の fail-open を防ぐヒアストリング方式を踏襲する。

## 5. NOTICE・帰属表示

実際の変更は「Servo ホストの実装と同時に記載する」運用（[licensing](../../.claude/rules/licensing.md)）に従い、実装 Issue（N1）で行う。

- `NOTICE` §2 の MPL 適用範囲を「Servo ホストの配布物（`fandhe-browser-servo-host` バイナリとそれを含むコンテナイメージ）」に書き換える。本体バイナリには含まれない。Servo 版は実装時点の `0.6.0`（現行の版は `NOTICE` を正とする。#1297 で `0.7.0` へ一時的に上げた）、ソース入手先は `https://github.com/servo/servo` の対応タグ、ソースは無改変と明記する。MPL-2.0 全文を `LICENSE-THIRD-PARTY` に追加する。
- `NOTICE` §1 の CA バンドルの注記は「MPL-2.0 は `fandhe-browser-render` 配下に限る規約の例外」から「Servo ホスト workspace 外の例外（データファイル）」へ言い換える。
- FreeType は FTL を選び、`NOTICE` に FTL の帰属文（FreeType のソース配布物の `docs/FTL.TXT` の文言に一致させる）を載せる。ホスト単体では FreeType を同梱しないが、FTL の帰属は残す（オーナー判断 2026-10-29・#1001）。GPLv2 側は選ばない旨を明記する。
- libpng は現行構成（`bundled_freetype` 無効）ではホストのバイナリに同梱されない。system の libfreetype・libfontconfig 経由で OS から供給されるため、著作権表示・許諾文の扱いは下記の FreeType の項に従う。同梱構成に変える場合は PNG Reference Library License v2 の著作権表示・許諾文を `LICENSE-THIRD-PARTY` に収録する。
- Cargo 依存約 569 件分の著作権表示のため、`scripts/gen-cargo-licenses.sh` を manifest パスを受け取れる形に拡張し、ホスト配布物用の `LICENSE-CARGO-DEPENDENCIES` を別に生成する。`webpki-roots`（CDLA-Permissive-2.0）の帰属を含める。
- `baked-in-resources`（`servo-default-resources`）に埋め込まれるリソースファイルのライセンスを手動確認する。
- FreeType・fontconfig は system の共有ライブラリを実行時に読み込む（解決済み・#1001）。`servo-host/Cargo.toml` は `default-features = false` と `baked-in-resources` のみで `bundled_freetype` を無効にしているため、ホスト単体の配布物には FreeType・fontconfig は同梱されない（musl・glibc の 3 構成で 2026-10-29 に実測。§7.1）。libpng・expat・bz2・brotli は system の libfreetype・libfontconfig 経由で入り、harfbuzz は harfbuzz-sys の `bundled` により全構成で静的に取り込まれる。配布物ごとの出所は一意で、同梱版と system 版の判別は不要になった。`bundled_freetype` を有効にした場合は試していない（feature の切り替えはしていない）。
- `NOTICE` は配布形態別に分けず 1 本のまま（オーナー判断 2026-10-29・#1001。§13 項目 9）。注記で次を分ける。ホスト単体の配布物には FreeType・fontconfig・libpng 等は同梱されない。コンテナでは OS パッケージとして入るため、そのパッケージの著作権ファイル（`/usr/share/doc` 配下）を残す。FreeType の FTL の帰属は残す。
- ライセンス判断が要る事項は Agent が決めず、オーナーへ報告する。

### 5.1 `servo-default-resources` 埋め込みリソースの手動確認（TASK-38・Issue #996）

`servo-default-resources` 0.6.0（crate の SPDX は MPL-2.0、上流タグ `v0.6.0` の `components/default-resources`）が `include_bytes!` で埋め込む 11 ファイルを確認した。ファイル個別のライセンスヘッダは `gatt_blocklist.txt` のみにあり、他は crate の SPDX（MPL-2.0）に従う。上流に個別の出所表記が無いものは推測で埋めず「未確認」とする。

0.7.0 への更新（#1297）で再確認した: `servo-default-resources` 0.7.0（上流タグ `v0.7.0`、コミット `aac43a3f`）の `include_bytes!` の 11 ファイル一覧は 0.6.0 と同じで、内容が変わったのは `debugger.js` のロジックだけ（ライセンスヘッダ・出所の記載の追加なし）。個別のライセンスヘッダが `gatt_blocklist.txt` のみである点も変わらないため、下表は 0.7.0 でも有効である。

| ファイル | 出所 | ライセンス | 確認状態 |
| -------- | ---- | ---------- | -------- |
| `about-memory.html` | Servo | MPL-2.0（crate の SPDX） | ヘッダ無し。crate の SPDX に従う |
| `badcert.html` | Servo | MPL-2.0（同上） | 同上 |
| `crash.html` | Servo | MPL-2.0（同上） | 同上 |
| `debugger.js` | Servo（SpiderMonkey Debugger API 向け。Mozilla のドキュメントへのリンクコメントのみで、コード引用は確認していない） | MPL-2.0（同上） | 同上 |
| `directory-listing.html` | Servo | MPL-2.0（同上） | 同上 |
| `json-viewer.html` | Servo | MPL-2.0（同上） | 同上 |
| `neterror.html` | Servo | MPL-2.0（同上） | 同上 |
| `rippy.png` | Servo（20x20 の小アイコン） | MPL-2.0（同上） | 出所の一次資料は未確認 |
| `gatt_blocklist.txt` | WebBluetoothCG/registries の `gatt_blocklist.txt`（ファイル先頭に Source・License の記載） | Apache-2.0（上流 `LICENSE` を取得して確認） | 確認済み。Apache-2.0 の著作権表示・許諾文の扱いは未判断（オーナー確認事項） |
| `public_domains.txt` | Public Suffix List 由来と推定（8818 行） | 未確認（PSL 自体は MPL-2.0 と記憶しているが一次資料で未確認） | 未確認 |
| `hsts_preload.fstmap` | HSTS preload list から生成されたバイナリと推定 | 未確認 | 未確認（バイナリで先頭ヘッダ無し） |

未確認の 3 件（`rippy.png`・`public_domains.txt`・`hsts_preload.fstmap`）の出所・ライセンスは Servo の履歴を一次資料として後続で確認する。

## 6. CI への影響

### 6.1 変更検出

`scripts/ci-changes.sh` に `servo=true|false` を出力に追加する。

- `servo=true`: `servo-host/*`、`crates/fandhe-browser-render/*`（契約 fixture を含む）、`docs/design/render-host-protocol*`、`scripts/*`、`.github/*`、`rust-toolchain.toml`、入力が空のとき（fail-closed）。
- 推奨: `servo-host/*` のみの変更では `rust` を立てず `servo` のみ立てる。根拠は、本体の Rust テスト・ビルドが `servo-host/` を読まない（契約 fixture は `crates/` 側に置く）こと。判定スクリプトのコメントにも書く。
- 自己テストにケースを追加し、`bash scripts/ci-changes.sh --self-test` を通す。

### 6.2 ジョブ構成

| 案 | 内容 | 評価 |
| -- | ---- | ---- |
| 推奨 | 明示ジョブ `servo-host-ubuntu`（PR + main）、`servo-host-macos`・`servo-host-windows`（main / `workflow_dispatch` のみ。PR は skipped）。matrix は使わない | required check-run が 16 件から 19 件に増える。ruleset 更新はオーナー作業 |
| 代替 1 | 既存 `harness-*` にステップを足す | ruleset 更新は不要だが、Servo の長いビルドで harness ジョブが遅くなり責務も混ざる |
| 代替 2 | PR では `cargo check` のみ | 速いが、リンク・実行時の誤りを PR で検出できない |

- 推奨案の内容: fmt、dev プロファイルでの clippy（`-D warnings`）と test、分離検証 C〜E、ホスト単体の契約テスト。release ビルド + サイズ検査は main と release 前のみ。
- `windows-servo-prereqs` は、`servo-host-windows` が前提検証を内包するまで残す。
- ジョブ追加時は ruleset の required checks 同時更新、SHA 固定 action、最小 `permissions`、`timeout-minutes` を実装 PR のチェックリストとする（[ci](../../.claude/rules/ci.md)・[github-actions](../../.claude/rules/github-actions.md)）。ruleset の更新は本設計・実装 Agent が行わず、要対応事項として報告する。

### 6.3 cache・所要時間

- cache 対象は `~/.cargo/registry` と `servo-host/target`（`scripts/ci-prune-target.sh` を workspace ルート指定に対応させる）。key は `rust-<os>-servohost-vN-<rustc cachekey>-<hashFiles('servo-host/Cargo.lock')>` とし、ルート `Cargo.lock` と独立にする。
- 現状 3 OS で約 6.3 GB に、Linux のホスト（`target` 約 8 GB から**推定**で数 GB。要実測）が加わり上限 10 GB に迫る。推奨は v1 では ubuntu のみ cache し、macOS / Windows は main のみ実行・cache なしとして `timeout-minutes` は実測で決める。
- 所要時間は試作の実測（コールド 8 分 28 秒〜13 分 10 秒、再リンク約 3 分）に基づく推定だったが、ubuntu の CI 上は実測済み（#1029）。cache なしの main は 49 分 10 秒、cache ありは 17〜22 分で、`timeout-minutes` は 75 分に確定した。実測値は `github-actions.md` の所要時間表を参照する。macOS / Windows は未計測。

## 7. 未検証の点と検証計画

| 項目 | 方法 | 成功基準 | 失敗時 |
| ---- | ---- | -------- | ------ |
| macOS ビルド | `servo-host-macos`（main）・dispatch | `--locked` ビルドと `--selftest` が通る | 原因を報告し、対象外とするかをオーナーが判断 |
| Windows ビルド | `servo-host-windows`（#229 条件 5）。前提は `windows-servo-prereqs` の範囲 | 同上 | 報告し、一級対応方針との整合を判断 |
| 実ページのスクリーンショット | `harness/render-screenshot/` の runbook を流用し代表サイトで PNG 取得 | `paint()` で PNG 取得に成功（#50 の知見）。画質比較は `RENDER-5` の別タスク | 切り分けは `TASK-36` の知見を引き継ぐ |
| musl 静的 / scratch 配布 | musl ターゲットでビルドし `ldd` / `check-static-link.sh` 相当で確認 | 動的リンク 0 | glibc 系ベースの別コンテナイメージを代替案とする。2026-10-29 に実測（§7.1）。静的強制は起動直後に SIGSEGV で不成立のため、glibc（Debian slim）系の別イメージを採用（#1001） |
| FreeType の同梱 / system 判別 | `nm` / `ldd` / ビルドログでシンボル出所を確認 | 配布物で出所が一意 | `NOTICE` を配布形態別にする。2026-10-29 に実測（§7.1）。出所は一意で基準を満たしたため、配布形態別の分割は採らない（NOTICE は 1 本のまま注記で分ける。オーナー判断 2026-10-29・#1001） |
| 最小 system 依存 | `make`・Python・`uv`・libclang の個別欠如試験 | 必須ツールの一覧が確定 | `docs/setup/` に前提を追記 |
| IPC 実装・負荷 | Servo なしの偽ホストで再起動・タイムアウト・巨大フレーム・不正 JSON を試験 | 各ケースで成功を装わず失敗する | 上限値を調整 |
| メモリ・リソース制限 | 実ページの RSS 計測（`TASK-82` と連携） | 親側 RSS 監視が効く | OS 制限の追加を検討（承認事項） |
| 読み込み検査（§2.5・§13 項目 11） | 実ホストの結合テスト（`servo-host/tests/stdio_server.rs` の `load_allowlist_rejects_disallowed_scheme`・`load_allowlist_rejects_disallowed_main_frame`・`websocket_connections_are_refused`）で、許可外 scheme の読み込み・iframe・Worker・主フレームの遷移・WebSocket が空の応答で終わり、同じページの http・`data:` の対照は描かれること。真理値表は単体テスト `url_allowlist_truth_table`（`servo-host/src/guard.rs`）で確かめる | 許可外が読み込まれないこと | 検査点を足す。`ServoDelegate` 側を通る経路は Servo 0.7.0 の設定では作れず、テストで固定していない |

### 7.1 musl・配布形態の実測（#1001）

実測の数値と経過は Issue #1001 のコメント（https://github.com/Fandhe-AI/fandhe-browser/issues/1001#issuecomment-6490592615）を参照する。ここでは判断に必要な要点だけを記す。

- 対象: ソース 165eda0（`servo =0.7.0`）の `servo-host/` を release ビルド。イメージは rust:1-alpine（rustc 1.99.0）と rust:1-bookworm（rustc 1.99.1）。実測マシンは DGX Spark（GB10・aarch64・121 GiB）と TASK-87 の Linux VM（x86_64）。

| 構成 | 結果 | 判断 |
| ---- | ---- | ---- |
| musl 既定（Alpine・`-crt-static`） | aarch64・x86_64 とも成功。動的リンク（PIE）で、直接 NEEDED は 6 個、ldd の閉包は 12 行。`--selftest` はランタイム（wayland・mesa 等）を入れると通るが、最小集合は未確定。素の Alpine では wayland-sys が panic する | 動的リンクのため静的配布の基準を満たさない |
| musl 静的強制（`--target` と `+crt-static`） | リンクは通り NEEDED は 0。しかし両アーキとも起動直後に SIGSEGV する（原因未特定）。実行時の dlopen（wayland-client・libEGL）も未検証。`--target` では bindgen が libclang を dlopen できない問題もある | 不成立。musl 静的・scratch では配布しない（オーナー判断 2026-10-29） |
| glibc（rust:1-bookworm） | 標準の clang 14・15 では mozjs の bindgen が失敗する。backports の clang-19 で成功。直接 NEEDED は 8 個、ldd の閉包は 13 行 | 採用。glibc（Debian slim）系の別イメージでビルド・配布する。ビルドには backports の clang-19 が要る |

- 出所: `servo-host/Cargo.toml` は `bundled_freetype` を無効にしているため、FreeType・fontconfig は system の共有ライブラリ、harfbuzz は静的。出所は構成によらず一意で、ホストのバイナリには FreeType 等が入らない（§5）。
- 帰結: Servo ホストのコンテナは glibc 系の別イメージとする（§13 項目 8）。本体の既定・軽量コンテナ（musl 静的・scratch）は変えない。コンテナの配布の起票は spec の整理（`TASK-64`・`CTR-4`）後とし、#1001 では起票しない。
- 未検証の点: 静的強制の SIGSEGV の原因、Servo の dlopen 先（wayland-client・libEGL）の実行時の扱い、`--selftest` のランタイムの最小集合、`bundled_freetype` を有効にした場合の出所（試していない）。

## 8. #230〜#237 の書き換え案と依存順

Issue 本文の書き換えと追加起票は、本設計のレビュー後に行う（本書では実行しない）。

### 8.1 既存 Issue の新しい位置づけ

| 既存 | 新しい位置づけ |
| ---- | -------------- |
| #230 (38.1) | ホスト側。`servo-host/src/engine.rs` で `ServoBuilder` / `SoftwareRenderingContext` の生成・破棄。依存欄は H0・H1・H2 に差し替える |
| #231 (38.2) | ホスト側。WebView 生成・`load`・post-load フレーム待機・`paint()`（`page.rs`） |
| #232 (38.3) | ホスト側。PNG エンコード（`screenshot.rs`）。画像 crate の追加は承認事項 |
| #233 (38.4) | ホスト側。可視性（`layout.rs`）。取得手段の確認を先頭ステップに置く |
| #234 (38.5) | ホスト側。境界ボックス。同上 |
| #235 (38.6) | 本体側。render crate の `HostRenderer` が core の `Renderer` を実装（`host_renderer.rs`） |
| #236 (38.7) | cli。ホストパス設定と `AppState` への注入、feature 整理 |
| #237 (38.8) | 2 分割: (a) 偽ホストを使うクライアント結合テスト（本体 CI）、(b) 実ホストでの結合テスト（`servo-host-ubuntu`） |

### 8.2 追加が要る Issue（各 2h 以下・1 関心事）

| ID | 内容 | 担当 |
| -- | ---- | ---- |
| R1a | AGENTS.md のレビュー観点更新（依存ライセンス・レンダリング層の隔離・crate 境界・ローカルゲート・依存方向表の render / cli）。**最初の実装 PR より前**に単独 PR で入れる。旧記述のままだと MPL 許可 PR が P0 になるため。変更自体が P0 になりうる点を PR 本文に書き、受容に ruleset の一時変更が要る場合はオーナー作業 | docs-writer |
| R1b | README・CLAUDE.md・`.claude/` の rules / agents、`docs/design` 冒頭の状態の更新。CI 記述は CI 変更 PR と同時に行う | docs-writer |
| H0 | `servo-host/` の足場（Servo 依存なしの `main.rs`、自前 `[workspace]`、ルート `exclude`、manifest 検査の追加） | infra-builder |
| H1 | `servo =0.6.0` 追加（**依存承認が前提**）+ `Cargo.lock` + Linux ビルド + `baked-in-resources` + `--selftest` | render-builder |
| H2 | `servo-host/deny.toml` と `scripts/deny.sh`・canary の 2 workspace 化 | infra-builder |
| H3 | `check-render-isolation.sh` の PATTERN 見直しと検査 C〜E + 自己テスト | infra-builder |
| H4 | ホスト側 stdio サーバーループ（フレーム読み書き・`hello`・ディスパッチ・上限・`shutdown`・stderr ログ）。偽ハンドラで単体テスト可能な形 | render-builder |
| P1 | プロトコル仕様 `render-host-protocol.schema.json` とゴールデン fixture | docs-writer + render-builder |
| K1 | render crate のプロトコル型とコーデック（上限の確保前検証、`unwrap` / 添字アクセスなし） | render-builder |
| K2 | render crate の子プロセス管理（バイナリ解決・環境変数許可リスト・遅延起動・タイムアウト・kill・再起動 / ブレーカー・RSS 監視） | render-builder |
| C1 | core の `Renderer` ページ指定拡張と `RenderError` variant 追加（設計は §3.3） | core-builder |
| S1 | `ci-changes.sh` の `servo=` 出力と自己テスト | infra-builder |
| S2 | `servo-host-ubuntu` ジョブ + cache（ruleset 更新はオーナー作業） | infra-builder |
| S3 | `servo-host-macos` / `servo-host-windows` ジョブと `windows-servo-prereqs` の整理 | infra-builder |
| N1 | `NOTICE`・`LICENSE-THIRD-PARTY`・`gen-cargo-licenses.sh` のホスト対応（H1 で依存ツリー確定後） | docs-writer |
| V1〜V3 | 検証 Issue（macOS / Windows ビルド、musl・scratch 配布、実ページ）。判断を要する項目は owner:human で分ける | 人間 + infra-builder |

crates.io のソース配布は #1538 配下で進める（§13 項目 10）。コンテナイメージの配布は `TASK-64` 系との関係を spec 側で整理してから別途起票する。バイナリ配布は未決。

### 8.3 依存順

| 波 | 内容 |
| -- | ---- |
| 0 | R1a、#935 のレビュー |
| 1（並列） | H0、P1、C1 |
| 2 | H1（H0 後・依存承認後）、K1（P1 後）、S1（H0 後） |
| 3 | H2・H3（H1 後）、H4（P1・H0 後）、#230（H1 後）、K2（K1 後） |
| 4 | #231、S2（H1・H2・H3・S1 後）、#235（K1・K2・C1 後）、N1（H1 後） |
| 5 | #232・#233・#234、#236・#237a（#235 後）、R1b（CI 変更と同時） |
| 6 | #237b（#232〜#234・H4・S2・#235 後）、S3、V1〜V3 |

依存追加を伴う H1 とホスト側 PNG エンコードは、dependency-policy の承認を得るまで着手しない。ruleset 更新・egress 方針・コンテナとバイナリの配布方式は owner:human として分ける。

## 9. 周辺文書の更新計画

| 文書 | 更新内容 | 時期・依存 | 担当 |
| ---- | -------- | ---------- | ---- |
| `AGENTS.md`（4 行 + 依存方向表） | MPL の許可範囲、render 隔離の定義、crate 境界、ローカルゲート | R1a（最初の実装 PR より前・単独 PR） | docs-writer |
| `README.md` | 二層アーキテクチャ・crate 一覧・ライセンス節 | R1b | docs-writer |
| `CLAUDE.md` | ツリー、`render-builder` の担当範囲、委譲表への `servo-host/` | R1b | docs-writer |
| `.claude/rules/ci.md`・`github-actions.md`・`licensing.md` | ジョブ構成・required 件数・cache | CI 変更 PR（S2・S3）と同時 | infra-builder + docs-writer |
| `servo-out-of-process-prototype.md`・`render-feature-gate.md` | 冒頭の「状態」を本書への参照に更新 | R1b | docs-writer |
| `NOTICE` | §5 のとおり | N1（H1 後） | docs-writer |

AGENTS.md の変更はレビュー基準の変更として AI レビューで P0 になりうる（PR #960 の事例）ため、本 PR には含めず R1a で単独に扱う。

## 10. spec 側の課題（spec リポ側で対応。本リポからは編集しない）

| ID | 課題 |
| -- | ---- |
| `RENDER-1` | 「本体側で呼び出し部分を feature gate で切り替えるか」を設計の結論（feature 廃止・設定による起動）に合わせて確定する。検証方法（2 workspace）の記述も更新する |
| `RENDER-2` | Servo ホストのバイナリサイズ基準を新設するか。試作値約 65〜68 MB は現行上限 91,480,000 B（Chromium 比 80% 削減）以内だが、3 OS と既定プロファイル相当は未計測 |
| `TASK-33` | 成果物の「workspace 側 Cargo.toml の feature 定義」と、cli だけが render を optional で持つ記述が feature 廃止の推奨と食い違う |
| `TASK-34` | 2 workspace での検証内容（検査 C〜E、deny の 2 系統）を追記する |
| `TASK-35` / `TASK-82` | サイズ増分が「ホスト単体の絶対値」になり、メモリ増分の測定対象もホストの RSS に変わる |
| `TASK-38` | 枝番 38.1〜38.8 を §8 の分け方に合わせて改訂し、担当と成果物パスを更新する |
| 設計文書群 | `self-repair-design.md` 決定 4、`js-engine.md` の feature `rendering` 組合せ、`plugin-boundary-policy.md` の「コンパイル時プラグインの例に `rendering` を挙げている」点（Servo ホストは実行時の別プロセス境界に近い）（`PLUG-1`） |
| crates.io のソース配布 | 配布方式の注記を 2026-11-07 の判断（#1538。§13 項目 10）に合わせて更新する |
| コンテナ（`CTR-4`） | Servo ホストの配布物をコンテナで扱うか。scratch + musl の前提とホストの動的リンクが衝突しうる（#1001 で musl 静的・scratch は不成立と実測。§7.1） |
| `OSS-2` / `OSS-3` | 注記の整合（ホスト workspace 限定の MPL 許可） |

## 11. セキュリティ考慮（OWASP Top 10 観点）

| 観点 | 対応 |
| ---- | ---- |
| インジェクション | URL・セレクタの検証、セレクタを JS に連結しない、引数配列で起動、シェル不使用 |
| 不安全な設計 | フレーム・URL・セレクタの上限、ページ読み込みのタイムアウト、無制限確保の防止、資源制限、サーキットブレーカー |
| アクセス制御の不備 | stdio IPC はポートを開かない（ただし `--egress-relay-uds` 指定時は中継が `127.0.0.1` のエフェメラルポートを開く。Linux の netns 内を前提とし、宛先は UDS 1 本固定。netns 外で誤用された場合は同一ホストの他プロセスが届き得るが、到達先は本体の許可リスト付きプロキシだけ）、バイナリ解決は絶対パスのみ・PATH 探索なし、他者書き込み可のバイナリを拒否 |
| SSRF | scheme 制限、プライベート IP ポリシーを core と揃える。読み込み先は許可リスト（§2.5。2026-11-17）で制限する。隔離なしの経路では送信を本体のプロキシ経由に限り、Windows は起動を拒否する。egress の宛先判定は本体の CONNECT プロキシ（#1434）が行い、ホストの中継は宛先を選べない（#1435） |
| 識別面（`SEC-2`） | UA 等を変えるメソッドを載せない、未実装を成功扱いにしない |
| 機密情報の露出 | ホストのエラー詳細を `Display` / `Debug` に出さない、stderr は上限付き、ログから URL クエリを除く |
| 脆弱な依存 | `=x.y.z` 固定、ホスト側 `cargo deny advisories`、更新運用は別 Issue |
| プロファイル境界 | プロファイルごとに別ホストプロセス、一時ディレクトリ、Cookie 非共有 |
| プラグイン境界（`PLUG-1`） | 動的ライブラリのロードなし、ホストからの応答も untrusted として検証 |
| ライセンス | MPL をホスト workspace に限定し、本体の `deny.toml` は変えない |

## 12. 受け入れ条件との対応

| 受け入れ条件 | 節 |
| ------------ | -- |
| 設計: workspace 配置・本体 `members` からの除外 | §1 |
| 設計: プロセス間通信（方式・メッセージ・上限・タイムアウト・起動・資源制限） | §2 |
| 設計: 本体側の境界（render crate・core 変更・feature・cli） | §3 |
| 設計: ホスト側 `deny.toml` と分離検証 | §4 |
| 設計: `NOTICE`・帰属表示 | §5 |
| 設計: CI への影響 | §6 |
| 設計: 未検証の点と検証計画 | §7 |
| #230〜#237 の書き換え案と依存順 | §8 |
| spec 側の課題 | §10 |

## 13. オーナー判断（2026-10-15・2026-10-29・2026-11-07・2026-11-17）

出典は #228。各項目の結果を記す。

1. feature `rendering` を廃止し、設定でホストパスを与えたときだけホストを起動する（§3.2）: 決定
2. v1 は URL のみ送り、ホストが自分で取得する。Cookie は共有しない（§3.4）: 決定
3. core `Renderer` に描画対象ページを指定するメソッドを追加する（§3.3）: 決定
4. ホストのビルド cache は ubuntu のみ（§6.3）: 決定
5. `ci.yml` のジョブは 16 → 19、required の合計は 20 → 23。ruleset はジョブ追加 PR のマージ時に更新する（§6.2）: 決定
6. egress 方針（Servo 内部のリダイレクト・サブリソース・DNS リバインディング。§2.5）: GA 前に判断する（#1003。比較設計と実測計画は [servo-host-egress-isolation.md](./servo-host-egress-isolation.md)）
7. 配布方式（§8.2・§10）: ホスト単体のソース配布は項目 10 で決定。コンテナは spec の整理（`TASK-64`・`CTR-4`）後に起票する。バイナリ配布は未決

2026-10-29 の判断（項目 8・9）は Issue #1001 による。実測の根拠は §7.1 と §5。

8. Servo ホストのコンテナは musl 静的・scratch では配布せず、glibc（Debian slim）系の別イメージとする。本体の既定・軽量コンテナ（musl 静的・scratch）は変えない。ビルドには backports の clang-19 が要る。コンテナの配布の起票は項目 7 のとおり後続とする: 決定
9. `NOTICE` は配布形態別に分けず 1 本のまま。ホスト単体の配布物には FreeType・fontconfig・libpng 等は同梱されないこと、コンテナでは OS パッケージの著作権ファイルを残すこと、FreeType の FTL の帰属は残すことを注記で分ける（§5）: 決定

2026-11-07 の判断（項目 10）は Issue #1538 による。

10. Servo ホストを crates.io へ 9 個目の crate としてソース配布する。crate は自前のコードと `Cargo.lock` だけで Servo のソースを含まず `MIT OR Apache-2.0`、対象は macOS・Linux（Windows は対象外）、Servo は `=0.7.0` のまま、版は他 crate と揃える。`NOTICE` の MPL 適用範囲（ビルドしたバイナリに適用）と、隔離（`RENDER-1`）・MPL の扱い（`OSS-2`・`OSS-3`）は変えない。コンテナとバイナリの配布は項目 7 のとおり別扱い: 決定

2026-11-17 の判断（項目 11〜13）はオーナー判断。対応する Issue の番号は本書に記さない。

11. ページの全ての読み込み（主フレーム・iframe・サブリソース・Worker）を、許可リスト（scheme が `http`・`https`・`data`・`blob` と、`about:blank`・`about:srcdoc` の完全一致）で検査する。許可外は空の応答で終える。WebSocket のハンドシェイクは隔離の有無によらず拒否する。主フレームの URL 検証（`http` / `https` のみ）は従来どおり。ディレクトリ一覧は無効にする（§2.5）: 決定
12. 隔離できない unix（`best-effort` の降格）でも、本体の egress プロキシを起動し `--egress-proxy-port` でホストへ渡す。プロキシ経由のため、既定（`allow_private_network_access=false`）ではループバック・プライベート宛を拒否する。プロキシの起動に失敗したらホストを起動しない（fail-closed）。ただし遮断はサンドボックスではなく、ホストがプロキシ設定に従うことに依存する（§2.5・§2.6）: 決定
13. Windows（非 unix）は本体のプロキシも隔離も無いため、`best-effort` でも隔離なしの起動を拒否する。Windows では撮影できない。`--version` の版の検査は隔離なしで実行できる（ページを読まない）。Windows 向けのプロキシは後続の作業（§2.5・§2.6）: 決定

## 14. 操作モードのプロトコル（TASK-121・#1582）

対応 `TASK-121` / `MS-10` / ビヘイビア `RENDER-10` / `RENDER-11` / `AISNAP-16`〜`AISNAP-21`（spec `rendering-layer.md`「Servo ホスト側のページの操作」節）。契約はスキーマ（`render-host-protocol.schema.json`）と fixture（§14.8）で、本節はその理由と、スキーマで表せない振る舞いを記す。確定条件 1〜8 は spec `05-tasks.md` の TASK-121。試作の結果は [servo-interactive-poc.md](./servo-interactive-poc.md)。

### 14.1 範囲と起動

- 追加するメソッドは `dom_snapshot`（TASK-122）・`act`・`wait`（TASK-126）の 3 つ。撮影の範囲（`RENDER-9`・TASK-134）と画面サイズ（`RENDER-8`・TASK-133）は含めない
- プロトコルの版（`v`）は 1 のまま。追加のメソッドは `hello` の `capabilities` で知らせ、載せていないホストは `unsupported` を返す（§2.2 と同じ）
- ホストは固定の起動引数 `--interactive` のときだけ 3 つを `capabilities` に載せて受け付け、`hello` の `limits` に §14.6 の項目を足す（fixture `hello-interactive-ok`）。`--interactive` は egress の引数（`--egress-proxy-port`・`--egress-relay-uds`。§2.6）と一緒のときだけ受け付け、単独なら起動を拒否する（fail-closed）
- 本体は、いま動いているホストの実際の隔離状態が `Isolated` で、`allow_private_network_access = false` のときだけ `--interactive` を付けて起動する（確定条件 7・`AISNAP-16`）。ホスト側の上の条件は本体の検査の重ねで、隔離の成立を証明するものではない。内部アドレスへの接続は egress プロキシが止める（TASK-120 の前提）

### 14.2 ページ識別子（確定条件 3）

- `page` は `{"session": <文字列>, "doc_seq": <整数>}`。`session` はホストの起動ごとに変わる値（1〜64 文字。ホストが起動時に作る）で、予測できないことは求めない（同じ値を再び使わないことだけ）。`doc_seq` は 0 から始まる文書番号
- `doc_seq` は、`load`・URL 変更の通知（`notify_url_changed`）・新しい文書の読み込みの開始で 1 上げる（2026-11-18 の判断。#1582）。同じ文書の中の `pushState`・`replaceState`・hash の変更でも上がる。SPA で取り直しが重い場合は、後から同じ文書の中の変更を除く方向で緩める。通知を受けるため `servo-host` に `url =2.5.8` を足す（承認済み。TASK-122）
- `--interactive` のホストは `load` の結果に `page` を足す（fixture `response-load-interactive-ok`）。`act`・`wait` は要求に `page` を持ち、ホストは現在の値と違えば処理せずに `stale` を返す。応答は処理の後の `page`
- 同期の取り直し: ページが `load` の応答の後に `pushState`・hash の変更・スクリプト起点の遷移を行うと、`doc_seq` はホスト側だけで上がる。`dom_snapshot` は `page` を照合せずにいまの文書の DOM と `page` を返すため、本体は `stale` を受けたら `dom_snapshot` で取り直して続けられる（再 `load` は要らない）。本体は返った `page` を保存済みの値と比べ、違えば文書が変わったとして確定結果を差し替える（TASK-124）。`act` は本体が直前に取り直す（`AISNAP-17`）ので、照合の厳密さは保たれる
- `session_lost` はクライアント側の kind。ホストの破棄・再起動の後、クライアントは自動で `load` し直さず、新しい navigate まで `session_lost` を返す（`AISNAP-21`）。確定条件 3 の「テストで固定する」は、クライアントを実装する TASK-123 のテストで行う（本タスクは契約までで、クライアントの実装を含まないため）
- 判断: ページ識別子をホストが持つのは、文書が変わったこと（戻る・進む・スクリプト起点の遷移）を本体が知る手段がホストの通知しかないため。本体のナビゲーション世代（`generation`）とは別の値で、対応づけは TASK-124 の確定結果が持つ

### 14.3 DOM の取得（`dom_snapshot`。確定条件 1・4・5・`RENDER-11`）

- 要求は params なし（`page` を照合しない。§14.2）。結果は `page`・`url`（現在の URL）・`html`・`element_count`・`snapshot`（§14.4）・`excluded`（`iframes`・`shadow_roots` の数）・`redacted`（値を伏せた要素の数）
- 現在の URL が http / https でなければ `invalid_state`（`about:blank` も含む）
- 番号: ホストの直列化器は、主フレームの要素に前順で 0 から番号を振り、各要素に属性 `data-fandhe-n="<番号>"` を付ける。番号は直列化器の整数のカウンタから整形し、ページ由来の文字列を使わない（確定条件 8）。属性は直列化の出力にだけ書き、ページの DOM には足さない（ページから見えない）。ページ自身が同名の属性を持っていても出力しない
- 本体の検査: パースした後に全要素から `data-fandhe-n` を外し、「本体の文書順 → ホストの番号」の対応表を確定結果に持つ（TASK-124）。値は先頭 0 なしの十進の数字列で、0 以上 `element_count` 未満、重複なし。どれかに違反すれば同期失敗にする。番号の無い要素（再パースでパーサーが補った要素）は操作の対象にしない。保存する HTML に番号を残さない（確定条件 1）。本体は `noscript` の中身を、ホストと同じくスクリプトが有効な扱い（生のテキスト）でパースする。ページが同名の属性を文字列で仕込んでも、要素として解釈されなければ番号にならない。解釈されて番号が重なれば同期失敗になる（自分のページの同期を壊すだけで、別の要素を操作させることはできない）
- 判断: 番号を HTML の外の表だけで渡す案は、本体の再パースで要素の並びが変わる場合（試作 (5)。DOM 操作で作った形の 9 試料中 5 件）に対応づけができないため採らない。属性で渡して本体が外す方式なら、並びが変わっても番号は要素に付いて移る
- 現在値: `input` の `value`・`checked`、`option` の `selected`、`textarea` の内容は、DOM の属性ではなく現在値を書く。伏せる対象（`type=password`、`autocomplete` が `current-password`・`new-password`・`one-time-code`・`cc-*` で始まる値）の値は書かない（第 1 層）。本体は保存の前に同じ条件で値を消す（第 2 層。TASK-125・2026-11-19 のオーナー判断。`input` の `value`・`checked`、`textarea` の中身、`select` 配下の `option` の `selected`。`autocomplete` の区切りは JS の `/\s+/` と同じく ASCII 以外の空白も認める。`autocomplete` が 1024 バイトを超えて判定できないときは、すべての `input`・`textarea`・`select` の値を消す。保存する文書と HTML の両方から消すため、要約・CDP の `DOM.*`・要素の指定は同じ結果を見る）
- iframe・shadow root の中身は含めず、数を `excluded` で返す（`REPAIR-3`）。`data:`・`blob:` の iframe も同じく中身を含めない（#1581 の引き継ぎ）
- 上限: ホストは HTML の UTF-8 の長さが 8 MiB を超えるとき、または応答を符号化した後のフレーム長が 12 MiB を超えるとき、切り詰めずに `too_large` を返す（確定条件 5）。本体はパースの時に要素数（200,000）と深さ（512）を数え、HTML の長さと `url` の長さ（2048）・scheme（http / https）も自分で検査する（確定条件 4）。応答フレームの上限は `dom_snapshot` だけ 12 MiB で、他のメソッドは従来の 1 MiB
- 直列化のスクリプトは固定の関数式で、ページのグローバルへ書き込まない。ページは prototype を書き換えて出力を変えられるため、取得した DOM は自己申告として扱う（本体の検査が前提）

### 14.4 操作（`act`。確定条件 2・8・`RENDER-10`）

- 要求は `page`・`snapshot`・`node`（`dom_snapshot` の番号）・`expect`（照合値）・`action`。`action` は `click`・`fill`（`text`・`sensitive`）・`select`（`values`）・`press`（`key`）のどれか
- 照合値は `tag`（小文字のローカル名。必須）と、`id`・`name`・`type` の属性値（本体の写しにあり 256 文字以下のものだけを送る。送ったものだけを照合する）。ホストは `snapshot` と `node` から要素そのもの（下記）を引き、照合値が一致しなければ `stale`。番号が範囲外のときも `stale`
- 要素の同一性: `doc_seq` は遷移でしか上がらないため、番号を「前順で何番目か」として引き直すと、`dom_snapshot` から `act` までの間に要素が増減・移動したときに番号がずれる。`id`・`name` の無い同じ形の要素が並ぶと照合値も一致し、隣の要素を操作しうる（`AISNAP-17`「推測で別要素を操作しない」に反する）。そこで番号は位置ではなく要素そのものを指す。ホストは `dom_snapshot` のとき、番号を振った要素を `evaluate_javascript` の戻り値として受け取り、Servo が要素ごとに振る ID（`JSValue::Element`）を番号の順に保持する。`act` では番号に対応する ID を `window.webdriverElement(id)`（Servo 0.7.0 がページの JS に公開している関数）で要素に戻して操作する。要素が動いても同じ要素を指し、消えていれば `null` で `stale` になる（試作の結果は [servo-interactive-poc.md](./servo-interactive-poc.md) (7)）。ホストが保持するのは最新の取得 1 件だけで、`snapshot` はその取得の識別子（違えば `stale`）。ID は本体へ出さない
- 自己申告: ページは `window.webdriverElement` を置き換えられるため、要素の引き直しも自己申告として扱う（ページは自分の DOM とイベントを常に変えられ、置き換えで得られるのは「自分のページの別の要素を操作させる」ことに限る）。照合値の検査は引き直した要素に対しても行う
- ホストへの値の渡し方: 要素の ID・照合値・`select` の `values` は、ホストが `serde_json` で JSON のリテラルにしてから固定のスクリプトへ渡す（連結しない）。比較はそのスクリプトの中で行う
- セレクタはホストへ渡さない。本体が同期済みの写しの上で解決し、番号で送る（確定条件 8・spec の判断 3）
- 検査の順: 現在の URL（http / https でなければ `invalid_state`）→ `page`（`stale`）→ `snapshot`（`stale`）→ 番号から要素を引き直す（消えていれば `stale`）→ 照合値（`stale`）→ 操作できるか（`not_actionable`）→ 操作 → `fill` は入力の前・1 文字ごと・後でフォーカスが対象にあるか（外れていたら残りを送らず `not_actionable`）
- `not_actionable`: 不可視・`disabled`・他の要素に隠れている・クリック点が iframe 等の上・スクロールしても画面に入らない（WebDriver の `ElementClick` が画面へのスクロールを行う。TASK-126）
- 自己申告の扱い（確定条件 2）: JS から取る矩形・可視判定・クリック点の要素は、ページが prototype を書き換えて偽れる。ホストはこれらを操作してよいかの判定にだけ使い、偽られた場合に起きるのは「別の座標をクリックする」ことに限る（入力値は入力イベントで渡すため、偽装で値が別の経路へ漏れない）。試作 (4) で、`Servo::execute_webdriver_command` の WebDriver の命令を使えば、ページが書き換えられない矩形とヒットテスト（隠れていれば `ElementClickIntercepted`）を Rust 側で取れると分かった。2026-11-19 のオーナー判断で `servo-base =0.7.0` の直接依存を承認し、TASK-126 で採った
- クリック（TASK-126）: WebDriver の `ElementClick` で、要素を画面に入れ、他の要素（iframe を含む）に隠れていないことを確かめる（隠れていれば `ElementClickIntercepted` で `not_actionable`。`option` は Servo が選択とイベントまで行うため `click` では断り、`select` を使わせる）。続けて同じ判定点（`GetElementInViewCenterPoint`。最初の矩形を画面で切り取った中心）へ、マウスの入力イベント（移動・押す・離す）を送る。点が画面の外なら `not_actionable`。判定と入力の送信は別の手順のため、その間にページが要素を動かすと別の座標を押しうる（試作 (4) の限界）。`fill`（`input`・`textarea` が対象。`contenteditable` は `ElementClear` が断るため `not_actionable`）は、操作してよいかを確かめてから `ElementClear` で値を消し、クリックでフォーカスを移して 1 文字ずつキーの入力イベントで渡す。文字ごとに入力の処理が済むのを待ち、フォーカスが対象に残り、遷移の要求も文書の更新も無いことを確かめてから次を送る（ページのハンドラがフォーカスを移しても、残りを別の要素へ入れない。送り済みの文字は戻さない。1 文字ごとに往復するため、長い `text` は `act` の期限〔10 秒〕に届きうる）。`press` は、フォーカスの有無によらず先に `ElementClick` の判定を通し（隠れていれば `not_actionable`）、フォーカスが無ければクリックしてから送る（クリックの副作用も起きる）。フォーカスの確認は WebDriver の `GetActiveElement`、照合値と `disabled`（`:disabled`）は `window.webdriverElement(id)` で引き直した要素で調べる（自己申告）。`press` のキーは固定の一覧（`Enter`・`Tab`・`Escape`・`Backspace`・`Delete`・矢印・`Home`・`End`・`PageUp`・`PageDown`）か 1 文字。`navigated` は最善努力: 入力の処理の後にページから始まる遷移が許可されていれば、新しい文書の読み込みの開始を最大 2 秒待ち、その時点の文書番号で判定する（`act` は読み込みを待たない。応答の遅い遷移・文書のできない応答では偽になりうる。文書番号はフレームを区別しないため iframe の遷移でも真になりうる）。`option` を対象にした `click`・`fill`・`press` は、命令を送る前に `not_actionable`
- `select`（TASK-126）: Servo 0.7.0 の `ElementClick` は、単一選択の `select` の `option` を押しても選択を変えない（`servo-script` の `handle_element_click` の Step 8.6.3 は `multiple` のときだけ切り替える）。そこで選択肢はスクリプトで設定し、`input`・`change` を出す。このイベントは `isTrusted` が `false` になる（既知の制約）。複数選択は `values` の選択肢だけを選んだ状態にし、単一選択に 2 つ以上を渡すと `invalid_params`（スキーマと同じ）、一致する選択肢が無い・選ぶ選択肢が `:disabled`・`select` でないときは `not_actionable`。設定の前に `ElementClick` の判定で操作してよいかを確かめる
- `unsupported`: 新しいウィンドウ（`request_create_new`）・ファイル選択（`FilePicker`）・ダウンロード。delegate の要求は応答せずに捨てても操作は止まらない（試作 (8)）。ダイアログは Servo の既定のまま（`alert` は捨てる・`confirm` は `false`・`prompt` は `null`。操作は止まらない。2026-11-19 の判断。#1592）。`alert()` の文字列が stdout に書かれる件は #1597 で直した（§2.1）。`act` の間に新しいウィンドウ（`request_create_new`）・ファイル選択（`FilePicker`）の要求が来たら、要求は捨てたうえで `act` を `unsupported` で返す（TASK-126）。ダウンロードは Servo 0.7.0 の delegate に通知が無く検出できない（未実装。#1592）
- 入力文字は入力イベント（`notify_input_event`）で渡し、評価する JS へ連結しない。`text` と `press` の値、遷移先の URL のクエリを、ログ・エラー・応答に出さない（応答は `page` と `navigated` だけ）
- 人の操作に見せるための遅延・軌跡・乱数は入れない（`SEC-2`）

### 14.5 待機（`wait`。確定条件 8・`AISNAP-18`）

- 要求は `page`・`condition`・`timeout_ms`（必須・1〜60,000）。`condition` は `selector_present`・`selector_absent`（`selector`）、`text_present`（`text`）、`load_complete` のどれか
- `selector` と `text` には同じ検証（1〜1024 バイト）をかけ、ホストは `serde_json` で JSON の文字列リテラルにしてから固定のスクリプトへ渡す（連結しない）
- 成立すれば `page` を返す。`timeout_ms` までに成立しなければ `timeout`（ホストは破棄しない）。待機中に `doc_seq` が変わったら `stale`（`load_complete` を除く）
- 同時に待てるのは 1 件（`max_in_flight` が 1）。待機中の他の要求を `busy` で返すのは本体（`AISNAP-18`）

### 14.6 上限・時間切れ・メモリ（確定条件 4〜6）

| 項目 | 値 | 判断 |
| ---- | -- | ---- |
| `dom_snapshot` の HTML | 8 MiB（8,388,608 バイト） | spec `RENDER-11` |
| `dom_snapshot` の応答フレーム | 12 MiB（12,582,912 バイト） | spec `RENDER-11` |
| `dom_snapshot` の時間切れ | 30 秒 | 試作 (10) の 8 MiB で 13.9 秒（release・macOS arm64。`outerHTML`）。遅い環境の分を見込んで 2 倍強。TASK-122 の直列化器（番号付き）では、7.6 MB・11.8 万要素の HTML で 1.0〜1.2 秒、上限超過の打ち切りは 1.1 秒（同じ環境）。`outerHTML` より速いが、遅い環境と要素数の多いページの分を見込んで 30 秒のままにする |
| `act` の時間切れ | 10 秒 | 入力イベントの完了通知を待つだけで、読み込みは待たない |
| `wait` の上限時間 | 最大 60 秒 | spec `AISNAP-18`（`load` の上限と同じ） |
| `fill` の `text` | 8192 バイト | 要求フレーム 64 KiB に収める |
| `select` の `values` | 32 件・各 512 バイト | 同上（通常は封筒を含めて 64 KiB に収まる。制御文字が多く符号化後に 64 KiB を超える要求は、本体が送らずに `invalid_params` にする） |
| `wait` の条件の文字列 | 1024 バイト | spec `AISNAP-18` |
| 読み取り契機の同期の最小間隔 | 1 秒 | 直前の同期から 1 秒未満で、その間に `act`・`wait` が無ければ取り直さない（TASK-125） |

- 時間切れでの破棄（確定条件 6・`AISNAP-17`）: `dom_snapshot` と `act` は、本体が上の時間に 5 秒の余裕を足した期限で待ち、過ぎたらホストを破棄して `session_lost` を返す。ホストは `act` の時間（10 秒）を過ぎたら `timeout` を返し、本体は `act` の `timeout` もホストの破棄と `session_lost` にする（TASK-123・TASK-126）。`dom_snapshot` はホスト側の 30 秒の期限でも `timeout` を返し、本体は同じく破棄する（TASK-122）。破棄の後は自動で読み直さない（§14.2）
- メモリの見積もり（多重のコピー込み。8 MiB・ASCII の最悪の場合）: ホストは、番号の対応（要素の ID。20 万件で約 6.4 MB と `Vec` の分）に加えて、ページの DOM が試作 (10) で約 1.2 GB（8 MiB のページで読み込み前から +1.18 GB）、直列化の間にさらに約 130 MB（JS の文字列〔UTF-16 で 16 MiB〕・Rust の `String` 8 MiB・符号化したフレーム 12 MiB とその途中の確保）。§2.7 の RSS 上限の既定（2 GiB）の内側だが余裕は小さく、上限を超えたらホストは破棄される。本体は、受信フレーム 12 MiB・JSON から取り出した `String` 8 MiB・番号を外した保存用の HTML 8 MiB・core の DOM（倍率は未測定）を同時に持ちうる。core の DOM の倍率と Linux での時間は TASK-138 で測り直す

### 14.7 エラー kind

| kind | 返すもの | 意味 |
| ---- | -------- | ---- |
| `invalid_state` | ホスト | 現在の URL が http / https 以外 |
| `stale` | ホスト | `page` が現在と違う、`snapshot` が最新の取得でない、番号が範囲外、番号の要素が消えた、照合値の不一致、待機中に文書が変わった |
| `not_actionable` | ホスト | §14.4 |
| `too_large` | ホスト | §14.3 の上限超過（切り詰めない） |
| `unsupported` | ホスト | 未対応のメソッド、新しいウィンドウ・ファイル選択・ダウンロード |
| `timeout` | ホスト | `wait`、`dom_snapshot`・`act` の時間切れ（`dom_snapshot`・`act` では本体がホストを破棄して `session_lost` にする） |
| `session_lost`・`busy` | 本体（render） | ホストの破棄・再起動の後、同時実行の超過 |
| `not_interactive`・`rendering_disabled`・`interactive_unavailable` | 本体（ai） | `AISNAP-21` |
| `stale`・`sync_failed`（409・502） | 本体（ai） | `/ai/snapshot` の同期の失敗。`stale` は同期の途中で新しい navigate が始まった、`sync_failed` はそれ以外の取得・検査の失敗。`stale` 以外では確定結果を無効にする（`AISNAP-20`。2026-11-19 のオーナー判断・TASK-125） |

- ホストが返す kind はスキーマの `Error.kind` に足した。本体側の kind はホストとの間を流れないため、スキーマに載せない
- エラーの `message` は固定の英語で、入力値・URL・セレクタを含めない（§2.2 と同じ）

### 14.8 契約 fixture

- 操作モードの fixture は `crates/fandhe-browser-render/tests/fixtures/protocol/interactive/` に置く（32 件。各メソッドの正常・不正な要求、結果、`--interactive` の `hello`、ホストが返す新しい kind）。`tests/protocol_fixtures.rs` がスキーマとの整合と網羅を検証する
- ホスト側（`servo-host/src/server/tests.rs`）とクライアント側（`tests/client_contract.rs`）の契約テストは直下の fixture だけを読む。`interactive/` は別のテストに配線した: ホスト側は TASK-122 で `dom_snapshot` の要求（`interactive_dom_snapshot_fixtures_match_host_behavior`）、クライアント側は TASK-123 で正常な要求の JSON・応答・hello・新しい kind（`render10_interactive_requests_match_fixtures`・`render11_interactive_responses_match_fixtures`）。直下への移動と `expected_response` は、両側の既存の契約テストが全件を読むため本体側の結線と合わせて行う（#1592 の残り）。TASK-126 では、ホスト側の `interactive_request_fixtures_match_host_behavior` を `act`・`wait` の要求に広げ、操作モードの hello を `interactive_hello_matches_fixture` で照合した。

### 14.9 残る判断

- WebDriver の命令（§14.4）を使うための `servo-base =0.7.0` の直接依存: 2026-11-19 のオーナー判断で承認し、TASK-126 で採った
- 8 MiB 級の DOM の Linux での時間・本体のメモリ: TASK-138 で測り、§14.6 の値を見直す
- ページのダイアログ（`alert()`・`confirm()`・`prompt()`）の扱い: Servo の既定のままにした（TASK-126・2026-11-19。§14.4）
- `pushState` を繰り返すページでは `act` が毎回 `stale` になりうる。本体が取り直して再試行する回数の上限を TASK-124・TASK-126 で決める
- `window.webdriverElement` は Servo が「公開すべきでないが当面公開している」と注記している拡張で、版の更新で消えうる。消えたら要素の同一性（§14.4）を WebDriver の命令（`servo-base` の依存が要る）で取り直す。servo の版を上げるときに確かめる

## 付録 A. 依存追加の承認提示雛形

[dependency-policy](../../.claude/rules/dependency-policy.md) に従い、実装 Issue で次を提示する。

1. クレート名・版（`=x.y.z`）・目的・配置する crate
2. ライセンス（[licensing](../../.claude/rules/licensing.md) の許可範囲との関係）
3. メンテナンス状況（最終リリース日・活動状況）
4. 推移的依存の概要（C/C++ ビルドの有無・3 OS でのビルド可否）
5. バイナリサイズへの影響
