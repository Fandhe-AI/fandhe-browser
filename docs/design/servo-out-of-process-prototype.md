# Servo 別 workspace・別プロセス構成の試作と計測

対応 `TASK-38`（前段の試作）/ `MS-4` / ビヘイビア `RENDER-1` / `RENDER-2` / `RENDER-3`（[spec-reference](../../.claude/rules/spec-reference.md)）。

- 実施日: 2026-10-12
- 関連 Issue: #914（本試作）・#915（構成・採用版・ライセンス可否の人間判断）・#229（採用条件）・#230（TASK-38.1 の依存解決競合）
- 状態: 試作・計測の記録（2026-10-12）。採用・版は #915 で確定し、構成とプロセス間通信の設計は [servo-host-design.md](./servo-host-design.md)（#935、2026-10-15 に確定）を正とする。本書の数値は設計の出典として残す
- 担当: Claude Code（計測）。採否・ライセンス判断は人間担当

## 目的と範囲

Issue #230 では、Servo 0.6.0 / 0.7.0 の `mozjs_sys` が `icu_capi =2.1.2` に固定されるため、本体の `v8 =152.2.0`・`boa_engine =0.22.0` と同じ `Cargo.lock` に共存できない。
オーナー判断（2026-10-12）により、Servo を別 workspace・別 `Cargo.lock` の子プロセスとして使う構成を第一候補とする。
`RENDER-1` は「同一バイナリの feature gate」を前提にしているため、spec を変更する前に実現性を試作で確認する。

本書は #915 の判断材料を揃えるための記録で、次を範囲外とする。

- 3 OS でのビルド確認（Linux x86_64 のみ。macOS・Windows は**未検証**。#229 条件 5 の Windows 既知問題も対象外）
- Servo の採用、版の確定、`deny.toml` の許可追加、spec の編集
- スクリーンショット取得などの実機能（`TASK-36`・#50 の領域）とランタイム性能・メモリ増分（`TASK-82`）

本体の `Cargo.toml`・`Cargo.lock`・`deny.toml`・`crates/` は一切変更していない。試作はリポジトリ外の一時ディレクトリで行い、作業後に削除した。

## 試作の構成

### 環境

| 項目 | 値 |
| ---- | -- |
| OS | Ubuntu 系 Linux（カーネル 7.0）x86_64 |
| CPU / メモリ | 10 論理コア / 27GB |
| Rust | 1.99.0（`rust-toolchain.toml` は本体と同一の stable） |
| 補助ツール | GNU Make 4.4.1・Python 3.14.4・clang 21.1.8・cmake 4.2.3・uv 0.12.19・pkg-config 2.5.1・gcc 15.2.0 |
| システムライブラリ | FreeType 26.5.20（pkg-config）・fontconfig 2.17.1（開発ヘッダ導入済み） |

### workspace

本体とは独立した `[workspace]`（`members = ["servo-host"]`）と専用の `Cargo.lock`・`target/` を持つ。
`[profile.release]` は本体と同じ `panic = "abort"`・`opt-level = "z"`・`lto = true`・`strip = true`。

```toml
# servo-host/Cargo.toml（抜粋。最終構成）
[dependencies]
servo = { version = "=0.7.0", default-features = false, features = ["baked-in-resources"] }
dpi = "=0.1.2"   # SoftwareRenderingContext::new の引数型。servo が再エクスポートしないため直接指定
url = "=2.5.8"   # WebViewBuilder::url の引数型
```

バイナリは 2 変種を用意した。

| 変種 | 内容 | 位置づけ |
| ---- | ---- | -------- |
| A（最小） | `println!` のみ。Servo は参照されずリンカが除去する | 参考値。PoC-6 と同じく **Servo は未リンク**で、サイズ・時間の主計測にしない |
| B（主計測） | `ServoBuilder` → `SoftwareRenderingContext` → `WebViewBuilder` → `WebView` を実際に参照。`--selftest` 指定時のみ `data:` URL を読み込み `LoadStatus::Complete` まで `spin_event_loop` を回す | 実リンクを伴う計測対象。ネットワークへは出ない |

以降の数値はすべて変種 B。ビルドは `cargo build --release --locked`。

## 計測結果

### 依存解決

| 項目 | 結果 |
| ---- | ---- |
| `servo =0.7.0`（`default-features = false`）の単独解決 | 成功。`cargo generate-lockfile` は約 4 秒 |
| 解決された `icu_capi` / `icu_calendar` | `2.1.2` / `2.1.1`（本体の `v8` は `icu_calendar ^2.2.1`、`boa_engine` は `~2.3.0` を要求するため、同一 `Cargo.lock` とは非共存。**別 workspace の裏付け**） |
| `mozjs_sys` | `153.3.0-1` |
| 依存クレート数（`cargo tree -e normal,build` の重複除去後） | 569（`baked-in-resources` で `servo-default-resources` が 1 件増える） |

### ビルド

| 構成 | 成否 | 所要時間（壁時計） | 備考 |
| ---- | ---- | ------------------ | ---- |
| コールド（`default-features = false`） | 成功 | 8 分 28 秒 | 最大 RSS 約 4.4GiB。依存の事前ダウンロード（`cargo fetch`）は含まない |
| 差分なし再ビルド | 成功 | 1 秒 | |
| ソース 1 行変更後 | 成功 | 3 分 11 秒 | `lto = true` の再リンクが支配的と推定（プロファイル別の切り分けは未実施） |
| `+baked-in-resources` | 成功 | 3 分 32 秒 | 追加クレート `servo-default-resources`（MPL-2.0）。再ビルドは差分のみ |
| `+baked-in-resources +bundled_freetype` | 成功 | 7 分 9 秒 | FreeType・libpng をソースから同梱ビルド |

- 前提ツール: `mozjs_sys` の `build.rs` は `make`（`MAKE` 環境変数または `find_make`）と Python（`PYTHON`）を、`servo-script-bindings` の `build.rs` は `uv run python`（フォールバック `python3`・`python`）を呼ぶ。bindgen が libclang を使う。上表の環境はこれらすべて導入済みで、**個別に欠如させた試験は未実施**（必須かどうかの厳密な切り分けは未検証）
- `target/` 総サイズは約 7.8GB（コールド直後）〜8.6GB（全構成のビルド後）。PoC-6（macOS・Servo 0.3.0）の約 2.4GB は `Servo` 未リンクの Hello World 時点の値で、実リンク後の値ではない

### 成果物サイズ・動的リンク・起動可否

| 構成 | サイズ（バイト） | 起動確認 |
| ---- | ---------------- | -------- |
| 変種 A（Servo 未リンク。参考） | 424KB 水準（PoC-6 の値。本試作では未計測） | 該当なし |
| `default-features = false` | 65,564,392（約 62.5MiB。strip 済み） | **起動不能**。`--selftest` で `No resource reader registered` の panic（`resources.rs`）。リソースを埋め込まないと実行時にリソースリーダー未登録で abort する |
| `+baked-in-resources` | 67,673,576（約 64.5MiB） | **成功**。`data:` URL の `LoadStatus::Complete` に到達（0.05 秒・最大 RSS 約 124MiB。極小ページの 1 回計測で、性能指標としては扱わない） |
| `+baked-in-resources +bundled_freetype` | 68,323,464（約 65.2MiB） | 成功（同上） |

- 最小の実用構成は `default-features = false` + `baked-in-resources`。`baked-in-resources` を足さないと、ビルドは通るが起動できない
- 既定プロファイル相当（strip なし・opt-level 3）のサイズは**未計測**
- `ldd` の結果（全構成で同一の 13 ライブラリ）: `libstdc++`・`libz`・`libfontconfig`・`libgcc_s`・`libm`・`libc`・`libfreetype`・`libexpat`・`libbz2`・`libpng16`・`libbrotlidec`・`libbrotlicommon`・`ld-linux`
  - **`bundled_freetype` を有効にしても `libfreetype.so.6` へ動的リンクされる**。`yeslogic-fontconfig-sys` が system の fontconfig → FreeType を引くためと推定されるが、同梱版と system 版のどちらのシンボルが使われているかは**未検証**
  - 本体の「動的リンクなし」配布（musl 静的・scratch。`scripts/check-static-link.sh`）の対象は本体バイナリで、Servo ホストは別アーティファクトになるため直接は衝突しない見込み。ただし Servo ホスト自体を musl 静的・scratch に載せられるかは**未検証**（システムの fontconfig / FreeType が必要になる）
- PoC-6 との比較: `servo-embed`（Servo 0.3.0、macOS arm64、strip 未設定）は 136,414,768 バイト。本試作は版・OS・プロファイルが異なり、直接比較できない（`strip = true`・`opt-level = "z"`・`lto = true`・Linux で約 65〜68MB）

### 依存ツリー検査

`cargo tree --locked -e normal,build` の結果に対し、パターン `webgl|webgpu|mozangle|gstreamer|gaol|lgpl|bluetooth|webxr|surfman|freetype|fontconfig` を大文字小文字非区別で検査した。

| 観点 | 結果 |
| ---- | ---- |
| `webgl`・`webgpu`・`mozangle`・`gstreamer`・`bluetooth` | **0 件**（#229 条件 1 を満たす） |
| LGPL / GPL 系の crate 名 | 0 件 |
| `servo-webxr-api` | 1 件。`webxr` 実装 crate ではなく API 型を持つ crate で、`servo-canvas-traits` 等から常時引かれる |
| `surfman` 0.14.0 | `servo`・`servo-paint`・`servo-paint-api` が常時依存（OpenGL コンテキスト管理）。`mozangle` は引かない |
| `gaol` 0.2.1 | `servo`・`servo-constellation` が依存。`multiprocess` を無効にしても依存グラフには残る（サンドボックス crate） |
| `freetype-sys` 0.23.0 / `yeslogic-fontconfig-sys` 6.0.1 | 常時依存。`freetype-sys` は `links = "freetype"` |

## ライセンス検査（`cargo deny`）

現行 `deny.toml` を試作 workspace へそのままコピーし、`cargo deny --locked check licenses` を最終構成（`+baked-in-resources +bundled_freetype`）の `Cargo.lock` に当てた。
終了コード 4（失敗）。

- `[graph] all-features = true` は workspace メンバー（`servo-host`）にだけ効く。Servo の optional 依存は依存先の feature として解決されるため、`all-features = false` に変えた設定でも結果は同一だった（拒否 crate は同じ集合）
- 拒否された crate: 計 85。内訳は次のとおり

| ライセンス式 | crate 数 | 代表例 | 現行 `allow` に不足する識別子 |
| ------------ | -------- | ------ | ------------------------------ |
| `MPL-2.0` | 83 | `servo`・`servo-*` 系・`mozjs`・`mozjs_sys`・`mozjs_*_glue`・`selectors`・`cssparser`・`app_units`・`cbindgen`・`dwrote`・`plane-split`・`servo-default-resources` | `MPL-2.0` |
| `MPL-2.0 AND BSD-3-Clause` | 1 | `servo-script-bindings` | `MPL-2.0`（`BSD-3-Clause` は許可済み） |
| `CDLA-Permissive-2.0` | 1 | `webpki-roots` 1.0.9（`servo-net` が `hyper-rustls`・`async-tungstenite` 経由で依存） | `CDLA-Permissive-2.0` |

- 不足は `MPL-2.0` と `CDLA-Permissive-2.0` の 2 識別子のみ。`MIT-0`・`OFL-1.1`・`Ubuntu-font-1.0`・`FTL`・`Unicode-DFS-2016` で拒否された crate は**ない**
- `0BSD`・`CC0-1.0`・`MIT-0`・`BSL-1.0` は `OR` の選択肢として現れるだけで、`MIT` / `Apache-2.0` の側で許可される（拒否対象にならない）
- `r-efi` 5.3.0 / 6.0.0 は `MIT OR Apache-2.0 OR LGPL-2.1-or-later`。選択肢に LGPL があるが `OR` で MIT / Apache-2.0 を選べ、UEFI 向けで Linux ビルドでは使われない
- ライセンス未記載の crate は 0 件
- 推移依存のうち GPL / LGPL / AGPL を**単独で**要求する crate は、今回の構成では 0 件

### FreeType のライセンス表記

| 層 | 表記 | 出典 |
| -- | ---- | ---- |
| Rust ラッパー `freetype-sys` 0.23.0 | `MIT`（`Cargo.toml` の `license`・同梱 `LICENSE` は PistonDevelopers の MIT） | `cargo deny` / `cargo metadata` が見るのはこの層のみ |
| 同梱 FreeType 本体（`freetype2/`。`bundled_freetype` 有効時にソースからビルド） | FreeType License（FTL）と GPLv2 の**二者択一**（`LICENSE.TXT`。利用者がどちらか一方を選んで従う） | crate 内の `freetype2/LICENSE.TXT`・`docs/FTL.TXT`・`docs/GPLv2.TXT` |
| 同梱 libpng（`libpng/`） | PNG Reference Library License version 2 | crate 内の `libpng/LICENSE` |
| system FreeType（`bundled_freetype` なしで動的リンク） | 配布物に含まれず system 側のライセンス。配布時はその `libfreetype` を同梱するか利用者環境に依存する | `ldd` と `pkg-config`（26.5.20） |

- `cargo deny` は FreeType 本体の二者択一を検出できない（`MIT` としか見えない）。ソース同梱時の FTL 選択と `NOTICE` への帰属表示は、#229 条件 4 のとおり別途の手動確認と記載が要る
- **ライセンス判断（MPL-2.0・CDLA-Permissive-2.0 の許可、FTL の選択）は Agent が決めない**。事実の列挙にとどめ、#915 に委ねる

## 本体との IPC 案（机上設計・未実装）

### 前提

- 本体（cli）は薄いクライアント（現 `fandhe-browser-render` 相当）を持ち、実体の Servo は別バイナリ `servo-host` の子プロセスとして動かす
- プラグイン境界の方針（`PLUG-1`・[plugin-boundary-policy](./plugin-boundary-policy.md)）と同じく、動的ライブラリの実行時ロードは行わず、プロセス分離は stdio 等で行う
- 描画トレイト（`TASK-33`。cdp・ai は render に直接依存しない）の背後に差し込む。子プロセスが無い・起動に失敗した場合は、`rendering` 無効時の既定実装と同じ応答に縮退する

### トランスポート候補

| 候補 | 3 OS で同一挙動 | 開くポート | 既存方針との整合 | 所見 |
| ---- | ---------------- | ---------- | ---------------- | ---- |
| stdio（長さ接頭辞付き JSON または JSON Lines） | 高い | なし | MCP プラグイン（stdio）と同じ流儀 | **本命案** |
| Unix ドメインソケット / Windows 名前付きパイプ | OS 差異の吸収が必要 | なし | 別実装が 2 系統になる | 大容量転送が要る場合の拡張候補 |
| ローカル TCP | 高い | あり | アクセス制御・公開範囲の管理が増える | 既定では**不採用**（外部インターフェース公開のリスク） |

- バイナリ（PNG 等）の返し方: stdio 内の base64（実装は単純だが約 33% 増）と、子が書いた一時ファイルのパスを返す方式（本体が検証してから読む）を比較する。サイズ上限が既定値で収まるなら前者でよく、決定は実測後
- メッセージ案（要旨のみ）: `hello`（版・能力の自己申告）・`render.screenshot`・`render.layout`（境界ボックス）・`render.visibility`・`shutdown`
  - 戻り値は将来拡張できる構造型にする（`REPAIR-4`）。プロトコル版・エラー種別・タイムアウトを持たせる
  - UA 等の識別面を変更する API は載せない（`SEC-2`）

### 運用・セキュリティ

- 子プロセスの探索: PATH ではなく、設定された絶対パスのみ。シェルを介さず引数配列で起動する。環境変数の引き継ぎは最小にする
- 入力検証: メッセージサイズ・件数の上限、URL の scheme 制限（`file:`・内部アドレスの SSRF を拒否）、タイムアウトと強制終了
- 資源上限: 子プロセスのメモリ・CPU・同時実行数の上限（rlimit 等。本体 js ワーカーの先例を参照）。無制限確保・無限実行を防ぐ
- 信頼境界: 子からの応答も untrusted として検証する。クラッシュ検出と再起動、機密情報をログへ出さない
- 配布: 本体は軽量のまま（`RENDER-2`）、Servo ホストは別アーティファクト（別バイナリ・別コンテナイメージ）。[render-feature-gate](./render-feature-gate.md)「オプトイン配布方式の比較」の「別バイナリ」「別コンテナイメージ」に対応する

## 2 workspace 運用の影響

実装はせず、#915 の後続 Issue 向けに論点を整理する。

| 対象 | 影響 | 必要な変更案 | 未検証点 |
| ---- | ---- | ------------ | -------- |
| `scripts/check-render-isolation.sh`（`RENDER-1`） | 本体 `Cargo.lock` に Servo が入らない構造的保証になり、検査 A・B は維持できる | 試作 workspace が本体 `members`（`crates/*` グロブ）に紛れ込まない検査を足す。置き場所は `crates/` 外（例 `render-host/`）にする案 | `fandhe-browser-render` crate の位置づけ（IPC クライアントのみにするか廃止か）が論点 |
| `deny.toml` / `cargo deny` | 2 workspace それぞれに設定が要る。本体は MPL-2.0 を許可しないまま維持でき、MPL 例外は Servo ホスト側 `deny.toml` に局所化できる | CI `rust-lint` ジョブの拡張、`make check-deny-license-reject` の対象追加 | 許可する識別子の確定（#915） |
| CI（`.claude/rules/ci.md`・[github-actions](../../.claude/rules/github-actions.md)） | Servo ホストのビルドは約 8.5 分（コールド）・target 約 8.6GB。cache 予算（上限 10GB）を圧迫する | 新規ジョブは ruleset 更新が要る。既存ジョブへのステップ追加か、main / release のみの実行かを選ぶ。`scripts/ci-changes.sh` の allowlist も更新 | 3 OS ビルド時間（macOS・Windows は未計測）。非 required の補助ジョブを置けない制約との整合 |
| feature 分離検証（`rust-features-*`・`js-engine-isolation`） | 本体側は変更不要の見込み | なし | 未確認 |
| `harness/binary-size` | 本体バイナリに Servo が入らないため `RENDER-2` の上限は維持される | Servo ホスト側のサイズ上限は新設の論点（試作値は約 65〜68MiB） | 3 OS・既定プロファイル相当の値 |
| `Dockerfile` / `docker/`・`NOTICE`・`scripts/gen-cargo-licenses.sh` | ホスト用イメージ・帰属表示が別に要る。`gen-cargo-licenses.sh` の 2 workspace 対応の要否 | 同上 | FTL 選択・MPL 適用範囲の記載（人間判断） |
| 依存更新運用（`update-external.yml` ほか） | Servo ホスト側の依存更新ルールが別に要る | 方針策定 | 未検討 |
| spec 側の見直し | 本リポからは編集しない | `RENDER-1`（前提の feature gate・検証方法）・`RENDER-3`（測り方）・`TASK-33`・`TASK-34`・`TASK-38`（成果物パス） | spec 側の変更が必要。#915 に報告 |

## 受け入れ条件との対応

| 条件 | 状態 | 根拠 |
| ---- | ---- | ---- |
| 別 workspace で `servo =0.7.0`（`default-features = false`）の最小バイナリが Linux でビルドできるかの記録 | 達成 | 成否・時間・システム依存（上記）。ただし起動には `baked-in-resources` が要る |
| ビルドできた場合のサイズ・`cargo tree` に webgl・webgpu・mozangle・LGPL 系が入らないこと | 達成 | 約 65.6〜68.3MB。該当 0 件 |
| `cargo deny check licenses` の結果（拒否一覧・FreeType 表記） | 達成 | 拒否 85 件・不足 2 識別子・FreeType の層別表記 |
| 本体との stdio 等の案、2 workspace の影響整理 | 達成（机上） | IPC 案・影響表 |
| `docs/design/` への文書化と #915 向け材料 | 達成 | 本書 |
| 3 OS でのビルド確認 | 範囲外（未検証） | Linux のみ |

## #915 向け判断材料

### 採否の論点

- 技術面: 別 workspace・別 `Cargo.lock` なら Servo 0.7.0 は単独で解決でき、`default-features = false` + `baked-in-resources` でビルド・極小ページの読み込みまで到達した。webgl・webgpu・mozangle は依存に入らない
- 構成面: 本体の依存グラフに Servo が入らないため、`RENDER-1` の趣旨（既定ビルドを軽量に保つ）は構造的に守れる。一方で spec が前提とする「同一バイナリの feature gate」とは異なり、spec 側の改訂が要る
- ライセンス面: Servo 系は `MPL-2.0` の crate が 83 件に及び、`webpki-roots` の `CDLA-Permissive-2.0` も現行 `allow` に無い。許可の範囲（Servo ホスト側 `deny.toml` に局所化するか）と FreeType の選択は人間判断

### 残るリスク

- `bundled_freetype` を有効にしても `libfreetype` へ動的リンクされ、FreeType の入り方（同梱か system か）が配布物で曖昧になる。静的リンク配布との両立は未検証
- ビルド時間（コールド約 8.5 分・再リンク約 3 分）・target 約 8.6GB は CI の cache 予算と所要時間に影響する
- Servo ホストの最大 RSS は極小ページで約 124MiB。実ページの値は未測定

### 未検証点

- macOS・Windows でのビルドと動作（Windows 既知問題を含む）
- 実ページのスクリーンショット取得（`TASK-36`・#50）、ランタイム性能・メモリ増分（`TASK-82`）
- `make`・Python・`uv`・libclang の個別欠如時の挙動、最小 system 依存の確定
- `lto = true` と `opt-level = "z"` を外した場合のサイズ・ビルド時間
- Servo ホストの musl 静的・scratch 配布
- IPC 案の実装・負荷・クラッシュ復旧

## 再現手順の要点

```bash
# 試作 workspace（リポジトリ外）。servo-host は上記の Cargo.toml と、Servo を参照する main.rs
cargo generate-lockfile
cargo tree --locked -e normal,build --prefix none | sort -u | grep -Ei 'webgl|webgpu|mozangle|gstreamer|lgpl'
cargo build --release --locked
ldd target/release/servo-host
cp <本体リポジトリ>/deny.toml . && cargo deny --locked check licenses
```

## 出典

- `docs/spec/05-tasks.md` TASK-33・TASK-34・TASK-36・TASK-38・TASK-82
- `docs/spec/06-roadmap.md` MS-4
- `docs/spec/04-behavior/rendering-layer.md` RENDER-1〜RENDER-3
- `docs/spec/03-poc/rendering-layer-servo/README.md`（PoC-6 の既存値）
- [render-feature-gate](./render-feature-gate.md)・[plugin-boundary-policy](./plugin-boundary-policy.md)
- Servo 0.7.0 の `servo-embedder-traits`（`resources.rs`）・`freetype-sys` 0.23.0 の同梱 LICENSE（crates.io 取得物の静的確認）
