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
