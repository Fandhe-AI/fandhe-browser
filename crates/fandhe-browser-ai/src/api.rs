//! `/ai/*` HTTP API の入口（`AISNAP-6`・TASK-19.1・Issue #223・`MS-4`、`PLUG-2`・TASK-92.3・Issue #355・`MS-9`）。
//!
//! core の [`AppState`] が保持する直近ナビゲート結果から [`Snapshot`]
//! （`AISNAP-1` 方式 B）を構築し、JSON で返す `GET /ai/snapshot` と、プラグインマニフェストを
//! 検証してレジストリへ登録する `POST /ai/plugins/register` のルータを提供する。
//! [`router`] は bind・アクセス制御（loopback 限定。`SEC-4`）を行わないが、DNS rebinding 対策として
//! `Host` ヘッダ（localhost / IP リテラルのみ許可）を cdp と共通の core 実装で検証する。cli が
//! `RouterFactory` として受け取り、cdp のルータと `Router::merge` で合成する
//! （TASK-19.3・Issue #225）。cdp と ai は互いに依存せず、`Arc<AppState>` だけを共有する。
//!
//! # スタブ・暫定仕様について（REPAIR-3）
//!
//! - JSON の形は spec `api-ai-snapshot.md` の想定スキーマ（検討中）に寄せた暫定形で、
//!   確定スキーマではない。エンベロープは `{"url", "tree", "truncated"}`。`url` は
//!   `NavigationResult::url` の値で、[`Snapshot`] 型には持たせない
//! - 未ナビゲート時は 409 と `{"code":"no_navigation","message":...}` を返す（成功を装わない。
//!   TASK-19.2・Issue #224・`AISNAP-14`）。spec 上 `AISNAP-14` の形式・ステータスは検討中で、
//!   spec の例示形に合わせた暫定確定。エラー本文は全 variant で `code` と `message` の 2 キー
//! - 共有状態を跨ぐ結合テストは cli の `server.rs` テスト（TASK-19.4・Issue #226。ai ⇔ cdp 依存は
//!   禁止のため両者を合成できる cli に置く）で実施済み
//! - プラグインレジストリを持つ ai 固有の状態型 `AiState` は定義済み（TASK-92.2・Issue #354）。
//!   [`router_with_state`] が `Arc<AiState>` を受け取り、`GET /ai/plugins`（TASK-92.4・Issue #356・
//!   `PLUG-2`）と登録 `POST /ai/plugins/register`（TASK-92.3・Issue #355）を提供する
//! - `GET /ai/plugins` の応答は `{"plugins":[<manifest>...]}`（登録順。未登録は空配列）の暫定形で、
//!   PoC-15 の `host-api.schema.json` 準拠。正式スキーマの文書化は TASK-92.5（Issue #357）。
//!   各マニフェストは `runtime`・`language` の申告値も含む（`PLUG-6`・TASK-98.1・Issue #389。未申告は `unspecified`）。
//!   一覧はプラグインの申告値をそのまま返すだけで「接続済み」「権限付与済み」を意味しない。
//!   ページング・絞り込みは持たない（上限 `MAX_PLUGINS` 件）
//! - `POST /ai/plugins/register` の応答形は暫定: 成功 `{"ok":true,"id":...}`（200）、失敗 `{"code","message"}`
//!   （マニフェスト不正 400・id 重複 409・満杯 429・Content-Type 不正 415・Origin 付き 403）。
//!   入力値（id・キー名・ヘッダ値）は応答に含めない。正式スキーマは TASK-92.5（Issue #357）
//! - 状態を変更する POST のため、cross-site からの登録を防ぐ目的で `Origin` ヘッダ付き要求を一律拒否し、
//!   `Content-Type: application/json` を必須とする（CORS 許可ヘッダは付けない）。許可リストは将来の設定項目
//! - 登録は申告値の保持のみで、プラグインプロセスの起動・接続・権限付与は行わない。
//!   成功は「接続済み」「権限付与済み」を意味しない
//!
//! # 資源上限
//!
//! パースは [`ParseOptions::default`] の入力サイズ・ノード数上限に従い、木の深さは
//! [`build_snapshot`] が `MAX_TREE_DEPTH` 以下に保証する。ハンドラは同期的にパースするため、
//! 巨大文書ではその間リクエスト処理を占有する（骨格段階では許容）。

use std::fmt;
use std::sync::Arc;

use fandhe_backend_http::response::Response;
use fandhe_backend_routes::Router;
use fandhe_browser_core::host::{Authority, HostError};
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use fandhe_browser_core::{AppState, NavigationState};
use serde_json::{Map, Value, json};

use crate::plugin_api::{AiState, ManifestError, PluginManifest, PluginRegistry, RegistryError};
use crate::snapshot::{CheckedState, DataLeafKind, Node, Snapshot, TableSummary, build_snapshot};

const JSON_CONTENT_TYPE: &str = "application/json; charset=UTF-8";

/// `/ai/snapshot` 処理の失敗。`Display` は固定の英語文言で、URL・HTML・内部エラーの
/// 詳細を含めない（情報漏えい防止）。応答本文の `message` に使われる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApiError {
    /// 直近のナビゲート結果がない。409 / `no_navigation` で返す（`AISNAP-14`・TASK-19.2）。
    NoNavigation,
    /// HTML のパースに失敗した。
    Parse,
    /// snapshot の構築に失敗した。
    Snapshot,
    /// JSON 化に失敗した。
    Serialize,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ApiError::NoNavigation => "no navigation has been performed yet",
            ApiError::Parse => "failed to parse document",
            ApiError::Snapshot => "failed to build snapshot",
            ApiError::Serialize => "failed to serialize snapshot",
        })
    }
}

impl std::error::Error for ApiError {}

/// `/ai/*` ルータを返す。cli が合成する（TASK-19.3）。内部で空のプラグインレジストリを持つ
/// [`AiState`] を構築し [`router_with_state`] へ委譲する（シグネチャは cli の `RouterFactory` 互換）。
pub fn router(app: Arc<AppState>) -> Router {
    router_with_state(Arc::new(AiState::new(app)))
}

/// ai 固有状態 [`AiState`] を使うルータを返す（`GET /ai/snapshot`・`GET /ai/plugins`・
/// `POST /ai/plugins/register`）。
///
/// snapshot・plugins は Host 検証のみ行う読み取り専用 GET で、CORS 許可ヘッダは付けない。
/// `/ai/plugins` は `PLUG-2`・TASK-92.4・Issue #356、register は TASK-92.3・Issue #355。
pub fn router_with_state(state: Arc<AiState>) -> Router {
    let snapshot_state = Arc::clone(&state);
    let plugins_state = Arc::clone(&state);
    let register_state = state;
    Router::new()
        .route("GET", "/ai/snapshot", move |head, _body| {
            // DNS rebinding 対策: cdp の `/json/*` と同じ Host 検証（core の `host` モジュール）を
            // 応答前に行い、不正な Host には snapshot（直近ページの URL・内容）を渡さない。
            if let Err(e) = Authority::from_host_header(head.header("host")) {
                return host_error_response(e);
            }
            match snapshot_body(snapshot_state.app().navigation()) {
                Ok(body) => Response::new(200, body).with_content_type(JSON_CONTENT_TYPE),
                Err(e) => error_response(e),
            }
        })
        .route("GET", "/ai/plugins", move |head, _body| {
            // snapshot と同じ Host 検証で DNS rebinding 経由の列挙を防ぐ。
            if let Err(e) = Authority::from_host_header(head.header("host")) {
                return host_error_response(e);
            }
            Response::new(200, plugins_body(plugins_state.plugins()))
                .with_content_type(JSON_CONTENT_TYPE)
        })
        .route("POST", "/ai/plugins/register", move |head, body| {
            // 検査順: Host -> Origin -> Content-Type -> 本文検証 -> 登録。前段で拒否した要求は
            // 本文をパースしない。
            if let Err(e) = Authority::from_host_header(head.header("host")) {
                return host_error_response(e);
            }
            if head.header("origin").is_some() {
                return fixed_error_response(403, "origin_not_allowed", "origin is not allowed");
            }
            if !is_json_content_type(head.header("content-type")) {
                return fixed_error_response(
                    415,
                    "unsupported_media_type",
                    "content type must be application/json",
                );
            }
            match register_plugin(register_state.plugins(), body) {
                Ok(result) => {
                    let body = json!({ "ok": true, "id": result.id() })
                        .to_string()
                        .into_bytes();
                    Response::new(200, body).with_content_type(JSON_CONTENT_TYPE)
                }
                Err(e) => register_error_response(e),
            }
        })
}

/// 登録済みプラグイン一覧の JSON 本文 `{"plugins":[...]}` を作る純粋部（`PLUG-2`・TASK-92.4）。
///
/// `GET /ai/plugins` ハンドラから呼ばれ、レジストリの登録順を保って各マニフェストを
/// [`PluginManifest::to_value`](crate::plugin_api::PluginManifest::to_value) で出力する。
/// `Value` の文字列化は失敗しないため `Result` にしない。
pub fn plugins_body(registry: &PluginRegistry) -> Vec<u8> {
    let plugins: Vec<Value> = registry.list().iter().map(|m| m.to_value()).collect();
    json!({ "plugins": plugins }).to_string().into_bytes()
}

/// プラグイン登録の失敗（`PLUG-2`・TASK-92.3・Issue #355）。マニフェスト検証とレジストリ登録の
/// 失敗を包み、[`register_error_response`] が HTTP ステータスへ写像する。`Display` は内包エラーの
/// 固定英語文言で、untrusted な入力値を含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegisterError {
    /// マニフェストがスキーマ制約に違反した。
    Manifest(ManifestError),
    /// レジストリが拒否した（重複・満杯）。
    Registry(RegistryError),
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(e) => e.fmt(f),
            Self::Registry(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for RegisterError {}

impl From<ManifestError> for RegisterError {
    fn from(e: ManifestError) -> Self {
        Self::Manifest(e)
    }
}

impl From<RegistryError> for RegisterError {
    fn from(e: RegistryError) -> Self {
        Self::Registry(e)
    }
}

/// プラグイン登録の成功結果（`PLUG-2`・TASK-92.3・Issue #355・REPAIR-4）。
/// 将来のフィールド追加に備え構造体で返す（`#[non_exhaustive]`）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RegisterResult {
    id: String,
}

impl RegisterResult {
    /// 登録したプラグインの id。
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// 本文をマニフェストとして検証しレジストリへ登録する純粋部。成功時は登録結果（id）を返す。
/// `Profile` に依存せず 3 OS でテストできる。登録は申告値の保持のみ（接続・権限付与はしない）。
pub fn register_plugin(
    registry: &PluginRegistry,
    body: &[u8],
) -> Result<RegisterResult, RegisterError> {
    let manifest = PluginManifest::from_slice(body)?;
    let id = manifest.id().to_owned();
    registry.register(manifest)?;
    Ok(RegisterResult { id })
}

/// 登録失敗を HTTP 応答へ写像する。マニフェスト不正は 400（`TooLarge` も PoC-15 契約に合わせ 400）、
/// id 重複 409、満杯 429。本文は `{"code","message"}` で入力値を含めない。
fn register_error_response(e: RegisterError) -> Response {
    let (status, code) = match e {
        RegisterError::Manifest(m) => (400, m.code()),
        RegisterError::Registry(RegistryError::DuplicateId) => {
            (409, RegistryError::DuplicateId.code())
        }
        RegisterError::Registry(RegistryError::Full) => (429, RegistryError::Full.code()),
    };
    fixed_error_response(status, code, &e.to_string())
}

/// `{"code","message"}` の固定内容エラー応答。
fn fixed_error_response(status: u16, code: &str, message: &str) -> Response {
    let body = json!({ "code": code, "message": message })
        .to_string()
        .into_bytes();
    Response::new(status, body).with_content_type(JSON_CONTENT_TYPE)
}

/// `Content-Type` が `application/json`（パラメータ・大文字小文字は不問）か。ヘッダ欠落は偽。
fn is_json_content_type(value: Option<&str>) -> bool {
    value
        .and_then(|v| v.split(';').next())
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
}

/// Host 検証エラーを 400（形式不正）/ 403（許可外ホスト）へ写像する。本体は固定コードのみで
/// ヘッダ値を含めない。
fn host_error_response(e: HostError) -> Response {
    let (status, code) = match e {
        HostError::NotAllowed => (403, "host_not_allowed"),
        _ => (400, "invalid_host"),
    };
    let body = json!({ "code": code }).to_string().into_bytes();
    Response::new(status, body).with_content_type(JSON_CONTENT_TYPE)
}

/// 固定文言のエラー応答 `{"code", "message"}`。`NoNavigation` は 409（200 で成功を装わない）、他は 500。
/// `message` は [`ApiError`] の `Display`（固定文言）で、リクエスト由来の値を含めない。
fn error_response(e: ApiError) -> Response {
    let (status, code) = match e {
        ApiError::NoNavigation => (409, "no_navigation"),
        ApiError::Parse => (500, "parse_failed"),
        ApiError::Snapshot => (500, "snapshot_failed"),
        ApiError::Serialize => (500, "serialize_failed"),
    };
    let body = json!({ "code": code, "message": e.to_string() })
        .to_string()
        .into_bytes();
    Response::new(status, body).with_content_type(JSON_CONTENT_TYPE)
}

/// 直近のナビゲート結果から JSON 本文を作る純粋部。`Profile` に依存せず 3 OS でテストできる。
pub fn snapshot_body(nav: &NavigationState) -> Result<Vec<u8>, ApiError> {
    let latest = nav.latest().ok_or(ApiError::NoNavigation)?;
    let parsed =
        parse_document(latest.html(), &ParseOptions::default()).map_err(|_| ApiError::Parse)?;
    let snapshot = build_snapshot(&parsed.document).map_err(|_| ApiError::Snapshot)?;
    let value = envelope(latest.url(), &snapshot);
    serde_json::to_vec(&value).map_err(|_| ApiError::Serialize)
}

/// `{"url", "tree", "truncated"}` のエンベロープを作る。
fn envelope(url: &str, snapshot: &Snapshot) -> Value {
    json!({
        "url": url,
        "tree": node_json(&snapshot.tree),
        "truncated": snapshot.truncated,
    })
}

/// [`Node`] を JSON にする。`#[non_exhaustive]` 型へフィールドが増えたら本関数の更新が必要。
///
/// 再帰の深さは [`build_snapshot`] が `MAX_TREE_DEPTH` 以下に保証するため有界。
/// 既定値のフィールド（`ref` なし（TASK-23.4）・`disabled: false`・`checked`/`data_leaf`/`table` なし・折り畳み行なし）は
/// トークン量削減のため省略する。
fn node_json(node: &Node) -> Value {
    let mut m = Map::new();
    m.insert("role".into(), json!(node.role));
    m.insert("name".into(), json!(node.name));
    if let Some(r) = &node.r#ref {
        m.insert("ref".into(), json!(r));
    }
    if node.state.disabled {
        m.insert("disabled".into(), json!(true));
    }
    if let Some(c) = node.state.checked {
        m.insert("checked".into(), json!(c == CheckedState::Checked));
    }
    if let Some(k) = node.data_leaf {
        m.insert("data_leaf".into(), json!(data_leaf_str(k)));
    }
    if let Some(t) = &node.table {
        table_into(&mut m, t);
    }
    if !node.folded_rows.is_empty() {
        let rows: Vec<Value> = node
            .folded_rows
            .iter()
            .map(|r| json!({"index": r.index, "text": r.text, "truncated": r.truncated}))
            .collect();
        m.insert("folded_rows".into(), Value::Array(rows));
    }
    m.insert(
        "children".into(),
        Value::Array(node.children.iter().map(node_json).collect()),
    );
    Value::Object(m)
}

fn data_leaf_str(k: DataLeafKind) -> &'static str {
    match k {
        DataLeafKind::TableCell => "table_cell",
        DataLeafKind::PriceClass => "price_class",
        // 拡充したデータ葉（TASK-15.1・15.2。統合は TASK-15.3・Issue #101・`AISNAP-11`）。
        DataLeafKind::ProseClass => "prose_class",
        DataLeafKind::Quote => "quote",
    }
}

/// 圧縮表の `header`・`rows`・`truncated_rows` をノードへ平坦化する。
fn table_into(m: &mut Map<String, Value>, t: &TableSummary) {
    let header: Vec<Value> = t
        .header
        .iter()
        .map(|h| {
            let mut hm = Map::new();
            hm.insert("role".into(), json!(h.role));
            hm.insert("name".into(), json!(h.name));
            hm.insert("ref".into(), json!(h.r#ref));
            if let Some(k) = h.data_leaf {
                hm.insert("data_leaf".into(), json!(data_leaf_str(k)));
            }
            Value::Object(hm)
        })
        .collect();
    let rows: Vec<Value> = t
        .rows
        .iter()
        .map(|r| {
            let controls: Vec<Value> = r
                .controls
                .iter()
                .map(|c| {
                    let mut cm = Map::new();
                    cm.insert("role".into(), json!(c.role));
                    cm.insert("name".into(), json!(c.name));
                    cm.insert("ref".into(), json!(c.r#ref));
                    if c.state.disabled {
                        cm.insert("disabled".into(), json!(true));
                    }
                    if let Some(ch) = c.state.checked {
                        cm.insert("checked".into(), json!(ch == CheckedState::Checked));
                    }
                    Value::Object(cm)
                })
                .collect();
            json!({
                "text": r.text,
                "truncated": r.truncated,
                "controls": controls,
                "controls_truncated": r.controls_truncated,
            })
        })
        .collect();
    m.insert("header".into(), Value::Array(header));
    m.insert("rows".into(), Value::Array(rows));
    m.insert("truncated_rows".into(), json!(t.truncated_rows));
}

#[cfg(test)]
mod tests {
    use super::*;
    use fandhe_browser_core::NavigationResult;

    fn nav_with(url: &str, html: &str) -> NavigationState {
        let nav = NavigationState::new();
        let g = nav.begin_navigation().expect("begin");
        nav.commit_navigation(g, NavigationResult::new(url, html))
            .expect("commit");
        nav
    }

    /// 木を深さ優先で走査し、`name` が一致する最初のノードを返す。
    fn find_by_name<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
        if v["name"] == name {
            return Some(v);
        }
        v["children"]
            .as_array()?
            .iter()
            .find_map(|c| find_by_name(c, name))
    }

    fn body_json(nav: &NavigationState) -> Value {
        serde_json::from_slice(&snapshot_body(nav).expect("body")).expect("json")
    }

    #[test]
    fn aisnap6_snapshot_body_has_url_tree_truncated() {
        let nav = nav_with(
            "https://example.com/",
            "<title>Example Domain</title><h1>Example Domain</h1><a href=\"/x\">More</a>",
        );
        let v = body_json(&nav);
        assert_eq!(v["url"], "https://example.com/");
        assert_eq!(v["truncated"], false);
        assert_eq!(v["tree"]["role"], "document");
        assert_eq!(v["tree"]["name"], "Example Domain");
        assert!(
            v["tree"]
                .as_object()
                .is_some_and(|o| !o.contains_key("ref"))
        );
        let link = find_by_name(&v["tree"], "More").expect("link");
        assert_eq!(link["role"], "link");
        assert_eq!(link["name"], "More");
        assert!(link["ref"].as_str().is_some_and(|r| r.starts_with('e')));
    }

    #[test]
    fn aisnap6_state_fields_are_emitted_only_when_set() {
        let nav = nav_with(
            "https://example.com/",
            "<button disabled>Cancel</button><input type=\"checkbox\" aria-label=\"Agree\" checked><button>Ok</button>",
        );
        let v = body_json(&nav);
        let cancel = find_by_name(&v["tree"], "Cancel").expect("cancel");
        assert_eq!(cancel["disabled"], true);
        let agree = find_by_name(&v["tree"], "Agree").expect("agree");
        assert_eq!(agree["checked"], true);
        let ok = find_by_name(&v["tree"], "Ok").expect("ok");
        assert!(ok.get("disabled").is_none());
        assert!(ok.get("checked").is_none());
    }

    #[test]
    fn aisnap6_data_leaf_and_compressed_table_are_emitted() {
        let mut rows = String::new();
        for i in 0..5 {
            rows.push_str(&format!("<tr><td>item{i}</td><td>{i}00</td></tr>"));
        }
        let nav = nav_with(
            "https://example.com/t",
            &format!(
                "<table><tr><th>Name</th><th>Price</th></tr>{rows}</table><span class=\"price\">9</span>"
            ),
        );
        let text = String::from_utf8(snapshot_body(&nav).expect("body")).expect("utf8");
        assert!(text.contains("\"truncated_rows\":0"), "{text}");
        assert!(text.contains("\"header\""), "{text}");
        assert!(text.contains("item3"), "{text}");
        assert!(text.contains("\"data_leaf\":\"price_class\""), "{text}");
    }

    /// 拡充したデータ葉（引用・地の文クラス）が ref 付きで `/ai/snapshot` の JSON へ出る
    /// （`AISNAP-6`・`AISNAP-11`・TASK-15.3・Issue #101）。
    #[test]
    fn aisnap6_expanded_data_leaves_are_emitted_with_refs() {
        let nav = nav_with(
            "https://example.com/q",
            "<blockquote><p>Quoted</p></blockquote><p><q>short</q></p><div><span class=\"text\">Prose body.</span></div>",
        );
        let text = String::from_utf8(snapshot_body(&nav).expect("body")).expect("utf8");
        assert_eq!(text.matches("\"data_leaf\":\"quote\"").count(), 2, "{text}");
        assert_eq!(
            text.matches("\"data_leaf\":\"prose_class\"").count(),
            1,
            "{text}"
        );
        let v: serde_json::Value = serde_json::from_str(&text).expect("json");
        let mut found = Vec::new();
        collect_leaves(&v["tree"], &mut found);
        assert_eq!(found.len(), 3, "{text}");
        for (kind, r) in found {
            assert!(!r.is_empty(), "{kind} has empty ref: {text}");
        }
    }

    /// `data_leaf` を持つノードの (種別, ref) を文書順に集める。
    fn collect_leaves(n: &serde_json::Value, out: &mut Vec<(String, String)>) {
        if let Some(k) = n.get("data_leaf").and_then(|x| x.as_str()) {
            let r = n
                .get("ref")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            out.push((k.to_string(), r));
        }
        if let Some(c) = n.get("children").and_then(|x| x.as_array()) {
            for ch in c {
                collect_leaves(ch, out);
            }
        }
    }

    #[test]
    fn aisnap6_about_blank_is_a_normal_snapshot() {
        let nav = nav_with("https://example.com/", "<h1>x</h1>");
        let g = nav.begin_navigation().expect("begin");
        nav.clear_navigation(g).expect("clear");
        let v = body_json(&nav);
        assert_eq!(v["url"], "about:blank");
        assert_eq!(v["tree"]["role"], "document");
    }

    fn error_json(e: ApiError) -> (u16, Value) {
        let res = error_response(e);
        let v: Value = serde_json::from_slice(&res.body).expect("json");
        (res.status, v)
    }

    #[test]
    fn aisnap14_no_navigation_is_409_with_code_and_message() {
        let nav = NavigationState::new();
        assert_eq!(snapshot_body(&nav), Err(ApiError::NoNavigation));
        let res = error_response(ApiError::NoNavigation);
        assert_eq!(res.status, 409);
        let v: Value = serde_json::from_slice(&res.body).expect("json");
        assert_eq!(v["code"], "no_navigation");
        assert_eq!(v["message"], "no navigation has been performed yet");
        assert_eq!(v.as_object().expect("object").len(), 2);
    }

    #[test]
    fn aisnap14_error_bodies_have_code_and_fixed_message() {
        for (e, status, code, message) in [
            (
                ApiError::Parse,
                500,
                "parse_failed",
                "failed to parse document",
            ),
            (
                ApiError::Snapshot,
                500,
                "snapshot_failed",
                "failed to build snapshot",
            ),
            (
                ApiError::Serialize,
                500,
                "serialize_failed",
                "failed to serialize snapshot",
            ),
        ] {
            let (s, v) = error_json(e);
            assert_eq!(s, status);
            assert_eq!(v["code"], code);
            assert_eq!(v["message"], message);
        }
    }

    fn registry_with(ids: &[&str]) -> PluginRegistry {
        let r = PluginRegistry::new();
        for id in ids {
            r.register(
                crate::plugin_api::PluginManifest::from_value(
                    &json!({"id": id, "version": "1.0.0", "transport": "stdio", "tools": ["t"]}),
                )
                .expect("manifest"),
            )
            .expect("register");
        }
        r
    }

    #[test]
    fn plug2_plugins_body_is_empty_array_when_unregistered() {
        let body = plugins_body(&PluginRegistry::new());
        assert_eq!(body, b"{\"plugins\":[]}");
    }

    #[test]
    fn plug2_plugins_body_lists_manifests_in_registration_order() {
        let r = PluginRegistry::new();
        r.register(
            crate::plugin_api::PluginManifest::from_value(&json!({
                "id": "b", "version": "2.0.0", "transport": "tcp", "tools": ["x", "y"],
                "permissions": ["network.fetch"], "protocolVersion": "1"
            }))
            .expect("m"),
        )
        .expect("reg");
        r.register(
            crate::plugin_api::PluginManifest::from_value(
                &json!({"id": "a", "version": "1.0.0", "transport": "stdio", "tools": ["t"]}),
            )
            .expect("m"),
        )
        .expect("reg");
        let v: Value = serde_json::from_slice(&plugins_body(&r)).expect("json");
        assert_eq!(
            v,
            json!({"plugins": [
                {"id": "b", "version": "2.0.0", "transport": "tcp", "tools": ["x", "y"],
                 "permissions": ["network.fetch"], "protocolVersion": "1", "runtime": "unspecified", "language": "unspecified"},
                {"id": "a", "version": "1.0.0", "transport": "stdio", "tools": ["t"],
                 "permissions": [], "protocolVersion": "unspecified", "runtime": "unspecified", "language": "unspecified"},
            ]})
        );
    }

    #[test]
    fn plug2_plugins_body_round_trips_through_manifest() {
        let r = registry_with(&["p1", "p2"]);
        let v: Value = serde_json::from_slice(&plugins_body(&r)).expect("json");
        let back: Vec<_> = v["plugins"]
            .as_array()
            .expect("array")
            .iter()
            .map(|e| crate::plugin_api::PluginManifest::from_value(e).expect("manifest"))
            .collect();
        assert_eq!(back, r.list());
    }

    // --- PLUG-2・TASK-92.3 ---

    const VALID: &str = r#"{"id":"mcp-ref","version":"0.1.0","transport":"stdio","tools":["t"]}"#;

    #[test]
    fn plug2_register_plugin_accepts_valid_manifest() {
        let reg = PluginRegistry::new();
        let result = register_plugin(&reg, VALID.as_bytes()).expect("register");
        assert_eq!(result.id(), "mcp-ref");
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.list()[0].id(), "mcp-ref");
    }

    #[test]
    fn plug2_register_plugin_rejects_invalid_manifests() {
        let reg = PluginRegistry::new();
        let big = vec![b' '; crate::plugin_api::MAX_MANIFEST_BYTES + 1];
        for (body, err) in [
            (&b"{"[..], ManifestError::InvalidJson),
            (
                &br#"{"id":"a","version":"1","transport":"stdio","tools":["t"],"x":1}"#[..],
                ManifestError::UnknownField,
            ),
            (&big[..], ManifestError::TooLarge),
        ] {
            assert_eq!(
                register_plugin(&reg, body),
                Err(RegisterError::Manifest(err))
            );
        }
        assert!(matches!(
            register_plugin(&reg, br#"{"id":"a"}"#),
            Err(RegisterError::Manifest(ManifestError::MissingField(_)))
        ));
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn plug2_register_plugin_rejects_duplicate_and_full() {
        let reg = PluginRegistry::new();
        register_plugin(&reg, VALID.as_bytes()).expect("first");
        assert_eq!(
            register_plugin(&reg, VALID.as_bytes()),
            Err(RegisterError::Registry(RegistryError::DuplicateId))
        );
        for i in 1..crate::plugin_api::MAX_PLUGINS {
            let b =
                format!(r#"{{"id":"p{i}","version":"0.1.0","transport":"stdio","tools":["t"]}}"#);
            register_plugin(&reg, b.as_bytes()).expect("fill");
        }
        let over = br#"{"id":"over","version":"0.1.0","transport":"stdio","tools":["t"]}"#;
        assert_eq!(
            register_plugin(&reg, over),
            Err(RegisterError::Registry(RegistryError::Full))
        );
    }

    #[test]
    fn plug2_register_error_response_maps_status_and_code() {
        for (e, status, code, message) in [
            (
                RegisterError::Manifest(ManifestError::UnknownField),
                400,
                "unknown_field",
                "manifest has an unknown field",
            ),
            (
                RegisterError::Manifest(ManifestError::TooLarge),
                400,
                "manifest_too_large",
                "manifest is too large",
            ),
            (
                RegisterError::Registry(RegistryError::DuplicateId),
                409,
                "duplicate_plugin_id",
                "plugin id is already registered",
            ),
            (
                RegisterError::Registry(RegistryError::Full),
                429,
                "plugin_registry_full",
                "plugin registry is full",
            ),
        ] {
            let res = register_error_response(e);
            assert_eq!(res.status, status);
            let v: Value = serde_json::from_slice(&res.body).expect("json");
            assert_eq!(v["code"], code);
            assert_eq!(v["message"], message);
            assert_eq!(v.as_object().expect("object").len(), 2);
        }
    }

    #[test]
    fn plug2_is_json_content_type() {
        assert!(is_json_content_type(Some("application/json")));
        assert!(is_json_content_type(Some(
            "Application/JSON; charset=utf-8"
        )));
        assert!(!is_json_content_type(Some("text/plain")));
        assert!(!is_json_content_type(Some("")));
        assert!(!is_json_content_type(None));
    }
}
