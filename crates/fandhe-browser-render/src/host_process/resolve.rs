//! ホストバイナリの解決と検証（設計 §2.6。RENDER-3・PLUG-1。TASK-38・#994）。
//!
//! 優先順位は 設定 → 環境変数 `FANDHE_BROWSER_RENDER_HOST` → 本体実行ファイルと同じ
//! ディレクトリの `fandhe-browser-servo-host[.exe]`。PATH 探索はしない。上位の候補が与えられて
//! 検証に失敗した場合は下位へ落とさずエラーにする（fail-closed）。
//! 環境変数と実行ファイルの場所は呼び出し側が読んで渡す純関数にして、テストが
//! プロセスの環境（edition 2024 では `set_var` が unsafe）に触れずに済むようにしている。
//! Windows の ACL 検査・署名・ハッシュ固定は v1 の範囲外。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{HostProcessError, InvalidBinaryReason};

/// ホストバイナリの場所を指す環境変数名（設計 §2.6）。
pub const HOST_BINARY_ENV: &str = "FANDHE_BROWSER_RENDER_HOST";
/// 本体実行ファイルの隣に置くホストの拡張子なしファイル名（設計 §2.6）。
pub const HOST_BINARY_STEM: &str = "fandhe-browser-servo-host";

/// 検証済みのホストバイナリ（canonicalize 済みの絶対パス）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostBinary {
    path: PathBuf,
}

impl HostBinary {
    /// canonicalize 済みの絶対パス。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// ホストバイナリを解決する。どの候補も無ければ [`HostProcessError::Disabled`]。
pub fn resolve_host_binary(
    configured: Option<&Path>,
    env_value: Option<&OsStr>,
    exe_dir: Option<&Path>,
) -> Result<HostBinary, HostProcessError> {
    if let Some(p) = configured {
        return validate(p);
    }
    if let Some(v) = env_value {
        return validate(Path::new(v));
    }
    if let Some(dir) = exe_dir {
        let candidate = dir.join(format!(
            "{HOST_BINARY_STEM}{}",
            std::env::consts::EXE_SUFFIX
        ));
        // 隣接ファイルが無いのは「設定されていない」であって不正ではない。
        if std::fs::symlink_metadata(&candidate).is_err() {
            return Err(HostProcessError::Disabled);
        }
        return validate(&candidate);
    }
    Err(HostProcessError::Disabled)
}

/// 本番用の薄いラッパー。環境変数と `current_exe()` の親ディレクトリを読んで渡す。
pub fn resolve_host_binary_from_process(
    configured: Option<&Path>,
) -> Result<HostBinary, HostProcessError> {
    let env_value = std::env::var_os(HOST_BINARY_ENV);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    resolve_host_binary(configured, env_value.as_deref(), exe_dir.as_deref())
}

fn validate(path: &Path) -> Result<HostBinary, HostProcessError> {
    let invalid = |r| Err(HostProcessError::InvalidHostBinary(r));
    if path.as_os_str().is_empty() {
        return invalid(InvalidBinaryReason::Empty);
    }
    if !path.is_absolute() {
        return invalid(InvalidBinaryReason::NotAbsolute);
    }
    let Ok(canonical) = path.canonicalize() else {
        return invalid(InvalidBinaryReason::NotFound);
    };
    let Ok(meta) = std::fs::metadata(&canonical) else {
        return invalid(InvalidBinaryReason::NotFound);
    };
    if !meta.is_file() {
        return invalid(InvalidBinaryReason::NotRegularFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        // グループ書き込み可（0o020）も他者書き込み可（0o002）も拒否する（設計 §2.6）。
        if mode & 0o022 != 0 {
            return invalid(InvalidBinaryReason::WorldWritable);
        }
        if mode & 0o111 == 0 {
            return invalid(InvalidBinaryReason::NotExecutable);
        }
    }
    Ok(HostBinary { path: canonical })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト専用の一時ディレクトリ（終了時に削除）。
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "fandhe-resolve-test-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("mkdir");
            Self(p.canonicalize().expect("canonicalize"))
        }
        fn file(&self, name: &str, mode: u32) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, b"x").expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).expect("chmod");
            }
            let _ = mode;
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn reason(r: Result<HostBinary, HostProcessError>) -> InvalidBinaryReason {
        match r {
            Err(HostProcessError::InvalidHostBinary(r)) => r,
            other => panic!("expected InvalidHostBinary, got {other:?}"),
        }
    }

    #[test]
    fn render3_unset_everywhere_is_disabled() {
        assert!(matches!(
            resolve_host_binary(None, None, None),
            Err(HostProcessError::Disabled)
        ));
        let t = Tmp::new("empty-dir");
        assert!(matches!(
            resolve_host_binary(None, None, Some(&t.0)),
            Err(HostProcessError::Disabled)
        ));
    }

    #[test]
    fn render3_rejects_relative_empty_missing_and_directory() {
        let t = Tmp::new("reject");
        assert_eq!(
            reason(resolve_host_binary(Some(Path::new("host")), None, None)),
            InvalidBinaryReason::NotAbsolute
        );
        assert_eq!(
            reason(resolve_host_binary(Some(Path::new("")), None, None)),
            InvalidBinaryReason::Empty
        );
        assert_eq!(
            reason(resolve_host_binary(None, Some(OsStr::new("")), None)),
            InvalidBinaryReason::Empty
        );
        assert_eq!(
            reason(resolve_host_binary(Some(&t.0.join("nope")), None, None)),
            InvalidBinaryReason::NotFound
        );
        assert_eq!(
            reason(resolve_host_binary(Some(&t.0), None, None)),
            InvalidBinaryReason::NotRegularFile
        );
    }

    #[test]
    fn render3_invalid_configured_does_not_fall_back_to_env() {
        let t = Tmp::new("fallback");
        let good = t.file("good", 0o755);
        let r = resolve_host_binary(Some(Path::new("relative")), Some(good.as_os_str()), None);
        assert_eq!(reason(r), InvalidBinaryReason::NotAbsolute);
    }

    #[test]
    fn render3_configured_wins_over_env_and_adjacent() {
        let t = Tmp::new("priority");
        let a = t.file("a", 0o755);
        let b = t.file("b", 0o755);
        let bin = resolve_host_binary(Some(&a), Some(b.as_os_str()), Some(&t.0)).expect("ok");
        assert_eq!(bin.path(), a.as_path());
        let bin = resolve_host_binary(None, Some(b.as_os_str()), Some(&t.0)).expect("ok");
        assert_eq!(bin.path(), b.as_path());
    }

    #[test]
    fn render3_adjacent_binary_is_found_and_validated() {
        let t = Tmp::new("adjacent");
        let name = format!("{HOST_BINARY_STEM}{}", std::env::consts::EXE_SUFFIX);
        let p = t.file(&name, 0o755);
        let bin = resolve_host_binary(None, None, Some(&t.0)).expect("ok");
        assert_eq!(bin.path(), p.as_path());
    }

    #[cfg(unix)]
    #[test]
    fn render3_rejects_world_writable_and_non_executable() {
        let t = Tmp::new("mode");
        let w = t.file("w", 0o777);
        assert_eq!(
            reason(resolve_host_binary(Some(&w), None, None)),
            InvalidBinaryReason::WorldWritable
        );
        let g = t.file("g", 0o775);
        assert_eq!(
            reason(resolve_host_binary(Some(&g), None, None)),
            InvalidBinaryReason::WorldWritable
        );
        let g2 = t.file("g2", 0o720);
        assert_eq!(
            reason(resolve_host_binary(Some(&g2), None, None)),
            InvalidBinaryReason::WorldWritable
        );
        let o = t.file("o", 0o702);
        assert_eq!(
            reason(resolve_host_binary(Some(&o), None, None)),
            InvalidBinaryReason::WorldWritable
        );
        let n = t.file("n", 0o644);
        assert_eq!(
            reason(resolve_host_binary(Some(&n), None, None)),
            InvalidBinaryReason::NotExecutable
        );
    }

    #[cfg(unix)]
    #[test]
    fn render3_symlink_is_canonicalized() {
        let t = Tmp::new("symlink");
        let real = t.file("real", 0o755);
        let link = t.0.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let bin = resolve_host_binary(Some(&link), None, None).expect("ok");
        assert_eq!(bin.path(), real.as_path());
    }
}
