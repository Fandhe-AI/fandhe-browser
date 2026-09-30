//! ファイル名・キー名の正規化と大文字小文字衝突の検出（`XOS-9`、TASK-62（62.1）・#208）。
//!
//! macOS（APFS）と Windows（NTFS）は既定で大文字小文字を区別せず、Linux（ext4）は
//! 区別する。`Foo` と `foo` を別の名前として作ると OS によって同一実体になったり
//! 別々になったりし、データの上書き・混線や 3 OS 間の挙動差を招く。そのため
//! キー・ファイル名は常に [`normalize_name`] で小文字へ正規化し、大文字小文字だけが
//! 異なる入力は [`NameRegistry`] がエラーとして検出する（OS の既定挙動に頼らない）。
//!
//! Cookie・Storage 等の実データ操作（TASK-51 以降）が、キー名をパスへ組み込む前に
//! 通す入口として使う想定。
//!
//! ## ASCII 限定の理由（fail-closed。REPAIR-3）
//!
//! APFS は Unicode 正規化（NFC/NFD）の違いも区別せず、NTFS は独自の大文字化表で
//! 比較する。std の `str::to_lowercase` はどちらとも一致せず、外部クレートなしでは
//! 「衝突を検出したように見えて漏れる」実装になる。そのため非 ASCII・非 UTF-8 は
//! 拒否する。ドメイン名は IDN でも punycode（ASCII）で扱う前提。
//!
//! ## スタブについて
//!
//! 非 ASCII 名の明示的エンコード（PoC-12 の代替案。percent-encoding 等）、
//! `Profile::create_file_in`・`ProfileStore` への組み込み、レジストリの永続化と
//! ディスク上の既存ファイルとの照合は未実装（`XOS-9` の将来仕様）。

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::OsStr;
use std::fmt;
use std::path::Path;

use crate::profile::{ProfileError, SafeComponent, sanitize_component};

/// [`NameRegistry`] が保持できる最大件数（外部入力による無制限確保の防止）。
pub const MAX_REGISTRY_ENTRIES: usize = 65_536;

/// [`normalize_name`] を通過した、正規化済みかつ検証済みの名前（`XOS-9`）。
///
/// フィールドは非公開で、`normalize_name` 経由でしか構築できない（REPAIR-4）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NormalizedName(String);

impl NormalizedName {
    /// 正規化後の文字列を返す。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `Profile::create_file_in` 等へ渡す [`SafeComponent`] を再検証なしで返す。
    pub fn as_safe_component(&self) -> SafeComponent<'_> {
        // `normalize_name` が sanitize_component 通過後に ASCII 小文字化しただけ
        // なので、単一の通常コンポーネントである不変条件が保たれる。
        SafeComponent::from_validated(OsStr::new(&self.0))
    }
}

impl AsRef<OsStr> for NormalizedName {
    fn as_ref(&self) -> &OsStr {
        OsStr::new(&self.0)
    }
}

impl AsRef<Path> for NormalizedName {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl fmt::Display for NormalizedName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 名前を正規化（ASCII 小文字化）する（`XOS-9`）。
///
/// 手順: [`sanitize_component`] でパス要素として検証（小文字化より先）→ UTF-8 検証
/// → ASCII 限定検証 → `to_ascii_lowercase`。いずれかに失敗すれば
/// `ProfileError::InvalidComponent` を返す。
pub fn normalize_name(raw: &OsStr) -> Result<NormalizedName, ProfileError> {
    sanitize_component(raw)?;
    let text = raw.to_str().ok_or(ProfileError::InvalidComponent {
        reason: "path component must be valid UTF-8",
    })?;
    if !text.is_ascii() {
        return Err(ProfileError::InvalidComponent {
            reason: "path component must be ASCII (non-ASCII names are not normalized yet)",
        });
    }
    Ok(NormalizedName(text.to_ascii_lowercase()))
}

/// 同一名前空間（例: 1 つのデータ種別ディレクトリ）内の大文字小文字衝突を検出する
/// レジストリ（`XOS-9`）。
///
/// key は正規化後の名前、value は登録時の元の綴り。スレッド間で共有する場合の
/// 同期は呼び出し側の責務（`&mut self` で操作する）。
#[derive(Debug, Default, Clone)]
pub struct NameRegistry {
    entries: HashMap<String, String>,
}

impl NameRegistry {
    /// 空のレジストリを作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// 正規化して登録する。
    ///
    /// - 正規化不能: `InvalidComponent`（状態は不変）
    /// - 未登録: 登録して `Ok`（上限到達時は `RegistryFull`）
    /// - 登録済みで綴りが完全一致: `Ok`（冪等）
    /// - 登録済みで綴りが異なる（大文字小文字のみの差）: `NameCollision`
    pub fn insert(&mut self, raw: &OsStr) -> Result<NormalizedName, ProfileError> {
        let normalized = normalize_name(raw)?;
        // normalize_name 通過済みなので UTF-8 かつ ASCII。
        let original = raw.to_str().unwrap_or_default();
        let full = self.entries.len() >= MAX_REGISTRY_ENTRIES;
        match self.entries.entry(normalized.0.clone()) {
            Entry::Occupied(existing) => {
                if existing.get() == original {
                    Ok(normalized)
                } else {
                    Err(ProfileError::NameCollision {
                        reason: "name differs from an existing entry only by letter case",
                    })
                }
            }
            Entry::Vacant(slot) => {
                if full {
                    return Err(ProfileError::RegistryFull {
                        limit: MAX_REGISTRY_ENTRIES,
                    });
                }
                slot.insert(original.to_owned());
                Ok(normalized)
            }
        }
    }

    /// 正規化したうえで登録済みかを返す（正規化できない入力は `false`）。
    pub fn contains(&self, raw: &OsStr) -> bool {
        normalize_name(raw)
            .map(|n| self.entries.contains_key(n.as_str()))
            .unwrap_or(false)
    }

    /// 登録件数を返す。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 未登録かを返す。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Result<NormalizedName, ProfileError> {
        normalize_name(OsStr::new(s))
    }

    fn reason(e: ProfileError) -> &'static str {
        match e {
            ProfileError::InvalidComponent { reason } => reason,
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn xos_9_normalize_lowercases_ascii() {
        for raw in ["Foo", "FOO", "fOo"] {
            assert_eq!(n(raw).unwrap().as_str(), "foo");
        }
    }

    #[test]
    fn xos_9_normalize_is_idempotent() {
        let once = n("foo").unwrap();
        assert_eq!(once.as_str(), "foo");
        assert_eq!(n(once.as_str()).unwrap(), once);
    }

    #[test]
    fn xos_9_normalize_preserves_non_letters() {
        assert_eq!(n("Example.COM_1-2").unwrap().as_str(), "example.com_1-2");
    }

    #[test]
    fn xos_9_normalize_rejects_invalid_component() {
        let long = "a".repeat(256);
        for raw in ["", ".", "a/b", "a\\b", "a..b", long.as_str()] {
            assert!(
                matches!(n(raw), Err(ProfileError::InvalidComponent { .. })),
                "{raw:?}"
            );
        }
        assert_eq!(
            reason(n("a/b").unwrap_err()),
            "path component must not contain '/'"
        );
    }

    #[test]
    fn xos_9_normalize_rejects_non_ascii() {
        for raw in ["Café", "İ"] {
            assert_eq!(
                reason(n(raw).unwrap_err()),
                "path component must be ASCII (non-ASCII names are not normalized yet)"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn xos_9_normalize_rejects_non_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let err = normalize_name(OsStr::from_bytes(b"a\xff")).unwrap_err();
        assert_eq!(reason(err), "path component must be valid UTF-8");
    }

    #[cfg(windows)]
    #[test]
    fn xos_9_normalize_rejects_non_utf8() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        // 単独の下位サロゲートは UTF-16 として不正（WTF-8 で保持され to_str が失敗する）。
        let raw = OsString::from_wide(&[0x61, 0xDC00]);
        let err = normalize_name(&raw).unwrap_err();
        assert_eq!(reason(err), "path component must be valid UTF-8");
    }

    #[test]
    fn xos_9_as_safe_component_matches_normalized() {
        let name = n("Foo").unwrap();
        assert_eq!(name.as_safe_component().as_os_str(), OsStr::new("foo"));
        assert_eq!(AsRef::<Path>::as_ref(&name), Path::new("foo"));
        assert_eq!(name.to_string(), "foo");
    }

    #[test]
    fn xos_9_registry_detects_case_only_collision() {
        let mut reg = NameRegistry::new();
        assert_eq!(reg.insert(OsStr::new("foo")).unwrap().as_str(), "foo");
        let err = reg.insert(OsStr::new("Foo")).unwrap_err();
        assert!(matches!(
            err,
            ProfileError::NameCollision {
                reason: "name differs from an existing entry only by letter case"
            }
        ));
        assert_eq!(reg.len(), 1);
        assert!(reg.contains(OsStr::new("FOO")));
    }

    #[test]
    fn xos_9_registry_same_spelling_is_idempotent() {
        let mut reg = NameRegistry::new();
        assert_eq!(reg.insert(OsStr::new("Foo")).unwrap().as_str(), "foo");
        assert_eq!(reg.insert(OsStr::new("Foo")).unwrap().as_str(), "foo");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn xos_9_registry_invalid_input_leaves_state_unchanged() {
        let mut reg = NameRegistry::new();
        assert!(reg.insert(OsStr::new("a/b")).is_err());
        assert_eq!(reg.len(), 0);
        assert!(reg.is_empty());
        assert!(!reg.contains(OsStr::new("a/b")));
    }

    #[test]
    fn xos_9_registry_full_is_rejected() {
        let mut reg = NameRegistry::new();
        for i in 0..MAX_REGISTRY_ENTRIES {
            reg.insert(OsStr::new(&format!("n{i}"))).unwrap();
        }
        assert!(matches!(
            reg.insert(OsStr::new("extra")),
            Err(ProfileError::RegistryFull {
                limit: MAX_REGISTRY_ENTRIES
            })
        ));
        assert_eq!(reg.insert(OsStr::new("n0")).unwrap().as_str(), "n0");
        assert!(matches!(
            reg.insert(OsStr::new("N0")),
            Err(ProfileError::NameCollision { .. })
        ));
        assert_eq!(reg.len(), MAX_REGISTRY_ENTRIES);
    }

    #[test]
    fn xos_9_new_error_variants_display() {
        assert_eq!(
            ProfileError::NameCollision { reason: "x" }.to_string(),
            "name collision: x"
        );
        assert_eq!(
            ProfileError::RegistryFull { limit: 3 }.to_string(),
            "name registry is full: limit is 3 entries"
        );
    }
}
