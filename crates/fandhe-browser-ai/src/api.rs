//! `/ai/*` HTTP API の入口（`AISNAP-6`・TASK-19.1・Issue #223・`MS-4`）。
//!
//! core の [`AppState`] が保持する直近ナビゲート結果から [`Snapshot`]
//! （`AISNAP-1` 方式 B）を構築し、JSON で返す `GET /ai/snapshot` ルータを提供する。
//! [`router`] は bind・アクセス制御（loopback 限定。`SEC-4`）を行わないが、DNS rebinding 対策として
//! `Host` ヘッダ（localhost / IP リテラルのみ許可）を cdp と共通の core 実装で検証する。cli が
//! `RouterFactory` として受け取り、cdp のルータと `Router::merge` で合成する想定
//! （TASK-19.3・Issue #225）。cdp と ai は互いに依存せず、`Arc<AppState>` だけを共有する。
//!
//! # スタブ・暫定仕様について（REPAIR-3）
//!
//! - JSON の形は spec `api-ai-snapshot.md` の想定スキーマ（検討中）に寄せた暫定形で、
//!   確定スキーマではない。エンベロープは `{"url", "tree", "truncated"}`。`url` は
//!   `NavigationResult::url` の値で、[`Snapshot`] 型には持たせない
//! - 未ナビゲート時は 409 と固定 JSON を返す（成功を装わない）。エラー本文の確定形と
//!   テストは TASK-19.2（Issue #224・`AISNAP-14`）が担う
//! - 共有状態を跨ぐ結合テストは TASK-19.4（Issue #226）が担う
//! - 将来プラグインレジストリ（`PLUG-2`・TASK-92）を持つ ai 固有の状態型へ
//!   `Arc<AppState>` を内包する形で差し替える余地がある
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

use crate::snapshot::{CheckedState, DataLeafKind, Node, Snapshot, TableSummary, build_snapshot};

const JSON_CONTENT_TYPE: &str = "application/json; charset=UTF-8";

/// `/ai/snapshot` 処理の失敗。`Display` は固定の英語文言で、URL・HTML・内部エラーの
/// 詳細を含めない（情報漏えい防止）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApiError {
    /// 直近のナビゲート結果がない（`AISNAP-14`。確定形は TASK-19.2）。
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
            ApiError::NoNavigation => "no navigation",
            ApiError::Parse => "failed to parse document",
            ApiError::Snapshot => "failed to build snapshot",
            ApiError::Serialize => "failed to serialize snapshot",
        })
    }
}

impl std::error::Error for ApiError {}

/// `GET /ai/snapshot` を登録したルータを返す。cli が合成する（TASK-19.3）。
pub fn router(app: Arc<AppState>) -> Router {
    Router::new().route("GET", "/ai/snapshot", move |head, _body| {
        // DNS rebinding 対策: cdp の `/json/*` と同じ Host 検証（core の `host` モジュール）を
        // 応答前に行い、不正な Host には snapshot（直近ページの URL・内容）を渡さない。
        if let Err(e) = Authority::from_host_header(head.header("host")) {
            return host_error_response(e);
        }
        match snapshot_body(app.navigation()) {
            Ok(body) => Response::new(200, body).with_content_type(JSON_CONTENT_TYPE),
            Err(e) => error_response(e),
        }
    })
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

/// 固定文言のエラー応答。`NoNavigation` は 409（200 で成功を装わない）、他は 500。
fn error_response(e: ApiError) -> Response {
    let (status, code) = match e {
        ApiError::NoNavigation => (409, "no_navigation"),
        ApiError::Parse => (500, "parse_failed"),
        ApiError::Snapshot => (500, "snapshot_failed"),
        ApiError::Serialize => (500, "serialize_failed"),
    };
    let body = json!({ "code": code }).to_string().into_bytes();
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
/// 既定値のフィールド（`disabled: false`・`checked`/`data_leaf`/`table` なし・折り畳み行なし）は
/// トークン量削減のため省略する。
fn node_json(node: &Node) -> Value {
    let mut m = Map::new();
    m.insert("role".into(), json!(node.role));
    m.insert("name".into(), json!(node.name));
    m.insert("ref".into(), json!(node.r#ref));
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
        assert!(v["tree"]["ref"].is_null());
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

    #[test]
    fn aisnap6_about_blank_is_a_normal_snapshot() {
        let nav = nav_with("https://example.com/", "<h1>x</h1>");
        let g = nav.begin_navigation().expect("begin");
        nav.clear_navigation(g).expect("clear");
        let v = body_json(&nav);
        assert_eq!(v["url"], "about:blank");
        assert_eq!(v["tree"]["role"], "document");
    }

    #[test]
    fn aisnap6_no_navigation_is_an_error_not_a_fake_success() {
        let nav = NavigationState::new();
        assert_eq!(snapshot_body(&nav), Err(ApiError::NoNavigation));
        let res = error_response(ApiError::NoNavigation);
        assert_eq!(res.status, 409);
        assert_eq!(res.body, br#"{"code":"no_navigation"}"#.to_vec());
    }

    #[test]
    fn aisnap6_error_display_is_fixed_text() {
        assert_eq!(ApiError::Parse.to_string(), "failed to parse document");
        assert_eq!(error_response(ApiError::Parse).status, 500);
    }
}
