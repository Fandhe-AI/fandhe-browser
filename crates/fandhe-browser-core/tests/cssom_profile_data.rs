//! `profiles/chrome.json`・`profiles/safari.json` のデータ配置の回帰テスト。
//!
//! TASK-100.1（Issue #265）・`PLUG-8`。後続の読み込みモジュール（TASK-100.2）が
//! `include_str!` で埋め込む相対パスが解決できることと、キュレーション結果
//! （`width` 等の基本プロパティを gating 対象に含めない）が維持されることを固定する。
//! JSON としての厳密な解釈検証は読み込みモジュール側の責務で、ここでは行わない
//! （依存を増やさないため std の文字列検査のみ）。

const CHROME: &str = include_str!("../../../profiles/chrome.json");
const SAFARI: &str = include_str!("../../../profiles/safari.json");

#[test]
fn plug8_profile_data_declares_browser() {
    assert!(CHROME.contains("\"browser\": \"chrome\""));
    assert!(SAFARI.contains("\"browser\": \"safari\""));
}

#[test]
fn plug8_profile_data_uses_lf_only() {
    assert!(!CHROME.contains('\r'));
    assert!(!SAFARI.contains('\r'));
}

#[test]
fn plug8_profile_data_records_sources_and_attribution() {
    for data in [CHROME, SAFARI] {
        assert!(data.contains("\"web-features\""));
        assert!(data.contains("\"caniuse-lite\""));
        assert!(data.contains("\"Apache-2.0\""));
        assert!(data.contains("\"CC-BY-4.0\""));
    }
}

#[test]
fn plug8_profile_data_keeps_gating_targets() {
    for data in [CHROME, SAFARI] {
        assert!(data.contains("\"user-select\": \"user-select\""));
        assert!(data.contains("\"position-area\": \"anchor-positioning\""));
    }
}

#[test]
fn plug8_profile_data_excludes_miscurated_properties() {
    for data in [CHROME, SAFARI] {
        assert!(!data.contains("\"width\":"));
        assert!(!data.contains("\"margin\":"));
        assert!(!data.contains("\"-webkit-user-select\":"));
    }
}
