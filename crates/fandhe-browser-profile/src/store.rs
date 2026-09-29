//! プロファイル保存先の解決・作成・削除を OS 差異ごと隠す抽象層。
//!
//! `fandhe-browser-cli`（TASK-41 で追加予定）が [`ProfileStore`] 越しに
//! [`Profile`] を得る設計を目指す（`XOS-7`、TASK-60（60.2）、MS-3）。保存先の
//! 慣習（Linux の XDG・macOS の Application Support・Windows の AppData）、
//! パス長、大文字小文字、ロックといった OS 差異をこのトレイトの背後へ閉じ込める。
//!
//! ## 実装者が守る契約
//!
//! - [`ProfileStore::resolve_root`] は絶対パスを返す。相対パスは
//!   [`ProfileStore::open_or_create`] が [`ProfileError::InvalidLayout`] で拒否する
//! - ルート経路上の symlink の検証は [`Profile::open`] がハンドル基準で行う
//!   （`PROF-1`・`PROF-4`）。macOS の `/var` のような OS 標準の symlink を含む
//!   パスは、[`Profile::open`] の doc と同様に事前の `canonicalize` が必要
//!
//! ## スタブについて（`code-comment-style.md`・REPAIR-3）
//!
//! 本モジュールはトレイト定義のみで、具象型は未実装である。
//!
//! - OS 既定パスの解決（`directories` は #200 の判断で不採用。std の環境変数で
//!   自前解決する）: #202（TASK-60（60.3））
//! - 明示指定による上書き（`RootSource::Explicit`。`PROF-1`）: #203（TASK-60（60.4））
//! - 削除（`PROF-5`）: #188（TASK-53（53.2））。[`ProfileStore::delete`] の既定実装は
//!   成功を装わず [`ProfileError::Unsupported`] を返す
//! - Windows 長パス（`XOS-8`、TASK-61）・名前正規化（`XOS-9`、TASK-62）は
//!   将来この層へ差し込む。Windows の ACL 隔離が未実装のため、現状 Windows の
//!   [`Profile::open`] は `Unsupported` を返し、[`ProfileStore::open_or_create`] も
//!   それをそのまま伝える

use std::path::{Path, PathBuf};

use crate::profile::{Profile, ProfileError};

/// プロファイルルートの解決元。将来の解決経路追加に備え `non_exhaustive`（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RootSource {
    /// OS 慣習に基づく既定保存先（`XOS-7`。#202 で実装）。
    OsDefault,
    /// 呼び出し元による明示指定（`PROF-1`。#203 で実装）。
    ///
    /// 相対パスの絶対化は上書き境界（#203）の責務で、ここへ渡る値は絶対パスとする。
    Explicit,
}

/// [`ProfileStore::resolve_root`] の結果。解決したパスとその解決元を持つ。
///
/// 生の `PathBuf` ではなく構造体にすることで、解決元などの情報を後から
/// 拡張できるようにする（REPAIR-4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRoot {
    path: PathBuf,
    source: RootSource,
}

impl ResolvedRoot {
    /// 解決結果を作る。`ProfileStore` 実装者（#202・#203）が使う。
    pub fn new(path: PathBuf, source: RootSource) -> Self {
        Self { path, source }
    }

    /// 解決したプロファイルルートのパス。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 解決元。
    pub fn source(&self) -> RootSource {
        self.source
    }

    /// パスの所有権を取り出す。
    pub fn into_path(self) -> PathBuf {
        self.path
    }
}

/// プロファイル保存先のパス解決・作成・削除を担うトレイト（`XOS-7`、TASK-60（60.2））。
///
/// dyn 互換（`Box<dyn ProfileStore>` を cli が保持できる）。
pub trait ProfileStore {
    /// プロファイルルートを解決する。絶対パスを返すこと。
    ///
    /// 既定実装は置かない（偽装や #202 との重複を避ける）。
    fn resolve_root(&self) -> Result<ResolvedRoot, ProfileError>;

    /// 解決したルートでプロファイルを開く。存在しなければ作成する。
    ///
    /// 解決結果が相対パスなら何も作成せず [`ProfileError::InvalidLayout`] を返す
    /// （カレントディレクトリ次第で保存先が変わるのを防ぐ）。境界検証は
    /// [`Profile::open`] に委譲する。
    fn open_or_create(&self) -> Result<Profile, ProfileError> {
        let resolved = self.resolve_root()?;
        if !resolved.path().is_absolute() {
            return Err(ProfileError::InvalidLayout {
                path: resolved.into_path(),
                reason: "profile root must be an absolute path",
            });
        }
        Profile::open(resolved.path())
    }

    /// プロファイルを削除する。
    ///
    /// 既定実装は fail-closed で [`ProfileError::Unsupported`] を返し、成功を装わない
    /// （`PROF-5`・REPAIR-3）。#188（TASK-53（53.2））が置き換える。`Profile` を借用で
    /// 受け取るのは、ロック保持者だけが削除できるようにするためと、削除失敗時に
    /// 呼び出し側が `Profile`（と `profile.lock`）を保持し続けられるようにするため
    /// （値渡しだと失敗時にロックが解放され、他プロセスが同一プロファイルを開ける。
    /// `PROF-1`）。成功時のロック解放の扱いは #188 が定める。
    fn delete(&self, profile: &Profile) -> Result<(), ProfileError> {
        let _ = profile;
        Err(ProfileError::Unsupported {
            reason: "profile deletion is not implemented yet (tracked by PROF-5 / TASK-53)",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedStore {
        resolved: ResolvedRoot,
    }

    impl ProfileStore for FixedStore {
        fn resolve_root(&self) -> Result<ResolvedRoot, ProfileError> {
            Ok(self.resolved.clone())
        }
    }

    #[cfg(unix)]
    struct TempDir(PathBuf);

    #[cfg(unix)]
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let base = std::env::temp_dir().canonicalize().unwrap();
            let path = base.join(format!(
                "fandhe-profile-store-test-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            TempDir(path)
        }
    }

    #[cfg(unix)]
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// XOS-7: dyn 互換で解決結果が具体値で返る。
    #[test]
    fn xos_7_profile_store_is_dyn_compatible() {
        let root = std::env::temp_dir().join("fandhe-store-dyn");
        let store: Box<dyn ProfileStore> = Box::new(FixedStore {
            resolved: ResolvedRoot::new(root.clone(), RootSource::OsDefault),
        });
        let resolved = store.resolve_root().unwrap();
        assert_eq!(resolved.path(), root.as_path());
        assert_eq!(resolved.source(), RootSource::OsDefault);
    }

    /// XOS-7: `ResolvedRoot` のアクセサが入力と一致する。
    #[test]
    fn xos_7_resolved_root_accessors() {
        let path = PathBuf::from("some").join("path");
        let r = ResolvedRoot::new(path.clone(), RootSource::Explicit);
        assert_eq!(r.path(), path.as_path());
        assert_eq!(r.source(), RootSource::Explicit);
        assert_eq!(r.into_path(), path);
    }

    /// XOS-7: 相対パスは拒否され、何も作成されない。
    #[test]
    fn xos_7_open_or_create_rejects_relative_root() {
        let rel = PathBuf::from("relative").join("fandhe-store-profile");
        let store = FixedStore {
            resolved: ResolvedRoot::new(rel.clone(), RootSource::Explicit),
        };
        match store.open_or_create() {
            Err(ProfileError::InvalidLayout { path, reason }) => {
                assert_eq!(path, rel);
                assert_eq!(reason, "profile root must be an absolute path");
            }
            other => panic!("expected InvalidLayout, got {:?}", other.map(|_| ())),
        }
        assert!(!rel.exists());
    }

    /// XOS-7: 絶対パスなら `Profile::open` に委譲される。
    #[cfg(unix)]
    #[test]
    fn xos_7_open_or_create_delegates_to_profile_open() {
        let tmp = TempDir::new();
        let store = FixedStore {
            resolved: ResolvedRoot::new(tmp.0.clone(), RootSource::OsDefault),
        };
        let profile = store.open_or_create().unwrap();
        assert_eq!(profile.root(), tmp.0.as_path());
    }

    /// XOS-7: Windows では既存の fail-closed（`Unsupported`）を伝え、作成しない。
    #[cfg(not(unix))]
    #[test]
    fn xos_7_open_or_create_is_unsupported_on_windows() {
        let root = std::env::temp_dir().join("fandhe-store-win-unsupported");
        let store = FixedStore {
            resolved: ResolvedRoot::new(root.clone(), RootSource::OsDefault),
        };
        assert!(matches!(
            store.open_or_create(),
            Err(ProfileError::Unsupported { .. })
        ));
        assert!(!root.exists());
    }

    /// PROF-5: 既定の `delete` は削除を装わず `Unsupported` を返しルートが残る。
    #[cfg(unix)]
    #[test]
    fn prof_5_delete_default_is_fail_closed() {
        let tmp = TempDir::new();
        let store = FixedStore {
            resolved: ResolvedRoot::new(tmp.0.clone(), RootSource::OsDefault),
        };
        let profile = store.open_or_create().unwrap();
        match store.delete(&profile) {
            Err(ProfileError::Unsupported { reason }) => {
                assert_eq!(
                    reason,
                    "profile deletion is not implemented yet (tracked by PROF-5 / TASK-53)"
                );
            }
            other => panic!("expected Unsupported, got {:?}", other),
        }
        assert!(tmp.0.is_dir());
        // 失敗後もロックは保持されたまま（PROF-1）: 別ハンドルの open は Locked になる。
        assert!(matches!(
            Profile::open(&tmp.0),
            Err(ProfileError::Locked { .. })
        ));
        drop(profile);
    }
}
