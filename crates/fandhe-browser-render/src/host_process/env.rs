//! 子プロセスへ渡す環境変数の許可リスト（設計 §2.6。SEC-2・PROF。TASK-38・#994）。
//!
//! [`apply`] が `Command::env_clear()` の後に呼ばれ、次の 2 種類だけを設定する。
//!
//! - 強制設定 ([`FORCED_ENV_NAMES`]): 実ホーム・実プロファイルを漏らさないよう、専用
//!   一時ディレクトリ配下を指す変数。
//! - 引き継ぎ ([`PASSTHROUGH_ENV_NAMES`]): 親に値がある場合だけ渡す変数。
//!
//! 渡さない変数と理由: `PATH`（子は PATH 探索をしない）、`LANG`・`LC_*`・`TZ`・`*_PROXY`
//! （識別面。SEC-2）、`LD_*`・`DYLD_*`（ライブラリ注入経路）、`RUST_*`。
//!
//! 実機確認の状態（**すべて暫定。設計 §2.6「未検証」を解消するのは実ホームが揃う #237b**）:
//!
//! | 項目 | OS | 状態 |
//! | ---- | -- | ---- |
//! | `HOME`・`TMPDIR`・`XDG_*` を workdir 配下へ | Linux | 未確認（Servo ホストでの起動は未試験） |
//! | `FONTCONFIG_FILE`・`FONTCONFIG_PATH` の引き継ぎ | Linux | 未確認 |
//! | `HOME`・`TMPDIR` を workdir 配下へ | macOS | 未確認 |
//! | `TMP`・`TEMP` を workdir 配下へ | Windows | 未確認 |
//! | `SystemRoot` の引き継ぎ | Windows | js 子プロセスでは実績あり。Servo ホストでは未確認 |
//!
//! HOME を一時ディレクトリにした場合の fontconfig キャッシュ再生成コストも未確認。

use std::path::Path;
use std::process::Command;

/// 親の値によらず workdir 配下を指して設定する変数名。
#[cfg(all(unix, target_os = "linux"))]
pub const FORCED_ENV_NAMES: &[&str] = &[
    "HOME",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
];
/// 親の値によらず workdir 配下を指して設定する変数名。
#[cfg(all(unix, not(target_os = "linux")))]
pub const FORCED_ENV_NAMES: &[&str] = &["HOME", "TMPDIR"];
/// 親の値によらず workdir 配下を指して設定する変数名。
#[cfg(not(unix))]
pub const FORCED_ENV_NAMES: &[&str] = &["TMP", "TEMP"];

/// 親に値があるときだけ引き継ぐ変数名。
#[cfg(target_os = "linux")]
pub const PASSTHROUGH_ENV_NAMES: &[&str] = &["FONTCONFIG_FILE", "FONTCONFIG_PATH"];
/// 親に値があるときだけ引き継ぐ変数名。
#[cfg(windows)]
pub const PASSTHROUGH_ENV_NAMES: &[&str] = &["SystemRoot"];
/// 親に値があるときだけ引き継ぐ変数名。
#[cfg(not(any(target_os = "linux", windows)))]
pub const PASSTHROUGH_ENV_NAMES: &[&str] = &[];

/// `cmd` へ許可リストの環境変数を設定する（呼び出し側が `env_clear` 済み）。
/// `home` と `tmp` は workdir 配下の専用ディレクトリ。
pub fn apply(cmd: &mut Command, workdir: &Path, home: &Path, tmp: &Path) {
    for (name, value) in forced(workdir, home, tmp) {
        cmd.env(name, value);
    }
    for name in PASSTHROUGH_ENV_NAMES {
        if let Some(v) = std::env::var_os(name) {
            cmd.env(name, v);
        }
    }
}

fn forced<'a>(
    workdir: &Path,
    home: &'a Path,
    tmp: &'a Path,
) -> Vec<(&'static str, std::path::PathBuf)> {
    FORCED_ENV_NAMES
        .iter()
        .map(|&name| {
            let value = match name {
                "HOME" => home.to_path_buf(),
                "XDG_CONFIG_HOME" => workdir.join("xdg-config"),
                "XDG_CACHE_HOME" => workdir.join("xdg-cache"),
                "XDG_DATA_HOME" => workdir.join("xdg-data"),
                _ => tmp.to_path_buf(),
            };
            (name, value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render3_forced_env_points_into_workdir() {
        let wd = Path::new("/w");
        let (home, tmp) = (wd.join("home"), wd.join("tmp"));
        let f = forced(wd, &home, &tmp);
        let names: Vec<_> = f.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, FORCED_ENV_NAMES);
        assert!(f.iter().all(|(_, v)| v.starts_with(wd)));
    }

    #[test]
    fn render3_allowlist_excludes_identity_and_injection_vars() {
        for banned in ["PATH", "LANG", "TZ", "LD_PRELOAD", "DYLD_INSERT_LIBRARIES"] {
            assert!(!FORCED_ENV_NAMES.contains(&banned));
            assert!(!PASSTHROUGH_ENV_NAMES.contains(&banned));
        }
    }
}
