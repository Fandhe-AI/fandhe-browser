//! リポジトリ直下の `fandhe-browser.toml` サンプルが `Config::load` で読め、
//! コメントに書いた挙動と実装がずれていないことを検証する結合テスト
//! （TASK-91（91.3）・Issue #216。対象ビヘイビアなし・基盤タスク）。

use std::path::PathBuf;

use fandhe_browser_core::{Config, ConfigError, Error, IsolationStrength, JsConfig};

fn sample_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fandhe-browser.toml")
}

/// サンプルは既定値どおりに読み込める。
#[test]
fn task_91_3_sample_loads_with_expected_values() {
    let config = Config::load(&sample_path()).expect("サンプルは読み込める");
    assert_eq!(config.profile().root(), None);
    assert_eq!(
        config.profile().isolation(),
        IsolationStrength::DataDirectory
    );
    assert!(!config.rendering().enabled());
    assert_eq!(config.js().engine(), JsConfig::default().engine());
}

/// サンプルのコメントどおり、`enabled = true` は未同梱ビルドで設定エラーになる。
#[test]
fn task_91_3_sample_enabled_true_is_rejected() {
    let text = std::fs::read_to_string(sample_path()).expect("サンプルを読める");
    let enabled = text.replace("enabled = false", "enabled = true");
    assert_ne!(text, enabled);
    let err = Config::from_toml_str(&enabled).expect_err("未同梱は拒否");
    assert!(matches!(
        err,
        Error::Config(ConfigError::RenderingNotCompiled)
    ));
}
