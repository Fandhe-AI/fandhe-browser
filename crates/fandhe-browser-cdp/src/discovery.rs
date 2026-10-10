//! `/json/*` ディスカバリ応答の本体生成と Host ヘッダ検証（TASK-41（41.3）・#172、`CDP-1`・MS-3）。
//!
//! `crate::server::router` のハンドラから呼ばれる純関数群で、`CdpState` にも HTTP の型にも
//! 依存しない（3 OS で単体テストできる）。JSON は `serde_json` で組み立て、文字列連結はしない。
//!
//! # Host 検証（DNS rebinding 対策）
//!
//! `webSocketDebuggerUrl` の authority はリクエストの `Host` ヘッダから作る（Chromium と同方式。
//! cli が port 0 で bind してもルータ構築後に決まるアドレスへ追従できる）。ホストは `localhost`
//! か IP リテラルに限り、それ以外（= DNS rebinding で到達した任意ホスト名）は拒否する。
//! 生のヘッダ文字列は応答へ出さず、parse した値から authority を組み立て直す。
//! プロセス全体のアクセス制御（loopback 限定バインド）は TASK-41.5・`SEC-4` の責務で、cli の `server` モジュール（`ensure_loopback`）で実装済み。
//!
//! # スタブについて
//!
//! `/json/version` の `webSocketDebuggerUrl` は `/devtools/browser/{id}` 受け口
//! （TASK-41.4。[`crate::ws`]）と対で返す。
//! `/json/list` の各要素はページごとの `webSocketDebuggerUrl`・`devtoolsFrontendUrl` を持たない。
//! `/devtools/page/{id}` 受け口が未実装のため存在しない URL を出さない（REPAIR-3）。
//! 受け口を実装するタスク（TASK-42 以降・`CDP-1`）で追加する。`title` も `TargetInfo` に
//! タイトルが無いため URL で代用する（Chromium もタイトル無しのページは URL を表示する）。
//! `/json/version` は実際に動くエンジンの値だけを返し、`V8-Version`・`WebKit-Version` は
//! 偽値を返さないため出さない（`security.md` の偽装禁止）。

use serde_json::{Value, json};

pub(crate) use fandhe_browser_core::host::{Authority, HostError};

use crate::target::{BrowserId, TargetInfo};

/// ブラウザ WS 受け口のパスパターン（`WebSocketConfig::with_path_pattern` に渡す。
/// `{id}` が [`BrowserId`]）。`webSocketDebuggerUrl` と受け口で同じ定義を共有する。
pub(crate) const BROWSER_WS_PATH_PATTERN: &str = "/devtools/browser/{id}";

/// `webSocketDebuggerUrl` のパス接頭辞（[`BROWSER_WS_PATH_PATTERN`] の `{id}` 直前まで）。
const BROWSER_WS_PATH_PREFIX: &str = "/devtools/browser/";

/// `Protocol-Version`・`Browser.getVersion` の `protocolVersion` に返す CDP のバージョン。
pub(crate) const PROTOCOL_VERSION: &str = "1.3";

/// 製品名と版（`fandhe-browser/<版>`）。`/json/version` の `Browser`・`User-Agent` と
/// `Browser.getVersion`（[`crate::playwright_compat`]）で共有し、版表記のずれを防ぐ（`CDP-2`・`SEC-2`）。
/// Chrome / Chromium を装う値は含めない。
pub(crate) const PRODUCT: &str = concat!("fandhe-browser/", env!("CARGO_PKG_VERSION"));

/// `/json/version` の本体（JSON）を作る。
///
/// `webSocketDebuggerUrl` は検証・再構築済みの `authority` と、文字種が制限された
/// `browser_id` からのみ組み立てる（生の Host ヘッダは使わない。`CDP-1`）。
/// `V8-Version`・`WebKit-Version` は偽値を返さないため出さない。
pub(crate) fn version_body(
    authority: &Authority,
    browser_id: &BrowserId,
) -> Result<Vec<u8>, serde_json::Error> {
    let body = json!({
        "Browser": PRODUCT,
        "Protocol-Version": PROTOCOL_VERSION,
        "User-Agent": PRODUCT,
        "webSocketDebuggerUrl": format!(
            "ws://{}{}{}",
            authority.as_str(),
            BROWSER_WS_PATH_PREFIX,
            browser_id.as_str()
        ),
    });
    serde_json::to_vec(&body)
}

/// `/json/list`・`/json` の本体（ターゲット配列の JSON）を作る。
pub(crate) fn list_body(targets: &[TargetInfo]) -> Result<Vec<u8>, serde_json::Error> {
    let items: Vec<Value> = targets
        .iter()
        .map(|t| {
            json!({
                "id": t.target_id().as_str(),
                "type": t.kind().as_str(),
                "url": t.url(),
                "title": t.url(),
                "description": "",
            })
        })
        .collect();
    serde_json::to_vec(&items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{TargetKind, TargetRegistry};

    fn version(host: &str, id: &str) -> Value {
        let a = Authority::from_host_header(Some(host)).unwrap();
        let b = BrowserId::parse(id).unwrap();
        serde_json::from_slice(&version_body(&a, &b).unwrap()).unwrap()
    }

    #[test]
    fn cdp1_version_body_has_real_values_only() {
        let v = version("127.0.0.1:9222", "fixed-1");
        let product = format!("fandhe-browser/{}", env!("CARGO_PKG_VERSION"));
        assert_eq!(v["Browser"], product.as_str());
        assert_eq!(v["User-Agent"], product.as_str());
        assert_eq!(v["Protocol-Version"], "1.3");
        assert_eq!(
            v["webSocketDebuggerUrl"],
            "ws://127.0.0.1:9222/devtools/browser/fixed-1"
        );
        assert!(v.get("V8-Version").is_none());
        assert!(v.get("WebKit-Version").is_none());
    }

    #[test]
    fn cdp1_version_ws_url_follows_authority_form() {
        assert_eq!(
            version("[::1]:9222", "b-2")["webSocketDebuggerUrl"],
            "ws://[::1]:9222/devtools/browser/b-2"
        );
        assert_eq!(
            version("LOCALHOST:1234", "b-2")["webSocketDebuggerUrl"],
            "ws://localhost:1234/devtools/browser/b-2"
        );
    }

    #[test]
    fn cdp1_list_body_empty_and_ordered() {
        let empty: Value = serde_json::from_slice(&list_body(&[]).unwrap()).unwrap();
        assert_eq!(empty, json!([]));

        let r = TargetRegistry::new();
        let a = r
            .create_target(TargetKind::Page, "https://a.test/")
            .unwrap();
        let b = r
            .create_target(TargetKind::Page, "https://b.test/")
            .unwrap();
        let v: Value = serde_json::from_slice(&list_body(&r.targets()).unwrap()).unwrap();
        assert_eq!(
            v,
            json!([
                {"id": a.as_str(), "type": "page", "url": "https://a.test/",
                 "title": "https://a.test/", "description": ""},
                {"id": b.as_str(), "type": "page", "url": "https://b.test/",
                 "title": "https://b.test/", "description": ""},
            ])
        );
        assert!(v[0].get("webSocketDebuggerUrl").is_none());
    }
}
