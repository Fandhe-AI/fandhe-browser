//! プラグインマニフェスト型の公開 API 結合テスト（`PLUG-2`・TASK-92.1・Issue #353）。

use fandhe_browser_ai::plugin_api::{ManifestError, PluginManifest, PluginTransport};
use serde_json::json;

#[test]
fn plug2_reference_plugin_manifest_round_trips_from_bytes() {
    let bytes = br#"{"id":"mcp-ref","version":"0.1.0","transport":"stdio",
        "tools":["fetch","snapshot"],"permissions":["network.fetch"]}"#;
    let m = PluginManifest::from_slice(bytes).unwrap();
    assert_eq!(m.transport(), PluginTransport::Stdio);
    assert_eq!(
        m.to_value(),
        json!({"id":"mcp-ref","version":"0.1.0","transport":"stdio",
               "tools":["fetch","snapshot"],"permissions":["network.fetch"],
               "protocolVersion":"unspecified"})
    );
}

#[test]
fn plug2_invalid_manifest_bytes_are_rejected() {
    let bytes = br#"{"id":"x","version":"1.0.0","transport":"stdio","tools":["t"],"extra":1}"#;
    assert_eq!(
        PluginManifest::from_slice(bytes).unwrap_err(),
        ManifestError::UnknownField
    );
}
