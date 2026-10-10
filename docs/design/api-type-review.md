# 公開 API 戻り値型の拡張性レビュー

**状態**: 確定（オーナー判断 2026-10-10。推奨どおり採用）  
**対象**: `04-behavior/self-repair-design.md` REPAIR-4（TASK-4・MS-7・Issue #328）  
**調査基準**: `origin/main` 43262c1（呼び出し元数は main 03b8617 の `git grep` による。`#[cfg(test)]` 以降・`tests/`・`benches/`・`examples/` は「テスト等」として本番と分けて数えた）

## オーナー判断（2026-10-10）

PR #817 のアーキテクチャ観点の推奨（問題候補 15 件）どおり採用する。

- 採否: 改修 2（A-1・P-2）・現状維持 11（A-2・A-3・A-4・A-5・A-6・A-7・C-2・C-4・P-1・P-3・P-4）・保留 2（C-1・C-3）
- 非破壊か置換か: 原則は R-4/R-5 の非破壊追加とする。置換は次の 2 条件に限って許容する。(a) 呼び出し元が同一 crate 内で 1 件以下（A-1・C-3）、(b) 既存を残すと契約を黙って破る（C-1）。全 crate が `publish = false` のため、置換の影響は workspace 内のコンパイルエラーで検出できる
- 後続:
  - A-1: 実装 Issue #838（`fandhe-browser-ai`・単独 1 件）
  - P-2: 実装 Issue #839（`fandhe-browser-profile`・単独 1 件）
  - C-1: page WS 実装 Issue（起票時）に内包（`CdpEndpoints::into_parts` の置換を前段サブタスクに含める）
  - C-3: ハンドラ実装 Issue（起票時）に内包（`close_target` の戻り値型を確定する）

## 位置づけ

TASK-4 は、公開 API（検索結果型等）の戻り値型が将来の拡張を見越した構造を持つことを、PoC-10 の教訓（タスク T-4 で発生した「`ElementRef` 直接走査への切替」という追加設計判断。型の表現力不足が改修コストを押し上げた）を踏まえてレビューする人間担当（アーキテクチャ判断）のタスクである。本書は事実の洗い出しと問題候補の抽出を行い、各候補の判断（改修・現状維持・保留）と理由を記録する。

REPAIR-4 の要求は次のとおり（spec が正）。

- 前提: 公開 API（検索結果型等）の戻り値型を設計する
- 期待: 型の表現力不足による改修コスト増大が再発しない構造になっている

### 受け入れ条件（Issue #328）との対応

| 受け入れ条件 | 本書の該当箇所 |
| ------------ | -------------- |
| 主要な公開 API 戻り値型を洗い出し、拡張性の観点でレビューした記録がある | 「洗い出し結果」「観点別サマリー」 |
| 表現力不足が見つかった型の改修方針（型変更または現状維持の判断理由）が記載されている | 「問題候補と判断」（各候補に判断と理由を記載） |
| レビュー結果が本書に残る | 本書 |

### 判断基準（`docs/design/repair-trial-record.md` R-4・R-5 に基づく）

試行用候補 R-4（`FetchResponse::mime_type()` → `#[non_exhaustive]` の `MimeType`）と R-5（`ParseDiagnosticEntry { line, message }`。既存 `messages` は互換のため残す）は、次の方針を示している。

- 真偽値・素の文字列・タプルで返さず、`#[non_exhaustive]` の構造化型（名前付きフィールド・アクセサ）で返す
- 既存の戻り値は互換のため残し、構造化型を追加する形で拡張する

本書の「問題」は、この方針から外れ、かつ将来の拡張で戻り値型の変更（＝呼び出し側の破壊的変更）が起き得るものを指す。非破壊追加の原則からの例外（置換）は「オーナー判断」の節に記載した条件に限る。

## 前提として押さえる事実

- **公開 API は workspace 外へ露出していない**。全 crate が `version = "0.1.0"`・`publish` 非公開（ルート `Cargo.toml` の `[workspace.package]`）。外部（CDP クライアント・AI エージェント・プラグイン）に露出しているのはワイヤ形式（CDP JSON・`/ai/*` の HTTP JSON・MCP stdio）であり、Rust 型の変更は workspace 内のコンパイルエラーに導かれて追従できる
- **依存方向**: `fandhe-browser-core` が `fandhe-browser-profile` と `fandhe-browser-js` に依存する（`crates/fandhe-browser-core/Cargo.toml`）。profile・js は core より下位にある。以下の改修案はいずれも crate 間の依存方向を変えない
- **`#[non_exhaustive]` で非破壊なのはバリアントの追加だけ**。既存の単位バリアントにフィールドを足すのは、パターン `ApiError::Snapshot` が一致しなくなるため破壊的変更である

## 洗い出し方法と範囲

- 対象: `fandhe-browser-ai`・`fandhe-browser-cdp`・`fandhe-browser-profile` の `src/` 配下で `pub fn` として宣言された関数・メソッドの戻り値型（`#[cfg(test)]` 以降は除外）。機械的な抽出に基づくため、`impl Trait` を返す宣言や複数行シグネチャの一部は丸めている
- `fandhe-browser-cdp` の `protocol` モジュールは非公開（`mod protocol`・型は `pub(crate)`）のため、その `pub fn`（9 件）は公開 API の対象外とした。公開面は `lib.rs` の再エクスポートと `pub mod server`・`pub mod target`
- 件数: ai 105 件・cdp 25 件（公開面のみ）・profile 33 件
- 型の宣言（トップレベルの `pub struct` / `pub enum`）の `#[non_exhaustive]` 付与状況: ai 43 付与 / 4 未付与、cdp 6 / 3、profile 4 / 9。未付与は主に状態保持型（レジストリ・アロケータ・ストア）で、フィールド公開の有無までは全数精査していない

## 洗い出し結果

### 観点別サマリー

| 観点 | ai | cdp | profile |
| ---- | -- | --- | ------- |
| `bool` を返す関数 | 4 件（`is_regular`・`is_data_leaf`・`is_empty` 等） | 0 件 | 2 件（`NameRegistry::contains`・`is_empty`） |
| 素の `String` を返す関数 | 1 件（`ElementRef::to_ref_string`） | 0 件（`as_str` 系は `&str`） | 0 件 |
| 素の数値（`u64` 等）を返す関数 | 7 件（ダイジェスト・性能予算） | 2 件（件数） | 0 件 |
| タプルを返す関数 | 1 件（`rows_to_fold` の `Vec<(usize, NodeId)>`） | 1 件（`CdpEndpoints::into_parts`） | 0 件 |
| `Option<T>` を返す関数 | 12 件（分類器は `Option<Kind>`） | 3 件（ターゲット / セッション引き） | 0 件 |
| `Result<_, E>` の `E` が構造化 | すべて型付き（`#[non_exhaustive]`。ただし単位バリアント中心） | 型付き（`CdpStateError`・`WsConfigError`、`#[non_exhaustive]`） | 型付き（`ProfileError`、`#[non_exhaustive]`・フィールド付き） |
| 成功値が `()` の `Result` | 1 件（`PluginRegistry::register`） | 2 件 | 2 件（`delete`・`assert_within_root`） |
| 生のバイト列（`Vec<u8>`）で返す | 2 件（`plugins_body`・`snapshot_body`） | 0 件 | 0 件 |

cdp の `String` 戻りは非公開の `protocol` モジュールの `to_json` のみで、公開面には存在しない。

### fandhe-browser-ai（簡約表現・プラグイン API）

| モジュール | 関数群 | 戻り値型 | 型の形 | `#[non_exhaustive]` | 備考 |
| ---------- | ------ | -------- | ------ | ------------------- | ---- |
| `snapshot` | `Snapshot::new`・`with_truncated`、`Node` / `FoldedRow` / `HeaderCell` / `RowControl` / `TableRow` / `TableSummary` の `new`・`with_*` | `Self`（ビルダー） | 構造体 | 付与 | `Snapshot { tree, truncated }` は構造体で拡張可能。ビルダーで追加フィールドに追従できる |
| `snapshot` | `Node::push_child` | `()` | — | — | 破壊的変更の影響は小さい（可変参照を取る更新メソッド） |
| `snapshot::build` | `build_snapshot` | `Result<Snapshot, SnapshotError>` | 構造体 + enum エラー | 付与 | |
| `snapshot::element_ref` | `RefAllocator::allocate` / `allocate_signature` | `Result<ElementRef, RefError>` | 構造体（`digest`・`variant`・`occurrence`）+ enum エラー | 付与 | `RefError` は単位バリアント 1 つ（`OccurrenceOverflow`） |
| `snapshot::element_ref` | `ref_signature`・`ElementSignature::digest` | `u64` | 素の数値 | — | `ElementRef` に包まれる前の中間値 |
| `snapshot::element_ref` | `ElementRef::to_ref_string` | `String` | 素の文字列 | — | `e<16hex>[v<variant>][-<n>]` 形式（`AISNAP-10`）を固定して返す |
| `snapshot::name` | `compute_name`・`compute_name_with_index` | `AccessibleName` | 構造体（text・source・truncated 等） | 付与 | |
| `snapshot::role` | `compute_role`・`ComputedRole::source`・`RoleSource::as_str` | `Option<ComputedRole>`・`RoleSource`・`&'static str` | 構造体 / enum | 付与 | |
| `snapshot::state` | `compute_state`・`State::with_*` | `State`・`Self` | 構造体 | 付与 | |
| `compress_table` | `detect_regular_structure` | `TableDetection` | enum（規則的 / 不規則 + `IrregularReason`） | 付与 | |
| `compress_table` | `TableDetection::is_regular` / `as_regular` | `bool` / `Option<&RegularStructure>` | 真偽値 / Option | — | `bool` は構造化版の簡易ラッパー |
| `compress_table` | `compress_row_list`・`compress_rows`・`assign_header_refs` | `Vec<CompressedRow>`・`CompressedRows`・`Result<Vec<HeaderCellRef>, RefError>` | 構造体 | 付与 | |
| `compress_table` | `rows_to_fold` | `Vec<(usize, NodeId)>` | タプル | — | 問題候補 A-1 |
| `data_leaf` | `classify_data_leaf` / `is_data_leaf` | `Option<DataLeafKind>` / `bool` | enum / 真偽値 | 付与（`DataLeafKind`） | `is_data_leaf` は簡易版と明記 |
| `retention` | `select_retained`・`select_retained_with_priority` | `Retention`（`kept`・`omitted`） | 構造体 | 付与 | |
| `retention` | `Retention::apply` | `Vec<&T>` | コレクション | — | 問題候補 A-5（省略件数が戻り値に出ない） |
| `retention` | `priority_candidates`・`pagination_candidates`・`submit_button_candidates` | `Vec<PriorityCandidate>` | 構造体のコレクション | 付与 | |
| `retention::pagination` / `submit_button` | `classify_*`・`find_*` | `Option<PaginationKind>` / `Option<SubmitButtonKind>` | enum の Option | 付与 | 問題候補 A-4 |
| `retention::pagination` | `rank` | `u8` | 素の数値 | — | |
| `plugin_api` | `PluginManifest::from_slice` / `from_value` | `Result<Self, ManifestError>` | 構造体 + enum エラー | 付与 | |
| `plugin_api` | `PluginManifest` の各アクセサ | `&str`・`&[String]`・`&[PluginPermission]`・`PluginTransport`・`SupportTier`・`Option<Plug3Targets>` | 型付き + 一部が文字列 | 付与 | 問題候補 A-6（`protocol_version`・`runtime`・`language`） |
| `plugin_api` | `Plug3Targets` のアクセサ（予算 4 件。`idle_rss_increase_max_bytes` 等） | `u64` | 素の数値 | — | 予算は `#[non_exhaustive]` の `Plug3Targets` に束ね済み（`plugin_api.rs:113-150`）。問題候補 A-6 |
| `plugin_api` | `PluginManifest::to_value` | `serde_json::Value` | 動的値 | — | 出力用（`host-api.schema.json` に対応） |
| `plugin_api` | `PluginRegistry::register` | `Result<(), RegistryError>` | `()` 成功 | 付与（エラー） | 問題候補 A-7 |
| `plugin_api` | `PluginRegistry::list`・`len`・`is_empty` | `Vec<PluginManifest>`・`usize`・`bool` | | 未付与（`PluginRegistry`） | |
| `api` | `router`・`router_with_state` | `Router` | フレームワーク型 | — | |
| `api` | `register_plugin` | `Result<RegisterResult, RegisterError>` | 構造体 + enum エラー | 付与 | |
| `api` | `plugins_body`・`snapshot_body` | `Vec<u8>`・`Result<Vec<u8>, ApiError>` | 生のバイト列 | 付与（`ApiError`） | 問題候補 A-2・A-3 |

### fandhe-browser-cdp（CDP 応答型・ターゲット管理）

| モジュール | 関数群 | 戻り値型 | 型の形 | `#[non_exhaustive]` | 備考 |
| ---------- | ------ | -------- | ------ | ------------------- | ---- |
| `server` | `CdpState::new`・`with_browser_id` | `Self` | 構造体 | 未付与 | 状態保持型 |
| `server` | `CdpState::received_methods` | `ReceivedMethodsSnapshot`（`MethodCounts` 含む） | 構造体 | 付与 | 観測用の構造化スナップショット |
| `server` | `CdpState::app_state`・`browser_id`・`registry` | `&Arc<AppState>`・`&BrowserId`・`&TargetRegistry` | 参照 | — | |
| `server` | `endpoints` | `Result<CdpEndpoints, WsConfigError>` | 構造体 + enum エラー | 付与（エラー） | |
| `server` | `CdpEndpoints::into_parts` | `(Router, WebSocketConfig)` | タプル | — | 問題候補 C-1 |
| `target` | `TargetId::parse`・`as_str` ほか | `Result<Self, CdpStateError>`・`&str` | 新しい型（newtype） | — | ID は newtype で型付き |
| `target` | `TargetInfo::target_id`・`kind`・`url` | `&TargetId`・`TargetKind`・`&str` | 構造体のアクセサ | 付与 | 問題候補 C-4（`url` が `&str`） |
| `target` | `TargetRegistry::create_target`・`attach`・`detach` | `Result<TargetId, CdpStateError>`・`Result<SessionId, CdpStateError>` | ID 型 | 付与（エラー） | |
| `target` | `TargetRegistry::set_target_url` | `Result<(), CdpStateError>` | `()` 成功 | 付与（エラー） | |
| `target` | `TargetRegistry::close_target` | `Result<Vec<SessionId>, CdpStateError>` | コレクション | 付与（エラー） | 問題候補 C-3 |
| `target` | `TargetRegistry::target`・`targets`・`session_target` | `Option<TargetInfo>`・`Vec<TargetInfo>`・`Option<TargetId>` | Option / コレクション | | 問題候補 C-2（Option と Result の使い分け） |
| `target` | `target_count`・`session_count` | `usize` | 素の数値 | — | |

### fandhe-browser-profile（Profile API）

| モジュール | 関数群 | 戻り値型 | 型の形 | `#[non_exhaustive]` | 備考 |
| ---------- | ------ | -------- | ------ | ------------------- | ---- |
| `normalize` | `normalize_name`・`NameRegistry::insert` | `Result<NormalizedName, ProfileError>` | newtype + enum エラー | 付与（エラー） | `NormalizedName` は newtype（フィールド非公開）。検証済みの型 |
| `normalize` | `NameRegistry::contains` | `bool` | 真偽値 | — | 問題候補 P-1 |
| `normalize` | `as_str`・`as_safe_component` | `&str`・`SafeComponent<'_>` | 検証済みの型 | — | |
| `profile` | `sanitize_component` | `Result<SafeComponent<'_>, ProfileError>` | 検証済みの型 | — | 文書上も「真偽値・フラットな文字列で返さない」(REPAIR-4) と明記 |
| `profile` | `assert_within_root` | `Result<(), ProfileError>` | `()` 成功 | 付与（エラー） | 検証関数のため成功値なしは自然 |
| `profile` | `Profile::open` | `Result<Profile, ProfileError>` | 構造体 | 付与（エラー） | |
| `profile` | `Profile::root`・`data_dir`・`DataKind::dir_name` | `&Path`・`PathBuf`・`&'static str` | パス | 付与（`DataKind`） | 問題候補 P-3 |
| `profile` | `Profile::root_fd`・`data_dir_fd` | `BorrowedFd<'_>` | OS 固有のハンドル | — | 問題候補 P-2（unix 限定） |
| `profile` | `Profile::create_file_in` | `Result<std::fs::File, ProfileError>` | 標準型 | 付与（エラー） | |
| `profile` | `Profile::delete` | `Result<(), ProfileError>` | `()` 成功 | 付与（エラー） | 問題候補 P-4 |
| `store` | `OsDefaultStore` / `ExplicitStore` / `OverridableStore` の `new`・`path`・`source`・`into_path` | `Self`・`&Path`・`RootSource`・`PathBuf`・`Result<Self, ProfileError>` | 構造体 / enum | `RootSource` は付与、ストア型は未付与 | 状態保持型 |
| `store` | `to_long_path`・`LongPath::kind` | `Result<LongPath, ProfileError>`・`LongPathKind` | 構造体 / enum | `LongPathKind` は付与、`LongPath` は未付与 | `LongPath` はアクセサ経由 |

## 総評

- ai の簡約表現（`Snapshot`・`Node`・`Retention` 等）は構造体 + `#[non_exhaustive]` + ビルダーで揃っており、PoC-10 の T-4 型の事象（木構造の表現力不足）に対して拡張余地がある。成功値は構造化型、エラーも型付き enum である
- cdp / profile は、外部入力を扱う境界（`TargetId`・`NormalizedName`・`SafeComponent`）が newtype で、戻り値の素の文字列化を避けている
- 問題候補は主に「`bool`・タプル・単位バリアントのエラー・成功値 `()`」に集中する。いずれも現状は機能上の不具合ではなく、**将来の拡張時に戻り値型を変えると呼び出し側が壊れる**という REPAIR-4 の観点上の候補である

## 問題候補と判断

各候補に判断（`改修` / `現状維持` / `保留`）と理由を記す。改修案・理由案は判断の根拠として残す。

### fandhe-browser-ai

#### A-1 `rows_to_fold` のタプル戻り値

- 現状: `Vec<(usize, NodeId)>`（行番号と DOM ノード ID の組。位置が意味を持つ）
- 改修案: `#[non_exhaustive]` の `FoldTarget { row_index: usize, node: NodeId }` を導入し、`Vec<FoldTarget>` を返す。省略対象に理由・行数などを足せる
- 現状維持の理由案: `crate` 内の呼び出し元が `snapshot::build` の 1 箇所で、影響範囲が小さく型を増やすコストの方が高い
- 判断: **改修**（置換）。位置依存のタプルと並列リストの `zip` 突き合わせを型で解消する。AISNAP-12 の優先保持など要素を足す際に 2 か所同時改修になるのを防ぐ。呼び出し元は ai crate 内で完結するため置換を採り、同義 API を残さない。影響範囲: 本番 1（`snapshot/build.rs:773`）・テスト等 0。Issue 化: 単独 1 件（0.5〜1h）

#### A-2 `ApiError` が原因を保持しない

- 現状: `ApiError::{NoNavigation, Parse, Snapshot, Serialize}` はすべて単位バリアント。`Snapshot` は下位の `SnapshotError` の内容を捨てる
- 改修案（採らない）: `Snapshot(SnapshotError)` のように既存の単位バリアントへフィールドを足すのは、パターン `ApiError::Snapshot` が一致しなくなる破壊的変更である。また `ApiError` は `Copy` を derive しており（`api.rs:60`）、`SnapshotError` を持たせると `Copy` も外れる。原因が必要になった場合は、既存バリアントを残して `std::error::Error::source` を返す別バリアントを追加するか、内部用エラー型を設ける（いずれも非破壊）
- 現状維持の理由案: 原因詳細を外部へ返さない設計意図（`api.rs:58-59`）。`SnapshotError` は `Ref(RefError)` の 1 種のみで、`RefError` も単位 1 つ（`snapshot/build.rs:88-91`）のため捨てる情報が実質ない
- 判断: **現状維持**。固定文言で詳細を出さないのは情報漏えい防止の明示的設計で、可観測性設計の「`FailureKind` は固定列挙・message を持ち込まない」（`docs/design/observability.md:35`）とも一致する。影響範囲: 本番 2（`api.rs` 内の `error_response`・`snapshot_body`）・テスト等 2 ファイル（`api.rs` テスト・`tests/mcp_token_reduction.rs:149`）

#### A-3 `plugins_body` / `snapshot_body` が `Vec<u8>` を返す

- 現状: JSON 本文を生のバイト列で返す（ステータス・Content-Type は呼び出し側のハンドラが決める）
- 改修案: `#[non_exhaustive]` の `ApiBody { bytes, content_type }` のような型で返し、メタデータを足せる構造にする
- 現状維持の理由案: ドキュメント上「純粋部」として HTTP から切り離された設計（テスト容易性）で、ステータス・Content-Type はハンドラ側が決める
- 判断: **現状維持**。HTTP から切り離した純粋部で、ステータス・Content-Type はハンドラが決める責務分割（`api.rs:108-118`・`:276`）。外部契約は JSON 本文（`host-api.schema.json`・AISNAP-6）でありバイト列の Rust 型ではない。メタデータを足す具体的な予定はなく、HTTP 応答型の拡張はハンドラ側（`Response`）で吸収できる。影響範囲: 本番 2（`api.rs:108,118`）・テスト等 3 ファイル

#### A-4 分類器の `Option<Kind>`（`classify_*`・`find_*`）

- 現状: 該当なしを `None` で表し、「なぜ該当しなかったか」「確度」は返さない
- 改修案: `Classification { kind, reason }` のような構造体の `Option` にする。あるいは判定根拠を含む型へ置き換える
- 現状維持の理由案: 単純な分類結果には `Option<enum>` が自然で、enum が `#[non_exhaustive]` のため種別の追加は非破壊。優先度は `PriorityCandidate` が別途保持している
- 判断: **現状維持**。`PaginationKind`・`SubmitButtonKind`・`DataLeafKind` は `#[non_exhaustive]` で種別追加は非破壊。順位は `PriorityCandidate` が別途保持する。呼び出し元が多く置換コストが大きい。判定根拠が必要になった時（例: AISNAP-10 の参照安定性で判定理由を診断に出す等）に `classify_*_detailed` を非破壊追加し、既存の `Option<Kind>` は残す。影響範囲: `classify_data_leaf` 本番 4（`build.rs:258,727,826`・`data_leaf.rs:163`）＋`find_pagination_link`・`find_submit_button` 各本番 1・テスト等 約 70

#### A-5 `Retention::apply` が省略件数を返さない

- 現状: `Vec<&T>`（保持された要素のみ）。省略件数は `Retention::omitted` で別途得られる
- 改修案: 保持要素と省略件数を併せ持つ型（`Applied<'a, T> { items, omitted }`）を返す
- 現状維持の理由案: `Retention` 自体が `kept` / `omitted` を持つ構造体で、`apply` は派生操作に過ぎない
- 判断: **現状維持**。`Retention`（`retention.rs:140-145`）が `omitted` を保持済みで、`apply` は添字の写像という派生操作（`:152`）。省略件数を戻り値に重ねると情報が 2 重化する。影響範囲: 本番 1（`compress_table.rs:589`）・テスト等 7

#### A-6 プラグイン許容値の素の `u64` と自由形式の `&str`

- 現状: 性能予算 4 値（`idle_rss_increase_max_bytes`・`cold_start_max_ms`・`call_latency_p50_max_ms`・`call_latency_p95_max_ms`）は `Plug3Targets` のアクセサが `u64` で返す（単位は関数名に埋め込み）。`PluginManifest::protocol_version`・`runtime`・`language` が `&str`
- 改修案（採らない）: 単位付き newtype（`Bytes`・`Millis`）化。`protocol_version` の構造化バージョン型化。予算の束ね自体は `Plug3Targets` で実現済みのため、この部分は改修案から外す
- 現状維持の理由案: 値は定数として束ねられ、宣言（JSON）も文字列のまま扱う設計。`Plug3Targets` が構造体として束ねている
- 判断: **現状維持**。予算は `Plug3Targets`（`#[non_exhaustive]`・非公開フィールド＋アクセサ、`plugin_api.rs:113-150`）に構造化済み。`protocol_version`・`runtime`・`language` は untrusted な自己申告の保持で、`support_tier` 判定以外に分岐へ使わない設計（`plugin_api.rs:65-69`）。spec（`04-behavior/`・`05-tasks.md`）にバージョン交渉の定義はなく、構造化バージョン型を要する根拠がない。改修する場合はバージョン交渉がビヘイビア化された時点で `protocol_version_parsed() -> Option<ProtocolVersion>` を非破壊追加する。影響範囲: ai crate 内のみ（`plugin_api.rs` と `tests/plugin_api.rs`）

#### A-7 `PluginRegistry::register` の成功値 `()`

- 現状: `Result<(), RegistryError>`。登録件数や置換の有無は返さない
- 改修案: `RegisterOutcome`（`#[non_exhaustive]`）を返す
- 現状維持の理由案: 重複は `RegistryError` でエラー扱いなので、成功時に伝える追加情報がない。api 層は `RegisterResult` で構造化済み
- 判断: **現状維持**。重複・満杯はエラー、削除・上書き不可の設計（`plugin_api.rs:578-583`）で、成功時に伝える追加情報がない。上位の `register_plugin` は `#[non_exhaustive]` の `RegisterResult` で構造化済み（`api.rs:195-219`）であり、拡張点はこちらに一本化されている。影響範囲: 本番 1（`api.rs:217`）・テスト等 4 ファイル

### fandhe-browser-cdp

#### C-1 `CdpEndpoints::into_parts` のタプル

- 現状: `(Router, WebSocketConfig)`。ドキュメントに「片方だけ渡すと `/json/version` の URL が接続不能になる」と明記
- 改修案: 取り出し用の `#[non_exhaustive]` 構造体（`EndpointParts { router, websockets: Vec<WebSocketConfig> }`）、または一括配線メソッド `CdpEndpoints::install(self, server, extra_routers)` へ置換する
- 現状維持の理由案: 2 要素が対で使われる契約を意図的にタプルで表している。唯一の利用先は cli で、将来要素が増えても `CdpEndpoints` 自体に新メソッドを足せる
- 判断: **保留**（トリガー: `/devtools/page/{id}` 受け口を実装する page WS 実装 Issue。TASK-42 以降・CDP-1）。現行型では要素を増やせず、増やす予定がコードに明記されている（`discovery.rs:19-20`）。page 用の WS 設定が加わると `Server::websocket` 側の契約（ルータと WS を必ず対で渡す。`server.rs:128-131`）を古い呼び出しが黙って破るため、トリガー時に置換し、置換は同一 PR で全呼び出し元を移行する。影響範囲: 本番 1（`crates/fandhe-browser-cli/src/server.rs:189`）・テスト等 5（`examples/trace_server.rs`・`tests/devtools_browser.rs`・`tests/json_endpoints.rs`・`tests/playwright_trace.rs`・`tests/script_harness/server.rs`）

#### C-2 `CdpStateError` の文脈・Option と Result の混在

- 現状: エラーは上限値などの文脈をフィールドで持つ（`TooManyTargets { limit }`・`TooManySessions { limit }`・`UrlTooLong { limit }`、`target.rs:39-57`）。対象 ID は持たない。`TargetRegistry::target` / `session_target` は `Option`、`set_target_url` / `attach` は `Result` で不在を表す
- 改修案（採らない）: エラーに対象 ID を持たせる。ID は呼び出し側が既に持っており、エラーへ入れると入力値の露出につながる（`target.rs:31`）
- 現状維持の理由案: CDP は不在を `Option` で問い合わせ、変更系だけエラーにする使い分けが自然。ID は呼び出し側が既に持つ
- 判断: **現状維持**。入力 ID・URL を含めないのは意図的な設計で（`target.rs:31`・`.claude/rules/security.md`）、不在を「照会は `Option`（`target`・`session_target`。`target.rs:267,340`）、変更系は `Result`（`attach`・`set_target_url`。`:313,286`）」で分ける使い方は CDP ハンドラの方針として文書化する。影響範囲: `CdpStateError` 参照 38（`target.rs` 31・`page.rs` 3・`tests/state.rs` 3・`lib.rs` 1）

#### C-3 `close_target` の `Vec<SessionId>`

- 現状: 閉じたターゲットの切断されたセッション ID の列だけを返す
- 改修案: `CloseOutcome { target, detached_sessions }`（`#[non_exhaustive]`）を返す
- 現状維持の理由案: CDP の `Target.detachedFromTarget` イベント発行に必要な情報はこれで足りる
- 判断: **保留**（トリガー: `Target.closeTarget`・`Target.detachedFromTarget` ハンドラの実装 Issue）。本番呼び出し元 0（`navigation.rs`・`page.rs` の呼び出しはすべて `#[cfg(test)]` 以降）で、doc も「将来 `Target.detachedFromTarget` の送出に使う」（`target.rs:292-294`）と未使用を明示している。必要な情報は現行型で足りる見込みで、追加情報が要るかはハンドラ設計時に確定する。追加が必要なら置換（`CloseOutcome`）を採る。影響範囲: 本番 0・テスト等 7

#### C-4 `TargetInfo::url` が `&str`

- 現状: URL を素の文字列で返す（CDP の `targetInfo.url` が文字列であるため）
- 改修案: 検証済みの URL 型を返す
- 現状維持の理由案: CDP は `about:blank` など URL として未検証の値も許容する。型を強めるとプロトコル互換を損なう
- 判断: **現状維持**。CDP の `targetInfo.url` は `about:blank` 等の非 URL 値も運ぶ文字列で、検証済み URL 型にするとプロトコル互換を損なう。長さ上限は `UrlTooLong` で検証済み（`target.rs:53-57`）。影響範囲: `crates/fandhe-browser-cdp/src` 内の `.url()` 参照 25

### fandhe-browser-profile

#### P-1 `NameRegistry::contains` の `bool`

- 現状: 「正規化できない入力は `false`」とし、不正入力と未登録を区別しない
- 改修案: `Result<bool, ProfileError>` または `Lookup`（`#[non_exhaustive]` の enum: 登録済み / 未登録 / 不正）を返す
- 現状維持の理由案: 呼び出し元は登録処理（`insert`）の事前確認程度で、不正入力は `insert` がエラーを返す。「登録されていない」ことに変わりはない
- 判断: **現状維持**。本番呼び出し元 0（テストのみ）。集合の所属判定に `bool` は自然で、不正入力が `false` になる点は doc で明示済み（`normalize.rs:142` 付近）。正規化・衝突検出の正規経路は `insert -> Result<NormalizedName, ProfileError>`（XOS-9）で構造化済み。Cookie・Storage 実装で事前照会が必要になった時点で `lookup(&OsStr) -> Result<Option<NormalizedName>, ProfileError>` を非破壊追加する。影響範囲: 本番 0・テスト等 3

#### P-2 `root_fd` / `data_dir_fd` の OS 固有型

- 現状: `BorrowedFd<'_>`（unix 限定。Windows では提供されない）。`pub` で公開されている（`profile.rs:682`・`:692`）
- 改修案: 可視性を `pub(crate)` へ縮小する。OS 非依存のハンドル型で包む案もあるが、Windows のハンドル基準アクセスは別課題のため採らない
- 現状維持の理由案: ハンドル経由でパスの再解決を避ける目的（テスト名に `handle_not_re_resolved_path` がある）で、unix 限定の最適化として `cfg` で局所化している
- 判断: **改修**（`pub` → `pub(crate)`）。上位 crate が unix 限定の API を使うと Windows でビルドできないコードを書けてしまい、XOS 一級対応と「OS 固有処理は `cfg` で局所化」（`.claude/rules/coding-rust.md`）に反する。利用者は crate 内の `create_file_in`（`profile.rs:752`）系のみで、crate 外の呼び出しは 0。PR #437 P1 で定めた「後続タスクはハンドルを使う」契約は変更するが、ハンドル基準で境界内へアクセスする目的は `create_file_in` 系の crate 内部実装で維持する（TOCTOU 対策は後退させない）。`profile.rs` の doc（「後続タスクはこのハンドルを使う契約」）は「上位 crate は `create_file_in` 系の OS 非依存操作を使い、ハンドルは内部の実装詳細」へ書き換える。Windows のハンドル基準アクセスは XOS-7〜XOS-10 の別課題。影響範囲: crate 外 0・crate 内 `profile.rs` のみ（公開 API の削除だが利用者 0）。Issue 化: 単独 1 件（0.5h）

#### P-3 `root` が `&Path`、`data_dir` が `PathBuf`

- 現状: 同種のパス取得で借用と所有が混在する
- 改修案: 戻り値の所有形態を方針化して揃える
- 現状維持の理由案: `data_dir` は `root` と `DataKind` から都度組み立てる値で、所有の戻り値が自然。公開契約上の問題はない
- 判断: **現状維持**。`root` は保持値の借用、`data_dir` は `root.join(kind.dir_name())` の都度生成（`profile.rs:659-672`）で所有形態の違いは自然。どちらも「表示・ログ用、再解決禁止」と doc 化済み。影響範囲: —

#### P-4 `Profile::delete` の `()`・`ProfileError` の自由形式 `reason`

- 現状: 削除結果（削除したエントリ数など）を返さない。`ProfileError::InvalidLayout` / `Unsupported` の `reason` は `&'static str`
- 改修案: `DeleteReport`（`#[non_exhaustive]`）を返す。`reason` を `#[non_exhaustive]` の enum にする
- 現状維持の理由案: 削除は「成功か拒否か」が本質で、失敗時は何も削除しない（fail-closed）。`reason` は診断用で、分岐に使わせない設計
- 判断: **現状維持**（TASK-47 の設計時に判断）。削除は fail-closed（成功か拒否か）で、本番呼び出し元 0（テストのみ）。`reason` は診断用固定英語文言で、`InvalidComponent` は untrusted 入力を保持しない設計（`profile.rs` の `ProfileError` doc）。`reason` の enum 化はフィールド型の変更で破壊的、かつ構築箇所が多い（`profile.rs` 59・`store.rs` 29 ほか）。CLI（TASK-47）の終了コード・機械可読エラーで分岐が必要になった時点で、`ManifestError::code()`（`plugin_api.rs:287`）と同型の `ProfileError::code() -> &'static str` を非破壊追加する。影響範囲: 本番 0・テストのみ

## 候補一覧と判断

候補 15 件の一覧。呼び出し元数は main 03b8617 の `git grep` による（本番 / テスト等）。

| ID | crate | 対象 | 判断 | 一行理由 | 本番 / テスト等 | 後続 |
| -- | ----- | ---- | ---- | -------- | --------------- | ---- |
| A-1 | ai | `rows_to_fold` | **改修**（置換） | 位置依存タプルと並列リストの `zip` 突き合わせを型で解消 | 本番 1 / テスト等 0 | 単独 Issue（0.5〜1h） |
| A-2 | ai | `ApiError` | 現状維持 | 捨てる原因は実質 1 種。固定文言は情報漏えい防止と一致。単位バリアントへのフィールド追加は破壊的 | 本番 2 / テスト等 2 ファイル | なし |
| A-3 | ai | `plugins_body` / `snapshot_body` | 現状維持 | HTTP から切り離した純粋部。外部契約は JSON 本文 | 本番 2 / テスト等 3 ファイル | なし |
| A-4 | ai | `classify_*` / `find_*` | 現状維持 | 種別 enum は `#[non_exhaustive]` で追加は非破壊。置換コストが大きい | 本番 4＋各 1 / テスト等 約 70 | なし |
| A-5 | ai | `Retention::apply` | 現状維持 | `Retention` が `omitted` を保持済み。`apply` は派生操作 | 本番 1 / テスト等 7 | なし |
| A-6 | ai | プラグイン予算・自由形式の文字列 | 現状維持 | 予算は `Plug3Targets` に構造化済み。バージョン交渉は spec 未定義 | ai crate 内のみ | なし |
| A-7 | ai | `PluginRegistry::register` | 現状維持 | 成功時に伝える追加情報がない。上位 `RegisterResult` が構造化済み | 本番 1 / テスト等 4 ファイル | なし |
| C-1 | cdp | `CdpEndpoints::into_parts` | **保留** | page WS 受け口の追加で要素が増える予定。現行の契約はそのとき破られる | 本番 1 / テスト等 5 | page WS 実装 Issue に内包（1h） |
| C-2 | cdp | `CdpStateError`・Option / Result | 現状維持 | フィールド付きバリアントで文脈を持つ。ID 不保持は意図的 | 参照 38（テスト等を含む） | なし |
| C-3 | cdp | `close_target` | **保留** | 本番呼び出し元 0。戻り値型はハンドラ設計時に確定 | 本番 0 / テスト等 7 | ハンドラ実装 Issue に内包 |
| C-4 | cdp | `TargetInfo::url` | 現状維持 | CDP の url は非 URL 値も運ぶ文字列。長さは検証済み | 参照 25 | なし |
| P-1 | profile | `NameRegistry::contains` | 現状維持 | 正規経路 `insert` が構造化済み | 本番 0 / テスト等 3 | なし |
| P-2 | profile | `root_fd` / `data_dir_fd` | **改修**（`pub(crate)` へ縮小） | unix 限定ハンドルが `pub` で上位 crate へ漏れ得る（XOS 違反） | crate 外 0 | 単独 Issue（0.5h） |
| P-3 | profile | `root` / `data_dir` | 現状維持 | 借用と都度生成の違いは自然。doc 化済み | — | なし |
| P-4 | profile | `Profile::delete`・`reason` | 現状維持 | fail-closed で成功値不要。`reason` の型変更は破壊的 | 本番 0 / テストのみ | なし（TASK-47 時に判断） |

内訳: 改修 2（A-1・P-2）・現状維持 11（A-2・A-3・A-4・A-5・A-6・A-7・C-2・C-4・P-1・P-3・P-4）・保留 2（C-1・C-3）。

## 関連

- ビヘイビア: `REPAIR-4`（主）、`REPAIR-3`（未実装箇所の明示）、`AISNAP-6`・`AISNAP-12`（A-1・A-3）、`CDP-1`（C-1）、`XOS-7`〜`XOS-10`・`XOS-9`（P-1・P-2）、`PLUG-6`（A-6）
- タスク: TASK-4（`docs/spec/05-tasks.md:66`）・前提 TASK-11・TASK-42・TASK-50・関連 TASK-47・TASK-51
- マイルストーン: MS-7
- Issue: #328（本レビュー）・#817（本 PR）・#437（P-2 の契約の出どころ）・後続の実装 Issue #838・#839
- 参考: `docs/design/repair-trial-record.md`（R-4・R-5）
