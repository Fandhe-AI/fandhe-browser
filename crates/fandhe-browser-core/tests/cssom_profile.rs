//! `cssom_profile` の結合テスト（TASK-100.2・Issue #266・`PLUG-8`）。

use fandhe_browser_core::cssom_profile::{
    BrowserProfile, BrowserProfileParseError, PropertySupport, is_property_supported, load_profile,
};

#[test]
fn plug8_from_str_accepts_chrome_and_safari() {
    assert_eq!(
        "chrome".parse::<BrowserProfile>(),
        Ok(BrowserProfile::Chrome)
    );
    assert_eq!(
        "safari".parse::<BrowserProfile>(),
        Ok(BrowserProfile::Safari)
    );
}

#[test]
fn plug8_from_str_rejects_others() {
    for s in ["firefox", "", "Chrome", " chrome", "chrome\n"] {
        assert_eq!(
            s.parse::<BrowserProfile>(),
            Err(BrowserProfileParseError::Unknown {
                value: s.to_string()
            })
        );
    }
}

#[test]
fn plug8_from_str_truncates_long_value_on_char_boundary() {
    let long = "あ".repeat(100);
    let Err(BrowserProfileParseError::Unknown { value }) = long.parse::<BrowserProfile>() else {
        panic!("expected error");
    };
    assert_eq!(value.len(), 63);
    assert!(long.starts_with(&value));
}

#[test]
fn plug8_display_roundtrip_and_error_message() {
    for p in BrowserProfile::ALL {
        assert_eq!(p.to_string().parse::<BrowserProfile>(), Ok(p));
    }
    let e = "x".parse::<BrowserProfile>().unwrap_err();
    assert_eq!(
        e.to_string(),
        "unknown browser profile 'x' (expected chrome or safari)"
    );
}

#[test]
fn plug8_embedded_profiles_load() {
    for p in BrowserProfile::ALL {
        let t = load_profile(p).expect("embedded data loads");
        assert_eq!(t.profile(), p);
        assert_eq!(t.feature_count(), 112);
        assert_eq!(t.property_count(), 261);
        assert!(std::ptr::eq(t, load_profile(p).unwrap()));
    }
}

#[test]
fn plug8_chrome_support() {
    let t = load_profile(BrowserProfile::Chrome).unwrap();
    for p in ["hanging-punctuation", "speak"] {
        assert_eq!(t.property_support(p), PropertySupport::Unsupported, "{p}");
    }
    for p in [
        "text-size-adjust",
        "interpolate-size",
        "user-select",
        "position-area",
    ] {
        assert_eq!(t.property_support(p), PropertySupport::Supported, "{p}");
    }
}

#[test]
fn plug8_safari_support() {
    let t = load_profile(BrowserProfile::Safari).unwrap();
    for p in ["text-size-adjust", "interpolate-size", "speak"] {
        assert_eq!(t.property_support(p), PropertySupport::Unsupported, "{p}");
    }
    for p in ["hanging-punctuation", "user-select"] {
        assert_eq!(t.property_support(p), PropertySupport::Supported, "{p}");
    }
}

#[test]
fn plug8_unlisted_properties_pass_through() {
    for profile in BrowserProfile::ALL {
        let t = load_profile(profile).unwrap();
        for p in ["width", "margin", "--custom-prop", "no-such-property", ""] {
            assert_eq!(t.property_support(p), PropertySupport::Unlisted, "{p}");
            assert!(t.is_supported(p));
        }
    }
}

#[test]
fn plug8_is_property_supported_examples() {
    assert_eq!(
        is_property_supported(BrowserProfile::Chrome, "speak"),
        Ok(false)
    );
    assert_eq!(
        is_property_supported(BrowserProfile::Chrome, "width"),
        Ok(true)
    );
    assert_eq!(
        is_property_supported(BrowserProfile::Safari, "speak"),
        Ok(false)
    );
}

// ---- TASK-100.3（Issue #267）: 公開入口 ----

use fandhe_browser_core::{Error, ProfileGate, profile_gate, profile_gate_from_name};

#[test]
fn plug8_gate_none_is_disabled_passthrough() {
    let gate = profile_gate(None).expect("None は常に成功する");
    assert!(!gate.is_enabled());
    assert_eq!(gate.profile(), None);
    assert_eq!(gate.property_support("speak"), PropertySupport::Unlisted);
    assert!(gate.allows_property("speak"));
}

#[test]
fn plug8_gate_some_reports_profile_and_matches_table() {
    for p in BrowserProfile::ALL {
        let gate = profile_gate(Some(p)).expect("埋め込みデータは有効");
        assert!(gate.is_enabled());
        assert_eq!(gate.profile(), Some(p));
        let table = load_profile(p).expect("埋め込みデータは有効");
        for prop in ["speak", "hanging-punctuation", "text-size-adjust", "width"] {
            assert_eq!(gate.property_support(prop), table.property_support(prop));
        }
    }
}

#[test]
fn plug8_gate_chrome_and_safari_values() {
    let chrome = profile_gate(Some(BrowserProfile::Chrome)).expect("ok");
    assert_eq!(
        chrome.property_support("speak"),
        PropertySupport::Unsupported
    );
    assert_eq!(
        chrome.property_support("hanging-punctuation"),
        PropertySupport::Unsupported
    );
    assert_eq!(
        chrome.property_support("text-size-adjust"),
        PropertySupport::Supported
    );
    assert_eq!(chrome.property_support("width"), PropertySupport::Unlisted);
    assert!(!chrome.allows_property("speak"));

    let safari = profile_gate(Some(BrowserProfile::Safari)).expect("ok");
    assert_eq!(
        safari.property_support("text-size-adjust"),
        PropertySupport::Unsupported
    );
    assert_eq!(
        safari.property_support("speak"),
        PropertySupport::Unsupported
    );
    assert_eq!(
        safari.property_support("hanging-punctuation"),
        PropertySupport::Supported
    );
}

#[test]
fn plug8_gate_is_copy() {
    let gate: ProfileGate = profile_gate(Some(BrowserProfile::Chrome)).expect("ok");
    let a = gate;
    let b = gate;
    assert_eq!(a.profile(), b.profile());
}

#[test]
fn plug8_gate_from_name_valid() {
    assert_eq!(
        profile_gate_from_name(Some("chrome"))
            .expect("ok")
            .profile(),
        Some(BrowserProfile::Chrome)
    );
    assert_eq!(
        profile_gate_from_name(Some("safari"))
            .expect("ok")
            .profile(),
        Some(BrowserProfile::Safari)
    );
    assert!(!profile_gate_from_name(None).expect("ok").is_enabled());
}

#[test]
fn plug8_gate_from_name_rejects_unknown() {
    for s in ["firefox", "Chrome", ""] {
        let err = profile_gate_from_name(Some(s)).expect_err("未知名は失敗する");
        assert!(matches!(
            err,
            Error::BrowserProfileName(BrowserProfileParseError::Unknown { ref value }) if value == s
        ));
    }
    let err = profile_gate_from_name(Some("firefox")).expect_err("失敗する");
    assert_eq!(
        err.to_string(),
        "invalid browser profile: unknown browser profile 'firefox' (expected chrome or safari)"
    );
}

#[test]
fn plug8_gate_from_name_truncates_long_value() {
    let long = "x".repeat(100);
    match profile_gate_from_name(Some(&long)) {
        Err(Error::BrowserProfileName(BrowserProfileParseError::Unknown { value })) => {
            assert_eq!(value.len(), 64);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// ---- TASK-100.4（Issue #268）: gating ----

mod gating {
    use fandhe_browser_core::cssom::{
        DeclarationOrigin, MAX_DECLARATION_INPUT_BYTES, collect_document_styles,
        computed_style_in_document,
    };
    use fandhe_browser_core::{
        BrowserProfile, ComputedDeclaration, ComputedStyle, ParseOptions, parse_document,
        profile_gate,
    };

    const HTML: &str = r#"<p style="speak:none;hanging-punctuation:first;text-size-adjust:none;width:1px;--custom-prop:1">x</p>"#;

    fn style_of(html: &str) -> ComputedStyle {
        let doc = parse_document(html, &ParseOptions::default())
            .expect("parse")
            .document;
        let styles = collect_document_styles(&doc).expect("collect");
        let p = doc
            .descendants(doc.root())
            .find(|&i| doc.local_name(i) == Some("p"))
            .expect("p");
        computed_style_in_document(&doc, p, &styles).expect("computed")
    }

    fn names(ds: &[ComputedDeclaration]) -> Vec<&str> {
        ds.iter().map(|d| d.property()).collect()
    }

    #[test]
    fn plug8_gating_disabled_keeps_all_declarations() {
        let style = style_of(HTML);
        let g = profile_gate(None).expect("ok").apply(&style);
        assert_eq!(g.profile(), None);
        assert_eq!(g.declarations(), style.declarations());
        assert!(g.removed().is_empty());
    }

    #[test]
    fn plug8_gating_chrome_removes_only_unsupported() {
        let style = style_of(HTML);
        let g = profile_gate(Some(BrowserProfile::Chrome))
            .expect("ok")
            .apply(&style);
        assert_eq!(
            names(g.declarations()),
            ["--custom-prop", "text-size-adjust", "width"]
        );
        assert_eq!(names(g.removed()), ["hanging-punctuation", "speak"]);
        let w = g.get("width").expect("width");
        assert_eq!(w.value(), style.get("width").expect("w").value());
        assert_eq!(w.origin(), DeclarationOrigin::Inline);
    }

    #[test]
    fn plug8_gating_safari_removes_only_unsupported() {
        let style = style_of(HTML);
        let g = profile_gate(Some(BrowserProfile::Safari))
            .expect("ok")
            .apply(&style);
        assert_eq!(
            names(g.declarations()),
            ["--custom-prop", "hanging-punctuation", "width"]
        );
        assert_eq!(names(g.removed()), ["speak", "text-size-adjust"]);
    }

    #[test]
    fn plug8_gating_preserves_important_and_retained_only_lookup() {
        let style = style_of(r#"<p style="width:1px !important;speak:none">x</p>"#);
        let g = profile_gate(Some(BrowserProfile::Chrome))
            .expect("ok")
            .apply(&style);
        assert_eq!(g.len(), 1);
        assert_eq!(
            g.get("width").expect("width").importance(),
            style.get("width").expect("w").importance()
        );
        assert!(g.get("speak").is_none());
    }

    #[test]
    fn plug8_gating_empty_style_is_empty() {
        let g = profile_gate(Some(BrowserProfile::Chrome))
            .expect("ok")
            .apply(&ComputedStyle::default());
        assert!(g.is_empty());
        assert!(g.removed().is_empty());
        assert!(!g.inline_skipped());
    }

    #[test]
    fn plug8_gating_passes_through_inline_skipped() {
        let big = "a".repeat(MAX_DECLARATION_INPUT_BYTES + 1);
        let style = style_of(&format!(r#"<p style="color:{big}">x</p>"#));
        assert!(style.inline_skipped());
        let g = profile_gate(Some(BrowserProfile::Chrome))
            .expect("ok")
            .apply(&style);
        assert!(g.inline_skipped());
    }
}

// ---- TASK-100.5（Issue #269）: Chrome / Safari 差分プロパティの除去 ----

/// Chrome と Safari でプロパティ単位の対応可否が分かれるものを使い、
/// gating（`ProfileGate::apply`）が双方向に働くことを具体値で固定する（`PLUG-8`・MS-8）。
///
/// 埋め込みデータ上の実際の差分は 3 件のみ（Chrome のみ: `interpolate-size`・
/// `text-size-adjust`、Safari のみ: `hanging-punctuation`）。Issue 例示の
/// `user-select`・`position-area` は `profiles/README.md` の手動補正で両対応のため
/// 差分ではなく「どちらでも除去されない」側として検証する。
mod diff {
    use fandhe_browser_core::cssom::{
        DeclarationOrigin, collect_document_styles, computed_style_in_document,
    };
    use fandhe_browser_core::cssom_profile::{PropertySupport, load_profile};
    use fandhe_browser_core::{
        BrowserProfile, ComputedStyle, ParseOptions, parse_document, profile_gate,
    };

    const CHROME_JSON: &str = include_str!("../../../profiles/chrome.json");
    const SAFARI_JSON: &str = include_str!("../../../profiles/safari.json");

    const CHROME_ONLY: [&str; 2] = ["interpolate-size", "text-size-adjust"];
    const SAFARI_ONLY: [&str; 1] = ["hanging-punctuation"];

    const HTML: &str = r#"<style>p { text-size-adjust: 100%; interpolate-size: allow-keywords; hanging-punctuation: first; user-select: none; position-area: top; width: 10px }</style><p>x</p>"#;

    fn style_of(html: &str) -> ComputedStyle {
        let doc = parse_document(html, &ParseOptions::default())
            .expect("parse")
            .document;
        let styles = collect_document_styles(&doc).expect("collect");
        let p = doc
            .descendants(doc.root())
            .find(|&i| doc.local_name(i) == Some("p"))
            .expect("p");
        computed_style_in_document(&doc, p, &styles).expect("computed")
    }

    fn removed_pairs(p: BrowserProfile, style: &ComputedStyle) -> Vec<(String, String)> {
        profile_gate(Some(p))
            .expect("gate")
            .apply(style)
            .removed()
            .iter()
            .map(|d| (d.property().to_string(), d.value().to_string()))
            .collect()
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn plug8_diff_safari_removes_chrome_only_properties() {
        let style = style_of(HTML);
        assert_eq!(
            removed_pairs(BrowserProfile::Safari, &style),
            [
                pair("interpolate-size", "allow-keywords"),
                pair("text-size-adjust", "100%")
            ]
        );
        let s = profile_gate(Some(BrowserProfile::Safari))
            .expect("gate")
            .apply(&style);
        assert!(s.get("interpolate-size").is_none());
        assert!(s.get("text-size-adjust").is_none());
        // 対照: Chrome では同じ 2 件が値つきで残る。
        let c = profile_gate(Some(BrowserProfile::Chrome))
            .expect("gate")
            .apply(&style);
        assert_eq!(
            c.get("interpolate-size").expect("kept").value(),
            "allow-keywords"
        );
        assert_eq!(c.get("text-size-adjust").expect("kept").value(), "100%");
    }

    #[test]
    fn plug8_diff_chrome_removes_safari_only_property() {
        let style = style_of(HTML);
        assert_eq!(
            removed_pairs(BrowserProfile::Chrome, &style),
            [pair("hanging-punctuation", "first")]
        );
        let c = profile_gate(Some(BrowserProfile::Chrome))
            .expect("gate")
            .apply(&style);
        assert!(c.get("hanging-punctuation").is_none());
        let s = profile_gate(Some(BrowserProfile::Safari))
            .expect("gate")
            .apply(&style);
        assert_eq!(s.get("hanging-punctuation").expect("kept").value(), "first");
    }

    #[test]
    fn plug8_diff_common_and_unlisted_survive_both() {
        let style = style_of(HTML);
        for p in [BrowserProfile::Chrome, BrowserProfile::Safari] {
            let g = profile_gate(Some(p)).expect("gate").apply(&style);
            assert_eq!(g.get("user-select").expect("us").value(), "none");
            assert_eq!(g.get("position-area").expect("pa").value(), "top");
            assert_eq!(g.get("width").expect("w").value(), "10px");
            assert_eq!(g.len() + g.removed().len(), style.len());
        }
    }

    #[test]
    fn plug8_diff_keeps_origin_and_ignores_importance() {
        let style = style_of(&HTML.replace(
            "<p>",
            r#"<p style="text-size-adjust: none !important; color: red !important">"#,
        ));
        let g = profile_gate(Some(BrowserProfile::Safari))
            .expect("gate")
            .apply(&style);
        let removed = g
            .removed()
            .iter()
            .find(|d| d.property() == "text-size-adjust")
            .expect("removed");
        assert_eq!(removed.value(), "none");
        assert_eq!(removed.origin(), DeclarationOrigin::Inline);
        assert_eq!(
            removed.importance(),
            style.get("text-size-adjust").expect("orig").importance()
        );
        // 残存側は由来・重要度が元のまま。
        let color = g.get("color").expect("color");
        assert_eq!(color.value(), "red");
        assert_eq!(color.origin(), DeclarationOrigin::Inline);
        assert_eq!(
            color.importance(),
            style.get("color").expect("orig").importance()
        );
        let width = g.get("width").expect("width");
        assert!(matches!(width.origin(), DeclarationOrigin::Rule { .. }));
        assert_eq!(width.origin(), style.get("width").expect("o").origin());
    }

    /// `cssProperties` ブロックのキーを行単位で抜き出す（データは自前生成の固定書式）。
    fn css_property_keys(json: &str) -> Vec<String> {
        let mut keys = Vec::new();
        let mut inside = false;
        for line in json.lines() {
            if !inside {
                inside = line.trim_end() == r#"  "cssProperties": {"#;
            } else if line.trim_start().starts_with('}') {
                break;
            } else if let Some(rest) = line.trim_start().strip_prefix('"')
                && let Some((k, _)) = rest.split_once('"')
            {
                keys.push(k.to_string());
            }
        }
        keys
    }

    #[test]
    fn plug8_diff_property_set_is_exactly_three() {
        let chrome = load_profile(BrowserProfile::Chrome).expect("chrome");
        let safari = load_profile(BrowserProfile::Safari).expect("safari");
        for n in CHROME_ONLY {
            assert_eq!(
                chrome.property_support(n),
                PropertySupport::Supported,
                "{n}"
            );
            assert_eq!(
                safari.property_support(n),
                PropertySupport::Unsupported,
                "{n}"
            );
        }
        for n in SAFARI_ONLY {
            assert_eq!(
                chrome.property_support(n),
                PropertySupport::Unsupported,
                "{n}"
            );
            assert_eq!(
                safari.property_support(n),
                PropertySupport::Supported,
                "{n}"
            );
        }
        // 取りこぼしを黙って通さない（fail-closed）: 抽出数がテーブル件数と一致すること。
        // Safari 側にだけ追加されたキーも見逃さないよう、両プロファイルのキーの和集合を走査する。
        let chrome_keys = css_property_keys(CHROME_JSON);
        let safari_keys = css_property_keys(SAFARI_JSON);
        assert_eq!(chrome_keys.len(), chrome.property_count());
        assert_eq!(safari_keys.len(), safari.property_count());
        let mut keys = chrome_keys;
        keys.extend(safari_keys);
        keys.sort();
        keys.dedup();
        let mut chrome_only = Vec::new();
        let mut safari_only = Vec::new();
        for k in &keys {
            match (chrome.property_support(k), safari.property_support(k)) {
                (a, b) if a == b => {}
                (PropertySupport::Supported, _) => chrome_only.push(k.as_str()),
                (_, PropertySupport::Supported) => safari_only.push(k.as_str()),
                other => panic!("unexpected pair for {k}: {other:?}"),
            }
        }
        assert_eq!(chrome_only, CHROME_ONLY);
        assert_eq!(safari_only, SAFARI_ONLY);
    }
}
// ---- TASK-100.7（Issue #271）: 識別面の非変更 ----

/// CSSOM プロファイル（Chrome / Safari）の gating が識別面を変えないことの回帰テスト
/// （TASK-100.7・`PLUG-8`・線引きは `SEC-1`・`SEC-2`・MS-8）。
///
/// プロファイルは「CSS 機能の有無の再現」だけが対象で、UA 文字列・フィンガープリントの
/// 偽装は対象外（`.claude/rules/security.md`）。core が現に出力する唯一の識別面は
/// `Fetcher` が送る HTTP `User-Agent` ヘッダなので、これを検証対象とする。
///
/// 未検証の範囲（実装済みを装わない。REPAIR-3）:
/// - `navigator.userAgent` は JS グローバル `navigator` が未実装のため検証できない。
///   実装時に本節へ JS 評価ベースの検証を追加すること。
/// - cdp の `/json/version`・`Browser.getVersion` は cdp crate 側の範囲で、本節の対象外。
///
/// `FetchOptions` はプロファイルを受け取らないため、本テストは現状では「変化し得ない」
/// ことの確認であり、将来 UA がプロファイル依存になる配線が入った場合に落ちる tripwire である。
mod identity {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use fandhe_browser_core::cssom::{collect_document_styles, computed_style_in_document};
    use fandhe_browser_core::{
        BrowserProfile, Error, FetchOptions, Fetcher, ParseOptions, parse_document, profile_gate,
        profile_gate_from_name,
    };

    /// ヘッダ読み取りの上限（無制限バッファ確保を避ける）。
    const MAX_HEAD_BYTES: usize = 8192;

    /// 現行の識別面の期待値（`Fetcher::new` が固定設定する値）。
    fn expected_user_agent() -> String {
        format!("fandhe-browser/{}", env!("CARGO_PKG_VERSION"))
    }

    /// 受信した `User-Agent` ヘッダ値を channel へ送り、空の 200 を返す
    /// ループバック専用サーバーを起動する。
    fn spawn_ua_capture_server() -> (u16, mpsc::Receiver<Option<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let port = listener.local_addr().expect("local_addr").port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 512];
                while buf.len() < MAX_HEAD_BYTES && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or(&[])),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let ua = head.split("\r\n").skip(1).find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("user-agent")
                        .then(|| value.trim().to_string())
                });
                let _ = tx.send(ua);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        (port, rx)
    }

    /// ループバックへ 1 回 GET し、サーバーが受け取った `User-Agent` を返す。
    async fn sent_user_agent() -> String {
        let (port, rx) = spawn_ua_capture_server();
        let options = FetchOptions::new().with_allow_private_network_access(true);
        let fetcher = Fetcher::new(options).expect("Fetcher::new");
        let resp = fetcher
            .get(&format!("http://127.0.0.1:{port}/"))
            .await
            .expect("get");
        assert_eq!(resp.status(), 200);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("server must receive request")
            .expect("User-Agent header must be present")
    }

    /// gate を実際に適用し、除去されたプロパティ名を返す（gate が効いていることの確認用）。
    fn removed_names(profile: Option<BrowserProfile>) -> Vec<String> {
        let doc = parse_document(
            r#"<p style="speak:none;text-size-adjust:none;width:1px">x</p>"#,
            &ParseOptions::default(),
        )
        .expect("parse")
        .document;
        let styles = collect_document_styles(&doc).expect("collect");
        let p = doc
            .descendants(doc.root())
            .find(|&i| doc.local_name(i) == Some("p"))
            .expect("p");
        let style = computed_style_in_document(&doc, p, &styles).expect("computed");
        let gate = profile_gate(profile).expect("gate");
        gate.apply(&style)
            .removed()
            .iter()
            .map(|d| d.property().to_string())
            .collect()
    }

    #[tokio::test]
    async fn plug8_identity_user_agent_is_fixed_without_profile() {
        let _gate = profile_gate(None).expect("None は常に成功する");
        assert_eq!(sent_user_agent().await, expected_user_agent());
    }

    #[tokio::test]
    async fn plug8_identity_user_agent_unchanged_by_profile() {
        let mut seen = Vec::new();
        let mut removed = Vec::new();
        for profile in [
            None,
            Some(BrowserProfile::Chrome),
            Some(BrowserProfile::Safari),
        ] {
            removed.push(removed_names(profile));
            seen.push(sent_user_agent().await);
        }
        assert_eq!(removed[0], Vec::<String>::new());
        assert_eq!(removed[1], ["speak"]);
        assert_eq!(removed[2], ["speak", "text-size-adjust"]);
        let expected = expected_user_agent();
        assert_eq!(seen, [expected.clone(), expected.clone(), expected]);
    }

    #[tokio::test]
    async fn plug8_identity_user_agent_unchanged_by_profile_name() {
        for name in [None, Some("chrome"), Some("safari")] {
            profile_gate_from_name(name).expect("既知の名前は成功する");
            assert_eq!(sent_user_agent().await, expected_user_agent());
        }
        assert!(matches!(
            profile_gate_from_name(Some("firefox")),
            Err(Error::BrowserProfileName(_))
        ));
        assert_eq!(sent_user_agent().await, expected_user_agent());
    }

    #[tokio::test]
    async fn plug8_identity_user_agent_does_not_impersonate_browsers() {
        let _gate = profile_gate(Some(BrowserProfile::Chrome)).expect("gate");
        let ua = sent_user_agent().await;
        assert_eq!(ua, expected_user_agent());
        for token in ["Mozilla", "Chrome", "Safari", "AppleWebKit", "Gecko"] {
            assert!(!ua.contains(token), "UA must not contain {token}: {ua}");
        }
    }
}
