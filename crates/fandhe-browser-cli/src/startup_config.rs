//! 起動時の設定ファイル読み込み（TASK-30.5・`JS-1`・`JS-2`・MS-3）。
//!
//! `main::run_main` から、プロファイル open・bind より前に呼ばれる。環境変数
//! `FANDHE_BROWSER_CONFIG` で指定された設定ファイルを core の [`Config::load`] で読み込み、
//! 未同梱エンジンの指定（`JS-1` 切替方式ケース (3)）などをサーバー起動前の非 0 終了にする。
//! フォールバックはしない。
//!
//! # スタブについて（REPAIR-3）
//!
//! - 指定方法は暫定。正式な引数（`--config` 等）は TASK-47（`CLI-1`）で決め、環境変数方式の
//!   存廃もそこで判断する。未設定時は設定なし（既定値）で従来どおり起動する
//! - `[profile] root` は cli のプロファイル選択へ未配線のため、指定された設定は拒否する
//!   （黙って無視しない。TASK-47.6・TASK-60.4 で配線する）
//! - 選択された JS エンジンは検証のみで、`JsRuntime` の `AppState` への配線・起動時の
//!   構造化ログ出力（`REPAIR-9`）は後続タスクで行う

use std::ffi::OsString;
use std::path::Path;

use fandhe_browser_core::Config;

use crate::server::StartupError;

/// 設定ファイルのパスを指定する環境変数名（暫定）。
pub(crate) const CONFIG_ENV: &str = "FANDHE_BROWSER_CONFIG";

/// 環境変数から設定を読み込む。
pub(crate) fn load_startup_config() -> Result<Config, StartupError> {
    load_startup_config_from(std::env::var_os(CONFIG_ENV))
}

/// 環境変数値（未設定は `None`）から設定を読み込む。テスト容易性のため環境と分離している。
pub(crate) fn load_startup_config_from(raw: Option<OsString>) -> Result<Config, StartupError> {
    let Some(raw) = raw else {
        return Ok(Config::default());
    };
    if raw.is_empty() {
        return Err(StartupError::ConfigPathEmpty);
    }
    let config = Config::load(Path::new(&raw)).map_err(StartupError::Config)?;
    if config.profile().root().is_some() {
        return Err(StartupError::ProfileRootNotWired);
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{CONFIG_PATH_EMPTY_MESSAGE, PROFILE_ROOT_NOT_WIRED_MESSAGE};
    use std::error::Error as _;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "fandhe-cli-startup-config-{tag}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&p).expect("temp dir");
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn unset_yields_default_config() {
        let config = load_startup_config_from(None).expect("unset is ok");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn empty_path_is_rejected_with_fixed_message() {
        let err = load_startup_config_from(Some(OsString::new())).expect_err("empty");
        assert_eq!(err.to_string(), CONFIG_PATH_EMPTY_MESSAGE);
    }

    #[test]
    fn missing_file_is_config_error_and_chains_source() {
        let dir = TempDir::new("missing");
        let path = dir.0.join("nope.toml");
        let err = load_startup_config_from(Some(path.into_os_string())).expect_err("missing");
        assert!(matches!(err, StartupError::Config(_)));
        let shown = err.to_string();
        assert_eq!(shown, "failed to read the config file");
        assert!(!shown.contains("nope.toml"));
        assert!(err.source().is_some());
    }

    #[test]
    fn profile_root_is_rejected_with_fixed_message() {
        let dir = TempDir::new("root");
        let path = dir.0.join("fandhe-browser.toml");
        std::fs::write(&path, "[profile]\nroot = \"p\"\n").expect("write");
        let err = load_startup_config_from(Some(path.into_os_string())).expect_err("root");
        assert!(matches!(err, StartupError::ProfileRootNotWired));
        assert_eq!(err.to_string(), PROFILE_ROOT_NOT_WIRED_MESSAGE);
    }

    #[test]
    fn valid_config_is_loaded() {
        let dir = TempDir::new("valid");
        let path = dir.0.join("fandhe-browser.toml");
        std::fs::write(&path, "[js]\n").expect("write");
        assert!(load_startup_config_from(Some(path.into_os_string())).is_ok());
    }
}
