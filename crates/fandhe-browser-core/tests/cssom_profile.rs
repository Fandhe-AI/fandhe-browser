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
