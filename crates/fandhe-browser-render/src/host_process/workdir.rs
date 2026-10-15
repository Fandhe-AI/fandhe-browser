//! ホスト専用の一時作業ディレクトリ（設計 §2.6。PROF 境界。TASK-38・#994）。
//!
//! [`super::HostProcess`] がホストを起動するたびに 1 つ作り、子の `current_dir` と
//! `HOME`・`TMPDIR` 等の指し先にする。0700（Unix）で排他作成し、終了時に削除する。
//! プロファイル名などの外部文字列はパスに入れない。Windows の ACL は既定
//! （ユーザーの `%TEMP%` 配下）のままで、範囲外・未確認。

use std::fs::DirBuilder;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::HostProcessError;

static COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_CREATE_ATTEMPTS: u32 = 8;

/// 専用の一時ディレクトリ。Drop で削除する。
#[derive(Debug)]
pub struct WorkDir {
    root: PathBuf,
}

fn make_dir(path: &Path) -> std::io::Result<()> {
    let mut b = DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    #[cfg(not(unix))]
    let _ = &mut b;
    b.create(path)
}

impl WorkDir {
    /// `temp_root`（絶対パス必須）の直下に作る。名前が衝突したら別名で再試行する。
    pub fn create(temp_root: &Path) -> Result<Self, HostProcessError> {
        if !temp_root.is_absolute() {
            return Err(HostProcessError::WorkDirFailed);
        }
        for _ in 0..MAX_CREATE_ATTEMPTS {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = temp_root.join(format!("fandhe-render-{}-{nanos}-{n}", std::process::id()));
            match make_dir(&root) {
                Ok(()) => {
                    // 以降の失敗でも Drop で消えるよう、先に値を組み立てる。
                    let wd = Self { root };
                    wd.verify_and_populate()?;
                    return Ok(wd);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(HostProcessError::WorkDirFailed),
            }
        }
        Err(HostProcessError::WorkDirFailed)
    }

    fn verify_and_populate(&self) -> Result<(), HostProcessError> {
        let meta =
            std::fs::symlink_metadata(&self.root).map_err(|_| HostProcessError::WorkDirFailed)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(HostProcessError::WorkDirFailed);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(HostProcessError::WorkDirFailed);
            }
        }
        for sub in ["home", "tmp", "xdg-config", "xdg-cache", "xdg-data"] {
            make_dir(&self.root.join(sub)).map_err(|_| HostProcessError::WorkDirFailed)?;
        }
        Ok(())
    }

    /// ディレクトリ本体（子の cwd）。
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// `HOME` の指し先。
    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// `TMPDIR` / `TMP` / `TEMP` の指し先。
    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        // 子を wait した後に呼ばれる前提（Windows は cwd 使用中だと削除できない）。
        // 失敗しても panic せず、残骸は OS の一時領域掃除に任せる。
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render3_workdir_is_private_unique_and_removed_on_drop() {
        let root = std::env::temp_dir();
        let a = WorkDir::create(&root).expect("a");
        let b = WorkDir::create(&root).expect("b");
        assert_ne!(a.path(), b.path());
        assert!(a.home().is_dir() && a.tmp().is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(a.path())
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let p = a.path().to_path_buf();
        drop(a);
        assert!(!p.exists());
    }

    #[test]
    fn render3_workdir_rejects_relative_root() {
        assert!(matches!(
            WorkDir::create(Path::new("relative")),
            Err(HostProcessError::WorkDirFailed)
        ));
    }
}
