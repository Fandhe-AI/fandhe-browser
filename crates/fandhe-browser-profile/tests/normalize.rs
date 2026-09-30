//! 大文字小文字だけが異なる名前の衝突検出（`XOS-9`）の結合テスト
//! （TASK-62（62.2）・#209、MS-3）。
//!
//! `NameRegistry`（TASK-62（62.1）・#208）が、大文字小文字の差だけの 2 つの名前を
//! エラーとして検出することを、公開 API だけで確認する。`src/normalize.rs` の単体テストが
//! 正規化規則・上限・不正入力の詳細を扱うのに対し、本ファイルは crate 外部の利用者から見た
//! 衝突検出の契約（登録順・綴りの組み合わせ・状態不変・名前空間の独立性）を固定する。
//!
//! # OS 非依存である理由
//!
//! 実ファイルを作らずレジストリだけで判定するため、ext4（区別する）・APFS / NTFS
//! （既定で区別しない）の違いに左右されず、3 OS で同じ結果になる。これは
//! 「OS の既定挙動に頼らない」という `XOS-9` の要件そのものである。
//!
//! # 範囲外（未実装の将来仕様）
//!
//! ディスク上の既存ファイルとの照合、`Profile::create_file_in` / `ProfileStore` への
//! 組み込み、非 ASCII 名の扱いは `XOS-9` の将来仕様で、本テストでは扱わない。

use std::ffi::OsStr;

use fandhe_browser_profile::{NameRegistry, NormalizedName, ProfileError, normalize_name};

/// 衝突時に返る理由文字列（`NameRegistry::insert` の契約）。
const COLLISION_REASON: &str = "name differs from an existing entry only by letter case";

fn insert(reg: &mut NameRegistry, name: &str) -> Result<NormalizedName, ProfileError> {
    reg.insert(OsStr::new(name))
}

/// `NameCollision` かつ理由文字列まで一致することを検証する。
fn assert_collision(result: Result<NormalizedName, ProfileError>) {
    assert!(
        matches!(&result, Err(ProfileError::NameCollision { reason }) if *reason == COLLISION_REASON),
        "expected NameCollision, got {result:?}"
    );
}

#[test]
fn xos_9_case_only_difference_is_rejected() {
    let mut reg = NameRegistry::new();
    let first = insert(&mut reg, "Foo").unwrap();
    assert_eq!(first.as_str(), "foo");
    assert_collision(insert(&mut reg, "foo"));
    assert_eq!(reg.len(), 1);
}

#[test]
fn xos_9_collision_is_detected_regardless_of_insertion_order() {
    let mut reg = NameRegistry::new();
    insert(&mut reg, "foo").unwrap();
    assert_collision(insert(&mut reg, "Foo"));
    assert_eq!(reg.len(), 1);
}

#[test]
fn xos_9_every_case_variant_collides() {
    let mut reg = NameRegistry::new();
    insert(&mut reg, "foo").unwrap();
    for variant in ["FOO", "fOo", "foO", "Foo"] {
        assert_collision(insert(&mut reg, variant));
    }
    assert_eq!(reg.len(), 1);
}

#[test]
fn xos_9_collision_ignores_non_letter_characters() {
    let mut reg = NameRegistry::new();
    let first = insert(&mut reg, "Example.COM_1-2").unwrap();
    assert_eq!(first.as_str(), "example.com_1-2");
    assert_collision(insert(&mut reg, "example.com_1-2"));
    assert_eq!(reg.len(), 1);
}

#[test]
fn xos_9_collision_does_not_mutate_registry() {
    let mut reg = NameRegistry::new();
    insert(&mut reg, "Foo").unwrap();
    assert_collision(insert(&mut reg, "FOO"));
    // 元の綴りの再登録は冪等。
    assert_eq!(insert(&mut reg, "Foo").unwrap().as_str(), "foo");
    assert!(reg.contains(OsStr::new("FOO")));
    assert_eq!(reg.len(), 1);
    // 衝突後も無関係な名前は登録できる。
    assert_eq!(insert(&mut reg, "bar").unwrap().as_str(), "bar");
    assert_eq!(reg.len(), 2);
}

#[test]
fn xos_9_distinct_names_do_not_collide() {
    let mut reg = NameRegistry::new();
    for name in ["foo", "foo1", "fo", "bar"] {
        assert_eq!(insert(&mut reg, name).unwrap().as_str(), name);
    }
    assert_eq!(reg.len(), 4);
}

#[test]
fn xos_9_registries_are_independent_namespaces() {
    let mut a = NameRegistry::new();
    let mut b = NameRegistry::new();
    assert_eq!(insert(&mut a, "Foo").unwrap().as_str(), "foo");
    assert_eq!(insert(&mut b, "foo").unwrap().as_str(), "foo");
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
}

#[test]
fn xos_9_normalized_names_are_equal_for_case_variants() {
    let upper = normalize_name(OsStr::new("Foo")).unwrap();
    let lower = normalize_name(OsStr::new("foo")).unwrap();
    assert_eq!(upper, lower);
    assert_eq!(upper.as_str(), "foo");
    assert_eq!(lower.as_str(), "foo");
}

#[test]
fn xos_9_collision_error_display() {
    let mut reg = NameRegistry::new();
    insert(&mut reg, "Foo").unwrap();
    let err = insert(&mut reg, "foo").unwrap_err();
    assert_eq!(
        err.to_string(),
        "name collision: name differs from an existing entry only by letter case"
    );
}
