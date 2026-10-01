//! `fetch`・`parse`・`dom` への計装（`REPAIR-9`・`TASK-10.2.1`・Issue #549）の
//! 結合テスト。
//!
//! recorder は options / `Document` へ明示注入する方式のため、テストごとに
//! 専用の [`InMemoryRecorder`] を持たせ、並列実行しても記録が混ざらない。
//! 外部ネットワークには依存せず `127.0.0.1` のループバックだけを使う。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use fandhe_browser_core::js_stub::{JsStubOptions, execute_js_stub, execute_js_stub_with_options};
use fandhe_browser_core::selector::parse_selector_list;
use fandhe_browser_core::{
    FailureKind, FetchOptions, Fetcher, InMemoryRecorder, OperationKind, OperationOutcome,
    ParseOptions, parse_document, parse_document_bytes,
};
use fandhe_browser_core::{element_matches, query_selector, query_selector_all_str};

fn new_recorder() -> Arc<InMemoryRecorder> {
    Arc::new(InMemoryRecorder::with_capacity(16))
}

/// リクエストヘッダを読み捨てて 200 を 1 回返すループバックサーバーを起動する。
fn spawn_ok_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().expect("local_addr").port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                drain_request_head(&mut stream);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            });
        }
    });
    port
}

fn drain_request_head(stream: &mut TcpStream) {
    let mut buf = [0u8; 1024];
    let mut data = Vec::new();
    while let Ok(n) = stream.read(&mut buf) {
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if data.windows(4).any(|w| w == b"\r\n\r\n") || data.len() > 8192 {
            break;
        }
    }
}

/// `REPAIR-9`: fetch 成功は `Fetch` / `Success` として 1 件記録される。
#[tokio::test]
async fn repair_9_fetch_success_is_recorded() {
    let port = spawn_ok_server();
    let rec = new_recorder();
    let options = FetchOptions::new()
        .with_allow_private_network_access(true)
        .with_recorder(rec.clone());
    let fetcher = Fetcher::new(options).expect("Fetcher::new");

    let response = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect("200 を返すサーバーへの取得は成功する");
    assert_eq!(response.status(), 200);

    let records = rec.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].operation(), OperationKind::Fetch);
    assert_eq!(records[0].outcome(), OperationOutcome::Success);
}

/// `REPAIR-9`: fetch 失敗は `Failure { kind }` で 1 件記録され、URL は含まれない。
#[tokio::test]
async fn repair_9_fetch_failure_is_recorded_without_url() {
    let rec = new_recorder();
    let fetcher = Fetcher::new(FetchOptions::new().with_recorder(rec.clone())).expect("new");

    let result = fetcher.get("file:///etc/hosts").await;
    assert!(result.is_err());

    let records = rec.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].operation(), OperationKind::Fetch);
    assert_eq!(
        records[0].outcome(),
        OperationOutcome::Failure {
            kind: FailureKind::DisallowedScheme
        }
    );
    let line = records[0].to_json_line();
    assert!(!line.contains("/etc/hosts"), "unexpected line: {line}");
    assert!(!line.contains("file:"), "unexpected line: {line}");
}

/// `REPAIR-9`: parse 成功は `Parse` / `Success` として 1 件記録される。
#[test]
fn repair_9_parse_success_is_recorded() {
    let rec = new_recorder();
    let options = ParseOptions::default().with_recorder(rec.clone());
    parse_document("<p>hi</p>", &options).expect("parse");

    let records = rec.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].operation(), OperationKind::Parse);
    assert_eq!(records[0].outcome(), OperationOutcome::Success);
}

/// `REPAIR-9`: parse 失敗は `Failure { Parse }` として 1 件記録される。
#[test]
fn repair_9_parse_failure_is_recorded() {
    let rec = new_recorder();
    let options = ParseOptions::default()
        .with_max_nodes(0)
        .with_recorder(rec.clone());
    assert!(parse_document("<p>hi</p>", &options).is_err());

    let records = rec.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].operation(), OperationKind::Parse);
    assert_eq!(
        records[0].outcome(),
        OperationOutcome::Failure {
            kind: FailureKind::Parse
        }
    );
}

/// `REPAIR-9`: `parse_document_bytes` は成功・失敗とも二重記録せず 1 件だけ記録する。
#[test]
fn repair_9_parse_bytes_records_exactly_once() {
    let rec = new_recorder();
    let options = ParseOptions::default().with_recorder(rec.clone());

    parse_document_bytes(b"<p>hi</p>", &options).expect("parse bytes");
    assert_eq!(rec.records().len(), 1);

    assert!(parse_document_bytes(&[0xff, 0xfe], &options).is_err());
    let records = rec.records();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[1].outcome(),
        OperationOutcome::Failure {
            kind: FailureKind::Parse
        }
    );
    let parse = rec.counts(OperationKind::Parse);
    assert_eq!((parse.success, parse.failure), (1, 1));
}

/// `REPAIR-9`: parse 時の recorder が `Document` へ引き継がれ、`text_content` が
/// `Dom` / `Success` として記録される。
#[test]
fn repair_9_dom_text_content_is_recorded() {
    let rec = new_recorder();
    let options = ParseOptions::default().with_recorder(rec.clone());
    let parsed = parse_document("<p>hi</p>", &options).expect("parse");
    let doc = parsed.document;
    let p = doc
        .descendants(doc.root())
        .filter(|&id| doc.is_element(id))
        .last()
        .expect("<p> が存在する");
    let before = rec.counts(OperationKind::Dom).success;

    assert_eq!(doc.text_content(p), Some("hi".to_string()));

    let after = rec.counts(OperationKind::Dom);
    assert_eq!(after.success, before + 1);
    assert_eq!(after.failure, 0);
}

/// `REPAIR-9`: recorder なしでパースした文書へ `set_recorder` で後付けできる。
#[test]
fn repair_9_dom_set_recorder_after_parse() {
    let mut doc = parse_document("<p>hi</p>", &ParseOptions::default())
        .expect("parse")
        .document;
    let root = doc.root();
    assert_eq!(doc.text_content(root), None);

    let rec = new_recorder();
    doc.set_recorder(rec.clone());
    assert_eq!(doc.text_content(root), None);

    let dom = rec.counts(OperationKind::Dom);
    assert_eq!((dom.success, dom.failure), (1, 0));
}

/// `REPAIR-9`: recorder 未設定（既定）では何も記録されず、他の recorder にも漏れない。
#[test]
fn repair_9_no_recorder_records_nothing() {
    let rec = new_recorder();
    let doc = parse_document("<p>hi</p>", &ParseOptions::default())
        .expect("parse")
        .document;
    let _ = doc.text_content(doc.root());
    assert_eq!(rec.records().len(), 0);
    assert_eq!(rec.dropped(), 0);
}

fn doc_with_recorder(html: &str) -> (fandhe_browser_core::dom::Document, Arc<InMemoryRecorder>) {
    let mut doc = parse_document(html, &ParseOptions::default())
        .expect("parse")
        .document;
    let rec = new_recorder();
    doc.set_recorder(rec.clone());
    (doc, rec)
}

/// `REPAIR-9`: `element_matches` は `Query` / `Success` を 1 件記録する。
#[test]
fn repair_9_query_element_matches_is_recorded() {
    let (doc, rec) = doc_with_recorder("<p>hi</p>");
    let p = doc
        .descendants(doc.root())
        .find(|&id| doc.local_name(id) == Some("p"))
        .expect("<p>");
    let sel = parse_selector_list("p").expect("selector");
    assert!(element_matches(&doc, p, &sel).expect("ok"));
    let q = rec.counts(OperationKind::Query);
    assert_eq!((q.success, q.failure), (1, 0));
    assert_eq!(rec.records().len(), 1);
}

/// `REPAIR-9`: `query_selector` は `Query` / `Success` を 1 件記録する。
#[test]
fn repair_9_query_selector_is_recorded() {
    let (doc, rec) = doc_with_recorder("<p>hi</p>");
    let sel = parse_selector_list("p").expect("selector");
    assert!(
        query_selector(&doc, doc.root(), &sel)
            .expect("ok")
            .is_some()
    );
    let q = rec.counts(OperationKind::Query);
    assert_eq!((q.success, q.failure), (1, 0));
    assert_eq!(rec.records().len(), 1);
}

/// `REPAIR-9`: 成功する `_str` 呼び出しでも `Query` は 2 件でなく 1 件（二重記録の回帰）。
#[test]
fn repair_9_query_selector_all_str_records_exactly_once() {
    let (doc, rec) = doc_with_recorder("<ul><li>a</li><li>b</li></ul>");
    let found = query_selector_all_str(&doc, doc.root(), "li").expect("ok");
    assert_eq!(found.len(), 2);
    let q = rec.counts(OperationKind::Query);
    assert_eq!((q.success, q.failure), (1, 0));
    assert_eq!(rec.records().len(), 1);
}

/// `REPAIR-9`: 未対応 selector は `Failure { Unsupported }` として 1 件記録される。
#[test]
fn repair_9_query_unsupported_selector_is_recorded() {
    let (doc, rec) = doc_with_recorder("<a>x</a>");
    let _ = query_selector_all_str(&doc, doc.root(), "a:hover").expect_err("unsupported");
    let records = rec.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].operation(), OperationKind::Query);
    assert_eq!(
        records[0].outcome(),
        OperationOutcome::Failure {
            kind: FailureKind::Unsupported
        }
    );
}

/// `REPAIR-9`: parse の recorder は `Document` 経由で query にも引き継がれる。
#[test]
fn repair_9_query_via_parse_recorder() {
    let rec = new_recorder();
    let options = ParseOptions::default().with_recorder(rec.clone());
    let doc = parse_document("<p>hi</p>", &options)
        .expect("parse")
        .document;
    let _ = query_selector_all_str(&doc, doc.root(), "p").expect("ok");
    let parse = rec.counts(OperationKind::Parse);
    let query = rec.counts(OperationKind::Query);
    assert_eq!((parse.success, parse.failure), (1, 0));
    assert_eq!((query.success, query.failure), (1, 0));
}

/// `REPAIR-9`: js_stub は `JsStub` / `Failure { JsExecutionUnavailable }` として記録される。
#[test]
fn repair_9_js_stub_failure_is_recorded() {
    let rec = new_recorder();
    let options = JsStubOptions::new().with_recorder(rec.clone());
    let _ = execute_js_stub_with_options("1 + 1", &options).expect_err("stub");
    let records = rec.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].operation(), OperationKind::JsStub);
    assert_eq!(
        records[0].outcome(),
        OperationOutcome::Failure {
            kind: FailureKind::JsExecutionUnavailable
        }
    );
}

/// `REPAIR-9`: recorder 未設定の query・js_stub は何も記録しない。
#[test]
fn repair_9_query_and_js_stub_without_recorder_record_nothing() {
    let rec = new_recorder();
    let doc = parse_document("<p>hi</p>", &ParseOptions::default())
        .expect("parse")
        .document;
    let _ = query_selector_all_str(&doc, doc.root(), "p").expect("ok");
    let _ = execute_js_stub("1");
    let _ = execute_js_stub_with_options("1", &JsStubOptions::default());
    assert_eq!(rec.records().len(), 0);
}
