# Servo ホスト（別 workspace・別プロセス）の構成とプロセス間通信の設計

対応 `TASK-33` / `TASK-34` / `TASK-38` / `MS-4` / ビヘイビア `RENDER-1`〜`RENDER-4` / `OSS-2` / `OSS-3` / `PLUG-1` / `SEC-2`（[spec-reference](../../.claude/rules/spec-reference.md)）。

- 関連 Issue: #935（本設計）・#915（構成・採用版・ライセンス判断。本書は `Refs` で参照し、閉じない）・#914（試作）・#230〜#237（TASK-38 の実装 Issue）
- 状態: **確定**（オーナー判断 2026-10-15・#228）。実装は TASK-38 の issue（#228 配下）で進行中
- 範囲: 本書の作成時点では設計文書のみ。実装・依存追加・既存 Issue の本文書き換え・追加起票・ruleset 変更は行わない（設計レビュー後に別途行う）
- 数値の出典: #914 の試作（[servo-out-of-process-prototype.md](./servo-out-of-process-prototype.md)）と #915 の記録。計測は Linux x86_64 のみで、macOS・Windows・実ページ・musl / scratch 配布・IPC 実装は**未検証**
- spec の確認元: `docs/spec` の記録コミット f7cb79e（`RENDER-1`〜`RENDER-3`・`OSS-2`・`OSS-3` 注記、`TASK-33`・`TASK-34`・`TASK-38`）

## 0. 決定済み事項と本書の推奨

| 区分 | 内容 | 出典 |
| ---- | ---- | ---- |
| 決定済み | Servo は本体 workspace に入れず、別 workspace・別 `Cargo.lock` の子プロセス（Servo ホスト）とする | #915（2026-10-12） |
| 決定済み | 採用版は `servo =0.6.0` | #915 |
| 決定済み | Servo ホスト側 `deny.toml` だけで `MPL-2.0`・`MPL-2.0 AND BSD-3-Clause` を許可し、`CDLA-Permissive-2.0` は `webpki-roots` 1 件に限る。本体の `deny.toml` は変えない | #915・[licensing](../../.claude/rules/licensing.md) |
| 決定済み | FreeType は FTL を選び、`NOTICE` に帰属表示を載せる | #915 |
| 決定済み | 配置は `servo-host/`、通信は stdio + 長さ接頭辞 JSON | 本書 §1・§2（実装済み: #1005・#1021） |
| 決定済み | feature `rendering` を廃止し、設定でホストパスを与えたときだけ起動する | オーナー判断 2026-10-15（#228。本書 §3） |
| 決定済み | v1 は URL のみをホストへ送り、ホストが自分で取得する | オーナー判断 2026-10-15（#228。本書 §3） |
| 決定済み | core の `Renderer` にページ指定メソッドを追加する | オーナー判断 2026-10-15（#228。本書 §3） |
| 決定済み | CI は明示ジョブ 3 本を追加する（ジョブ 16 → 19、required 合計 20 → 23）。ruleset はジョブ追加 PR のマージ時に更新する | オーナー判断 2026-10-15（#228。本書 §6） |
| 決定済み | ホストのビルド cache は ubuntu のみ | オーナー判断 2026-10-15（#228。本書 §6.3） |
| 保留 | egress 方針（Servo 内部通信の制限）は GA 前に判断する | #1003（本書 §2.5） |
| 保留 | 配布方式は spec の整理（`TASK-64`・`CTR-4`）後に起票する | 本書 §8.2・§10 |

オーナー判断の結果は §13 に記録する。

## 1. Servo ホスト workspace の配置

### 1.1 構成

```text
servo-host/
├── Cargo.toml     # 自前の [workspace] を持つ単一 package（fandhe-browser-servo-host）
├── Cargo.lock     # 本体のものとは独立
├── deny.toml      # ホスト専用のライセンス・advisories 検査
├── src/           # main.rs・server.rs（stdio ループ）・engine.rs・page.rs・screenshot.rs・layout.rs
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

- `servo =0.6.0`、`default-features = false`、`features = ["baked-in-resources"]`（試作で起動に必須と確認済み）。
- 試作で直接必要になった `dpi` / `url` / PNG エンコード用の画像 crate 等は、Servo の `Cargo.lock` で解決される版に合わせて `=x.y.z` 固定する。
- これらの追加は本書の範囲では行わず、実装 Issue で承認事項として提示する（[dependency-policy](../../.claude/rules/dependency-policy.md)）。承認提示項目の雛形は付録 A。

### 1.5 ライセンス

- ホスト独自コードは `MIT OR Apache-2.0`。Servo のソースは無改変とし、改変が必要になったらユーザーに判断を仰ぐ（[licensing](../../.claude/rules/licensing.md)）。

## 2. プロセス間のやりとり

### 2.1 トランスポートとフレーミング

- トランスポートは stdio。ローカル TCP は外部公開リスクがあるため不採用（`PLUG-1`）。UDS / 名前付きパイプは大容量転送が必要になった場合の拡張候補とする。
- フレームは「`u32` リトルエンディアンの長さ接頭辞 + UTF-8 JSON 1 メッセージ」。js ワーカー（`crates/fandhe-browser-js/src/worker_protocol.rs`）と同流儀で、長さは確保前に上限検証する。JSON Lines は行長上限の検証がフレーム長より難しいため採らない。
- stdout 汚染対策: ホストは自コードから stdout に書かず、ログは stderr に出す。想定外のバイト列は不正な長さとして検出し、プロトコル違反として扱う。fd の付け替えは `unsafe` が要るため採らない。

### 2.2 メッセージ

- 要求: `{"v":1,"id":<u64>,"method":"...","params":{...}}`
- 応答: `{"v":1,"id":...,"result":{...}}` または `{"v":1,"id":...,"error":{"kind":"...","message":"..."}}`
- 要求の `id` が有効な `RequestId`（0 以上 2^53-1 以下の整数）として取り出せない場合（欠落・負数・文字列・範囲外）と、JSON として解釈できない場合、ホストは応答を返さずプロトコル違反として接続を閉じる（対応付けできない無関係な `id` の応答は返さない。fixture `request-id-*`・`request-truncated-json`）。`id` が有効なら、`v` 不一致・未知フィールド等も `invalid_request` を同じ `id` で返す
- `params` を省略した要求は空オブジェクトとして各 `Params` に照合する（必須項目を持つ `load`・`visibility`・`bounding_box` は `invalid_params`、`screenshot`・`shutdown` は許可）
- 応答は `result` か `error` のどちらか一方だけで、反対側のキーは `null` でも持たない
- 起動直後にホストが `hello` イベントを最初に送る。内容は `host_version`・`servo_version`・`protocol{min,max}`・`capabilities[]`・`limits{...}`。クライアントは `protocol` の互換性を検証し、不一致は接続を閉じて `ProtocolMismatch` 相当のエラーにする。

| メソッド | params | result |
| -------- | ------ | ------ |
| `load` | `url`, `timeout_ms` | `final_url`, `status?`（Servo 0.6.0 では省略） |
| `screenshot` | `full_page` | `format:"png"`, `width`, `height`, `data_base64` |
| `visibility` | `selector` | `visible`, `reason?` |
| `bounding_box` | `selector` | `x`, `y`, `width`, `height` |
| `shutdown` | なし | なし |

- 戻り値は将来拡張できる構造型（`#[non_exhaustive]`。`REPAIR-4`）。未知メソッドは `unsupported` エラー、応答側の未知フィールドはクライアントが無視する。
- 契約は `docs/design/render-host-protocol.schema.json`（`host-api.schema.json` と同流儀。作成は §10 の P1）で定義する。

### 2.3 識別面を変える API は載せない（`SEC-2`）

- `user_agent`・ヘッダ・Cookie・locale・タイムゾーン・TLS・proxy・画面サイズのパラメータを持たない。
- ビューポートは v1 では固定値（例 1280×800）とし API 化しない。画面サイズは識別面に当たりうるため、必要になったら [anti-bot-review](./anti-bot-review.md) の判断を経る。
- 起動引数は固定の許可リストのみで、クライアントからの任意引数は渡さない。未実装メソッドは成功を装わず `unsupported` を返す。

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

- バイナリは v1 では base64 inline で返す。一時ファイルパス返却はパス検証・TOCTOU・シンボリックリンクのリスクがあるため v1 では不採用で、サイズ実測後に再検討する。

### 2.5 URL・セレクタの検証

- URL はクライアントで送信前に検証し、ホストでも再検証する（二重化）。scheme は `http` / `https` のみ。`file:`・`data:`・`javascript:`・`about:` と userinfo は拒否。ループバック・プライベート IP・`localhost` は core の fetch と同じポリシー（`allow_private_network_access`、既定 false）に従う。
- **残存リスク**: Servo 内部のリダイレクト・サブリソース・DNS リバインディングはクライアント検証だけでは防げない。対策候補は egress ガード（`harness/render-screenshot/egress_guard.py` の方式）、OS ネットワーク隔離、core が取得済みの HTML を渡す方式。いずれも**未検証**で、GA 前のオーナー判断事項（§13）とする。
- 可視性・境界ボックスの取得に JS 評価を使う場合、セレクタは JSON エスケープした文字列リテラルとしてのみ埋め込み、連結しない。長さと文字種も検証する。取得手段（埋込 API のレイアウト取得か JS 評価か）は**未検証**で、実装 Issue の先頭ステップで確認する。

### 2.6 起動

- バイナリ解決の優先順位: 1. 設定（`render.host_path`）、2. 環境変数 `FANDHE_BROWSER_RENDER_HOST`、3. 本体実行ファイルと同じディレクトリの `fandhe-browser-servo-host[.exe]`。PATH 探索はしない。相対パスは拒否し、絶対パスのみ許可する。`canonicalize` 後に通常ファイルであることを確認し、Unix では他者書き込み可を拒否する。Windows の ACL 検査と署名・ハッシュ固定は v1 の範囲外。
- 未設定なら起動せず、`DisabledRenderer` 相当の「レンダリング層が無効」を返す。
- シェルを介さず引数配列で起動し、`env_clear` のうえ許可リストで環境変数を渡す（許可リストは Linux の `HOME`・fontconfig 関連、Windows の `SystemRoot` 等を実機で確認してから確定する。**未検証**）。
- 作業ディレクトリと Servo のユーザーデータは専用の一時ディレクトリ（0700、終了時に削除）で、永続しない。プロファイルごとに別のホストプロセスを使い、共有しない（PROF 境界）。
- stderr は末尾 8 KiB を上限付きでバッファし、失敗時のログに使う。URL のクエリや機密をログに出さない。
- **実装（TASK-38・#994・`fandhe_browser_render::host_process`）で決めた値**:
  - 起動引数は固定の `--stdio` のみ（`HOST_ARGS`）。ホスト側のサーバーモード（#993）はこの引数で stdio モードに入る。クライアントから任意引数は渡さない。
  - 環境変数は `env_clear` 後に、workdir 配下を指す強制設定（Unix: `HOME`・`TMPDIR`、Linux はさらに `XDG_CONFIG_HOME`・`XDG_CACHE_HOME`・`XDG_DATA_HOME`。Windows: `TMP`・`TEMP`）と、親に値があるときだけ引き継ぐ変数（Linux: `FONTCONFIG_FILE`・`FONTCONFIG_PATH`、Windows: `SystemRoot`、macOS: なし）だけを渡す。`PATH`・`LANG`・`LC_*`・`TZ`・`*_PROXY`（識別面。SEC-2）、`LD_*`・`DYLD_*`（注入経路）は渡さない。実機確認の状態は**すべて未確認**（Windows の `SystemRoot` は js 子プロセスでは実績あり・Servo ホストでは未確認）。HOME を一時ディレクトリにした場合の fontconfig キャッシュ再生成コストも未確認。
  - タイムアウトは handshake 10 s・load 既定 30 s（上限 60 s）・screenshot 20 s・visibility / bounding_box 5 s・shutdown 猶予 3 s。load はホスト自身の `timeout` 応答が先に届くよう、待ち時間に暫定 2 s を上乗せする。タイムアウト時は kill してホストを破棄する。

### 2.7 資源制限・クラッシュ・ライフサイクル

- 親側の RSS 監視と kill は js の `read_child_rss_bytes` 方式（`process_engine.rs`・`resource_limits.rs`）を踏襲する。子側の `RLIMIT_AS` は SpiderMonkey の仮想アドレス予約と衝突しうるため使わない（**未検証**）。Windows の Job Object は js の承認済み範囲を流用できるか実装時に確認し、新たな `unsafe` は承認事項とする。
- クラッシュ・タイムアウト・プロトコル違反は、当該呼び出しを `RenderError`（§3 の新 variant）で失敗させ、成功を装わない。ホストは破棄し、次回呼び出しで再起動する。サーキットブレーカーとして 60 秒内に 3 回失敗したら一定時間再起動しない。
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

設定でホストパスが与えられたとき、`HostRenderer` を `AppState::new` に注入する。未設定は `with_disabled_renderer`。起動時にホストを自動探索して起動しない。

## 4. Servo ホスト側 `deny.toml`

- `servo-host/deny.toml` の `[licenses] allow` に、本体と同じ許可集合に加えて `MPL-2.0` を足す（`MPL-2.0 AND BSD-3-Clause` は `BSD-3-Clause` が既許可のため `MPL-2.0` だけで足りる）。
- `CDLA-Permissive-2.0` は `[[licenses.exceptions]]` で `crate = "webpki-roots"` に限る（版固定の併記案あり）。許可リストには入れない。
- `MPL-2.0` を crate ごとの例外にしない。83 件が対象で版更新のたびに壊れ、ホスト workspace 全体が MPL の範囲という決定とも合うため。
- 本体の `deny.toml` は変更しない（`OSS-2`・`OSS-3`）。
- `[bans]` に `mozangle`・`gstreamer` 系・`wgpu` 系・LGPL 系を明示し、`default-features = false` の効果の回帰を検出する。`[sources]` は crates.io のみとし git ソースを拒否する。`mozangle` は `wrappers = ["servo"]` で、Windows の `no-wgl`（ANGLE）に限り servo からの直接依存だけ許す（#999）。
- `[graph]` は `all-features = false`、`targets` は 3 OS の主要ターゲットに限定する（aarch64-windows / musl は検証待ち）。
- 試作の `cargo deny` は拒否 85 件（`MPL-2.0` 83・`MPL-2.0 AND BSD-3-Clause` 1・`CDLA-Permissive-2.0` 1）。FreeType 本体（FTL）と同梱 libpng は `cargo deny` で検出できないため手動確認項目とする（§5）。

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

- `NOTICE` §2 の MPL 適用範囲を「Servo ホストの配布物（`fandhe-browser-servo-host` バイナリとそれを含むコンテナイメージ）」に書き換える。本体バイナリには含まれない。Servo 版は `0.6.0`、ソース入手先は `https://github.com/servo/servo` の対応タグ、ソースは無改変と明記する。MPL-2.0 全文を `LICENSE-THIRD-PARTY` に追加する。
- `NOTICE` §1 の CA バンドルの注記は「MPL-2.0 は `fandhe-browser-render` 配下に限る規約の例外」から「Servo ホスト workspace 外の例外（データファイル）」へ言い換える。
- FreeType は FTL を選び、`NOTICE` に FTL の帰属文（同梱ソースの `docs/FTL.TXT` の文言に一致させる）を載せる。GPLv2 側は選ばない旨を明記する。
- 同梱 libpng は PNG Reference Library License v2 の著作権表示・許諾文を `LICENSE-THIRD-PARTY` に収録する。
- Cargo 依存約 569 件分の著作権表示のため、`scripts/gen-cargo-licenses.sh` を manifest パスを受け取れる形に拡張し、ホスト配布物用の `LICENSE-CARGO-DEPENDENCIES` を別に生成する。`webpki-roots`（CDLA-Permissive-2.0）の帰属を含める。
- `baked-in-resources`（`servo-default-resources`）に埋め込まれるリソースファイルのライセンスを手動確認する。
- FreeType は `bundled_freetype` でも `libfreetype` へ動的リンクされる（試作の `ldd` で 13 ライブラリ）。同梱版と system 版のどちらが使われるかは**未検証**で、配布形態で `NOTICE` の内容が変わるため §7 の配布検証と対にする。
- ライセンス判断が要る事項は Agent が決めず、オーナーへ報告する。

### 5.1 `servo-default-resources` 埋め込みリソースの手動確認（TASK-38・Issue #996）

`servo-default-resources` 0.6.0（crate の SPDX は MPL-2.0、上流タグ `v0.6.0` の `components/default-resources`）が `include_bytes!` で埋め込む 11 ファイルを確認した。ファイル個別のライセンスヘッダは `gatt_blocklist.txt` のみにあり、他は crate の SPDX（MPL-2.0）に従う。上流に個別の出所表記が無いものは推測で埋めず「未確認」とする。

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
| musl 静的 / scratch 配布 | musl ターゲットでビルドし `ldd` / `check-static-link.sh` 相当で確認 | 動的リンク 0 | glibc 系ベースの別コンテナイメージを代替案とする |
| FreeType の同梱 / system 判別 | `nm` / `ldd` / ビルドログでシンボル出所を確認 | 配布物で出所が一意 | `NOTICE` を配布形態別にする |
| 最小 system 依存 | `make`・Python・`uv`・libclang の個別欠如試験 | 必須ツールの一覧が確定 | `docs/setup/` に前提を追記 |
| IPC 実装・負荷 | Servo なしの偽ホストで再起動・タイムアウト・巨大フレーム・不正 JSON を試験 | 各ケースで成功を装わず失敗する | 上限値を調整 |
| メモリ・リソース制限 | 実ページの RSS 計測（`TASK-82` と連携） | 親側 RSS 監視が効く | OS 制限の追加を検討（承認事項） |

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

配布（ホスト用 release ビルド・コンテナイメージ）は `TASK-64` 系との関係を spec 側で整理してから別途起票する。

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

依存追加を伴う H1 とホスト側 PNG エンコードは、dependency-policy の承認を得るまで着手しない。ruleset 更新・egress 方針・配布方式は owner:human として分ける。

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
| コンテナ（`CTR-4`） | Servo ホストの配布物をコンテナで扱うか。scratch + musl の前提とホストの動的リンクが衝突しうる |
| `OSS-2` / `OSS-3` | 注記の整合（ホスト workspace 限定の MPL 許可） |

## 11. セキュリティ考慮（OWASP Top 10 観点）

| 観点 | 対応 |
| ---- | ---- |
| インジェクション | URL・セレクタの検証、セレクタを JS に連結しない、引数配列で起動、シェル不使用 |
| 不安全な設計 | フレーム・URL・セレクタの上限、ページ読み込みのタイムアウト、無制限確保の防止、資源制限、サーキットブレーカー |
| アクセス制御の不備 | ローカル TCP 不採用でポートを開かない、バイナリ解決は絶対パスのみ・PATH 探索なし、他者書き込み可のバイナリを拒否 |
| SSRF | scheme 制限、プライベート IP ポリシーを core と揃える。Servo 内部のリダイレクト・サブリソースは残存リスクで GA 前の判断事項 |
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

## 13. オーナー判断（2026-10-15）

出典は #228。各項目の結果を記す。

1. feature `rendering` を廃止し、設定でホストパスを与えたときだけホストを起動する（§3.2）: 決定
2. v1 は URL のみ送り、ホストが自分で取得する。Cookie は共有しない（§3.4）: 決定
3. core `Renderer` に描画対象ページを指定するメソッドを追加する（§3.3）: 決定
4. ホストのビルド cache は ubuntu のみ（§6.3）: 決定
5. `ci.yml` のジョブは 16 → 19、required の合計は 20 → 23。ruleset はジョブ追加 PR のマージ時に更新する（§6.2）: 決定
6. egress 方針（Servo 内部のリダイレクト・サブリソース・DNS リバインディング。§2.5）: GA 前に判断する（#1003）
7. 配布方式（ホスト単体 release・コンテナ。§8.2・§10）: spec の整理（`TASK-64`・`CTR-4`）後に起票する

## 付録 A. 依存追加の承認提示雛形

[dependency-policy](../../.claude/rules/dependency-policy.md) に従い、実装 Issue で次を提示する。

1. クレート名・版（`=x.y.z`）・目的・配置する crate
2. ライセンス（[licensing](../../.claude/rules/licensing.md) の許可範囲との関係）
3. メンテナンス状況（最終リリース日・活動状況）
4. 推移的依存の概要（C/C++ ビルドの有無・3 OS でのビルド可否）
5. バイナリサイズへの影響
