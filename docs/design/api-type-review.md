# 公開 API 戻り値型の拡張性レビュー（草案）

**状態**: 草案（オーナー判断待ち）  
**対象**: `04-behavior/self-repair-design.md` REPAIR-4（TASK-4・MS-7・Issue #328）  
**調査基準**: `origin/main` 43262c1

## 位置づけ

TASK-4 は、公開 API（検索結果型等）の戻り値型が将来の拡張を見越した構造を持つことを、PoC-10 の教訓（タスク T-4 で発生した「`ElementRef` 直接走査への切替」という追加設計判断。型の表現力不足が改修コストを押し上げた）を踏まえてレビューする人間担当（アーキテクチャ判断）のタスクである。本書はその準備として、事実の洗い出しと問題候補の抽出までを行う草案であり、**各候補の採否（型変更か現状維持か）は「判断欄」にオーナーが記入する**。本書のどの候補も、現時点では未決定（オーナー判断待ち）である。

REPAIR-4 の要求は次のとおり（spec が正）。

- 前提: 公開 API（検索結果型等）の戻り値型を設計する
- 期待: 型の表現力不足による改修コスト増大が再発しない構造になっている

### 受け入れ条件（Issue #328）との対応

| 受け入れ条件 | 本書の該当箇所 |
| ------------ | -------------- |
| 主要な公開 API 戻り値型を洗い出し、拡張性の観点でレビューした記録がある | 「洗い出し結果」「観点別サマリー」 |
| 表現力不足が見つかった型の改修方針（型変更または現状維持の判断理由）が記載されている | 「問題候補と判断欄」（両案を併記。採否は未記入） |
| レビュー結果が本書に残る | 本書 |

### 判断基準（`docs/design/repair-trial-record.md` R-4・R-5 に基づく）

試行用候補 R-4（`FetchResponse::mime_type()` → `#[non_exhaustive]` の `MimeType`）と R-5（`ParseDiagnosticEntry { line, message }`。既存 `messages` は互換のため残す）は、次の方針を示している。

- 真偽値・素の文字列・タプルで返さず、`#[non_exhaustive]` の構造化型（名前付きフィールド・アクセサ）で返す
- 既存の戻り値は互換のため残し、構造化型を追加する形で拡張する

本書の「問題」は、この方針から外れ、かつ将来の拡張で戻り値型の変更（＝呼び出し側の破壊的変更）が起き得るものを指す。

## 洗い出し方法と範囲

- 対象: `fandhe-browser-ai`・`fandhe-browser-cdp`・`fandhe-browser-profile` の `src/` 配下で `pub fn` として宣言された関数・メソッドの戻り値型（`#[cfg(test)]` 以降は除外）。機械的な抽出に基づくため、`impl Trait` を返す宣言や複数行シグネチャの一部は丸めている
- `fandhe-browser-cdp` の `protocol` モジュールは非公開（`mod protocol`・型は `pub(crate)`）のため、その `pub fn`（9 件）は公開 API の対象外とした。公開面は `lib.rs` の再エクスポートと `pub mod server`・`pub mod target`
- 件数: ai 105 件・cdp 25 件（公開面のみ）・profile 33 件
- 型の宣言（トップレベルの `pub struct` / `pub enum`）の `#[non_exhaustive]` 付与状況: ai 43 付与 / 4 未付与、cdp 6 / 3、profile 4 / 9。未付与は主に状態保持型（レジストリ・アロケータ・ストア）で、フィールド公開の有無までは本草案で全数精査していない

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
| `plugin_api` | `SupportTier` の予算 4 件（`idle_rss_increase_max_bytes` 等） | `u64` | 素の数値 | — | 問題候補 A-6 に含める |
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

## 問題候補と判断欄

各候補に「改修案（型変更）」と「現状維持の理由案」を併記する。判断欄はオーナーが記入する（`改修` / `現状維持` / `保留` と理由）。

### fandhe-browser-ai

#### A-1 `rows_to_fold` のタプル戻り値

- 現状: `Vec<(usize, NodeId)>`（行番号と DOM ノード ID の組。位置が意味を持つ）
- 改修案: `#[non_exhaustive]` の `FoldTarget { row_index: usize, node: NodeId }` を導入し、`Vec<FoldTarget>` を返す。省略対象に理由・行数などを足せる
- 現状維持の理由案: `crate` 内の呼び出し元が `snapshot::build` の 1 箇所で、影響範囲が小さく型を増やすコストの方が高い
- 判断欄: （未記入）

#### A-2 `ApiError` が原因を保持しない

- 現状: `ApiError::{NoNavigation, Parse, Snapshot, Serialize}` はすべて単位バリアント。`Snapshot` は下位の `SnapshotError` の内容を捨てる
- 改修案: `Snapshot(SnapshotError)` のように原因を保持し、`std::error::Error::source` で辿れるようにする（バリアント追加は `#[non_exhaustive]` のため非破壊）。
- 現状維持の理由案: 原因詳細を外部へ返さない設計意図の可能性がある（要確認）。原因が必要になった時点でバリアントのフィールド追加で対応でき、`#[non_exhaustive]` のため破壊的でない
- 判断欄: （未記入）

#### A-3 `plugins_body` / `snapshot_body` が `Vec<u8>` を返す

- 現状: JSON 本文を生のバイト列で返す（ステータス・Content-Type は呼び出し側のハンドラが決める）
- 改修案: `#[non_exhaustive]` の `ApiBody { bytes, content_type }` のような型で返し、メタデータを足せる構造にする
- 現状維持の理由案: ドキュメント上「純粋部」として HTTP から切り離された設計（テスト容易性）で、ステータス・Content-Type はハンドラ側が決める
- 判断欄: （未記入）

#### A-4 分類器の `Option<Kind>`（`classify_*`・`find_*`）

- 現状: 該当なしを `None` で表し、「なぜ該当しなかったか」「確度」は返さない
- 改修案: `Classification { kind, reason }` のような構造体の `Option` にする。あるいは判定根拠を含む型へ置き換える
- 現状維持の理由案: 単純な分類結果には `Option<enum>` が自然で、enum が `#[non_exhaustive]` のため種別の追加は非破壊。優先度は `PriorityCandidate` が別途保持している
- 判断欄: （未記入）

#### A-5 `Retention::apply` が省略件数を返さない

- 現状: `Vec<&T>`（保持された要素のみ）。省略件数は `Retention::omitted` で別途得られる
- 改修案: 保持要素と省略件数を併せ持つ型（`Applied<'a, T> { items, omitted }`）を返す
- 現状維持の理由案: `Retention` 自体が `kept` / `omitted` を持つ構造体で、`apply` は派生操作に過ぎない
- 判断欄: （未記入）

#### A-6 プラグイン許容値の素の `u64` と自由形式の `&str`

- 現状: `SupportTier` の性能予算（`idle_rss_increase_max_bytes`・`cold_start_max_ms`・`call_latency_p50_max_ms`・`call_latency_p95_max_ms`）が `u64`（単位は関数名に埋め込み）。`PluginManifest::protocol_version`・`runtime`・`language` が `&str`
- 改修案: 予算を `#[non_exhaustive]` の `Plug3Budget` 構造体にまとめ、単位付き newtype（`Bytes`・`Millis`）にする。`protocol_version` は構造化されたバージョン型にする
- 現状維持の理由案: 値は定数として束ねられ、宣言（JSON）も文字列のまま扱う設計。`Plug3Targets` が構造体として束ねている
- 判断欄: （未記入）

#### A-7 `PluginRegistry::register` の成功値 `()`

- 現状: `Result<(), RegistryError>`。登録件数や置換の有無は返さない
- 改修案: `RegisterOutcome`（`#[non_exhaustive]`）を返す
- 現状維持の理由案: 重複は `RegistryError` でエラー扱いなので、成功時に伝える追加情報がない。api 層は `RegisterResult` で構造化済み
- 判断欄: （未記入）

### fandhe-browser-cdp

#### C-1 `CdpEndpoints::into_parts` のタプル

- 現状: `(Router, WebSocketConfig)`。ドキュメントに「片方だけ渡すと `/json/version` の URL が接続不能になる」と明記
- 改修案: 取り出し用の `#[non_exhaustive]` 構造体（`EndpointParts { router, websocket }`）にする
- 現状維持の理由案: 2 要素が対で使われる契約を意図的にタプルで表している。唯一の利用先は cli で、将来要素が増えても `CdpEndpoints` 自体に新メソッドを足せる
- 判断欄: （未記入）

#### C-2 `CdpStateError` が単位バリアントのみ・Option と Result の混在

- 現状: エラーに対象 ID などの文脈がない。`TargetRegistry::target` / `session_target` は `Option`、`set_target_url` / `attach` は `Result` で不在を表す
- 改修案: エラーに対象 ID を持たせる。不在の表現を `Result` に統一するかを方針化する
- 現状維持の理由案: CDP は不在を `Option` で問い合わせ、変更系だけエラーにする使い分けが自然。ID は呼び出し側が既に持つ
- 判断欄: （未記入）

#### C-3 `close_target` の `Vec<SessionId>`

- 現状: 閉じたターゲットの切断されたセッション ID の列だけを返す
- 改修案: `CloseOutcome { target, detached_sessions }`（`#[non_exhaustive]`）を返す
- 現状維持の理由案: CDP の `Target.detachedFromTarget` イベント発行に必要な情報はこれで足りる
- 判断欄: （未記入）

#### C-4 `TargetInfo::url` が `&str`

- 現状: URL を素の文字列で返す（CDP の `targetInfo.url` が文字列であるため）
- 改修案: 検証済みの URL 型を返す
- 現状維持の理由案: CDP は `about:blank` など URL として未検証の値も許容する。型を強めるとプロトコル互換を損なう
- 判断欄: （未記入）

### fandhe-browser-profile

#### P-1 `NameRegistry::contains` の `bool`

- 現状: 「正規化できない入力は `false`」とし、不正入力と未登録を区別しない
- 改修案: `Result<bool, ProfileError>` または `Lookup`（`#[non_exhaustive]` の enum: 登録済み / 未登録 / 不正）を返す
- 現状維持の理由案: 呼び出し元は登録処理（`insert`）の事前確認程度で、不正入力は `insert` がエラーを返す。「登録されていない」ことに変わりはない
- 判断欄: （未記入）

#### P-2 `root_fd` / `data_dir_fd` の OS 固有型

- 現状: `BorrowedFd<'_>`（unix 限定。Windows では提供されない）
- 改修案: OS 非依存のハンドル型（`DirHandle`）で包み、3 OS 一級対応（XOS 系）の API 面を揃える
- 現状維持の理由案: ハンドル経由でパスの再解決を避ける目的（テスト名に `handle_not_re_resolved_path` がある）で、unix 限定の最適化として `cfg` で局所化している。Windows 対応は別途の設計課題
- 判断欄: （未記入）

#### P-3 `root` が `&Path`、`data_dir` が `PathBuf`

- 現状: 同種のパス取得で借用と所有が混在する
- 改修案: 戻り値の所有形態を方針化して揃える
- 現状維持の理由案: `data_dir` は `root` と `DataKind` から都度組み立てる値で、所有の戻り値が自然。公開契約上の問題はない
- 判断欄: （未記入）

#### P-4 `Profile::delete` の成功値 `()` と `ProfileError` の自由形式 `reason`

- 現状: 削除結果（削除したエントリ数など）を返さない。`ProfileError::InvalidLayout` / `Unsupported` の `reason` は `&'static str`
- 改修案: `DeleteReport`（`#[non_exhaustive]`）を返す。`reason` を `#[non_exhaustive]` の enum にする
- 現状維持の理由案: 削除は「成功か拒否か」が本質で、失敗時は何も削除しない（fail-closed）。`reason` は診断用で、分岐に使わせない設計
- 判断欄: （未記入）

## 候補一覧と要判断事項

| ID | crate | 対象 | 現状の形 | 判断 |
| -- | ----- | ---- | -------- | ---- |
| A-1 | ai | `rows_to_fold` | タプル | 未記入 |
| A-2 | ai | `ApiError` | 単位バリアント | 未記入 |
| A-3 | ai | `plugins_body` / `snapshot_body` | `Vec<u8>` | 未記入 |
| A-4 | ai | `classify_*` / `find_*` | `Option<Kind>` | 未記入 |
| A-5 | ai | `Retention::apply` | `Vec<&T>` | 未記入 |
| A-6 | ai | プラグイン予算・自由形式の文字列 | `u64` / `&str` | 未記入 |
| A-7 | ai | `PluginRegistry::register` | `Result<(), _>` | 未記入 |
| C-1 | cdp | `CdpEndpoints::into_parts` | タプル | 未記入 |
| C-2 | cdp | `CdpStateError`・Option / Result | 単位バリアント | 未記入 |
| C-3 | cdp | `close_target` | `Vec<SessionId>` | 未記入 |
| C-4 | cdp | `TargetInfo::url` | `&str` | 未記入 |
| P-1 | profile | `NameRegistry::contains` | `bool` | 未記入 |
| P-2 | profile | `root_fd` / `data_dir_fd` | `BorrowedFd` | 未記入 |
| P-3 | profile | `root` / `data_dir` | `&Path` / `PathBuf` | 未記入 |
| P-4 | profile | `Profile::delete`・`reason` | `()` / `&'static str` | 未記入 |

総数は 15 件。オーナーに求める判断は次のとおり。

1. 各候補の採否（`改修` / `現状維持` / `保留`）
2. 型変更を採る場合の実施方法（本書の改修案を実装タスク化する範囲と、別 Issue への切り出し）
3. 既存の戻り値を残して追加する（R-4・R-5 方式）か、置き換える（破壊的変更）か。公開 API の破壊的変更の許容範囲（現在のバージョン・利用者の有無）

## 関連

- ビヘイビア: `REPAIR-4`（主）、`REPAIR-3`（未実装箇所の明示）
- 前提タスク: TASK-11・TASK-42・TASK-50
- 参考: `docs/design/repair-trial-record.md`（R-4・R-5）
