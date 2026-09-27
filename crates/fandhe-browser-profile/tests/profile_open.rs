//! `Profile::open`・`Profile::create_file_in` の公開 API を通した受入基準
//! 確認（TASK-50（50.1）・#176、TASK-50（50.4）・#179、ビヘイビア `PROF-1`、MS-3）。
//!
//! `PROF-1` は「ルートの `cookies/`・`storage/`・`cache/`・`history/` の
//! 4 サブディレクトリ構成、Linux/macOS ではパーミッション 700、Cookie・
//! ストレージ・キャッシュ・履歴はそれぞれ対応するサブディレクトリの配下に
//! 隔離する」ことを定める。本ファイルはそれを公開 API だけを使って固定する：
//!
//! - パーミッション（受入基準 A）: 新規作成時・既存の緩いディレクトリからの
//!   締め直し・ファイル作成後・再 open 後のいずれでもルートと 4 サブ
//!   ディレクトリが厳密に `0o700` であることを確認する
//! - データ隔離（受入基準 B）: 各データ種別への書き込みが対応する
//!   サブディレクトリの配下にだけ置かれ、他の kind のファイル・内容が
//!   混ざらないことを、ディレクトリツリーの集合一致で確認する
//!
//! 異なるプロファイル間でのデータ漏洩確認は `PROF-2`・TASK-51 の範囲
//! （`tests/isolation.rs` 相当・未実装）であり、本ファイルでは扱わない。
//!
//! Windows では ACL 隔離が未実装のため `Profile::open` は常に
//! `ProfileError::Unsupported` を返す（`XOS-7`〜`XOS-10`。PR #437 レビュー
//! 指摘への対応）。本ファイルの受入テストは実際にディレクトリが作られる
//! ことを検証する内容のため Unix 専用とし、Windows 側の契約は
//! `src/profile.rs` の `#[cfg(windows)]` テストで確認する。

#![cfg(unix)]

use fandhe_browser_profile::{DataKind, Profile, ProfileError};
use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// テスト用の一時ディレクトリ。drop 時に再帰削除する（tempfile 等の外部
/// 依存は追加しない方針。dependency-policy.md）。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // `std::env::temp_dir()` を `canonicalize` した基点から組み立てる。
        // macOS では `/var` が `/private/var` への OS 標準 symlink であり、
        // 正規化しないと `Profile::open` の厳格な symlink 検証（PR #437
        // レビュー指摘）が「意図しない既存の symlink」として弾いてしまう。
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let path = base.join(format!(
            "fandhe-profile-open-test-{}-{n}",
            std::process::id()
        ));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `path` のパーミッションビット（setuid・setgid・sticky を含む `0o7777`
/// マスク）を返す。`symlink_metadata` を使いリンクを辿らない。
/// `Profile` はパスを再解決しない契約（`src/profile.rs` の
/// モジュールドキュメント参照）のため、テスト側の確認もリンクを辿ると
/// その契約からずれてしまう。
fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap_or_else(|e| panic!("symlink_metadata({path:?}) が失敗した: {e}"))
        .permissions()
        .mode()
        & 0o7777
}

/// `root` 配下の通常ファイルを再帰的に集め、`root` からの相対パスの集合を
/// 返す。symlink を見つけた場合は辿らず即座に `panic!` する（受入基準 B の
/// 隔離確認は「4 サブディレクトリの外に何も書かれていない」ことを示す
/// ためのものであり、symlink 経由の到達可能性まで許容すると検証の意味が
/// 失われるため）。
fn collect_regular_files(root: &Path) -> BTreeSet<PathBuf> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeSet<PathBuf>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir({dir:?}) が失敗した: {e}"));
        for entry in entries {
            let entry = entry.unwrap_or_else(|e| panic!("{dir:?} の read_dir 走査失敗: {e}"));
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)
                .unwrap_or_else(|e| panic!("symlink_metadata({path:?}) が失敗した: {e}"));
            if meta.file_type().is_symlink() {
                panic!("想定外の symlink を検出した（辿らない）: {path:?}");
            } else if meta.is_dir() {
                walk(&path, root, out);
            } else if meta.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or_else(|e| panic!("{path:?} は {root:?} 配下ではない: {e}"));
                out.insert(relative.to_path_buf());
            } else {
                // FIFO・ソケット・デバイスファイル等、通常ファイル・
                // ディレクトリ・symlink のいずれでもない想定外のエントリ。
                // 受入基準 B（4 サブディレクトリの外に何も書かれていない
                // こと）を完全に確認するため、黙って無視せず検出する。
                panic!(
                    "想定外のエントリ種別を検出した（通常ファイル・ディレクトリ・symlink 以外）: {path:?}"
                );
            }
        }
    }

    let mut out = BTreeSet::new();
    walk(root, root, &mut out);
    out
}

/// 受入基準 1（PROF-1）: 存在しない入れ子のパスへ `open` すると、途中の
/// 階層を含めてルートディレクトリが作られる。
#[test]
fn prof_1_open_creates_nested_root_when_missing() {
    let tmp = TempDir::new();
    let root = tmp.path().join("a").join("b").join("profile");
    assert!(!root.exists());

    let profile = Profile::open(&root).expect("open は成功する");

    assert!(root.is_dir());
    assert_eq!(profile.root(), root.as_path());
}

/// 受入基準 2（PROF-1）: ルート直下に 4 つのデータ種別ディレクトリが
/// すべて作られ、いずれもディレクトリである。
#[test]
fn prof_1_open_creates_all_data_subdirectories() {
    let tmp = TempDir::new();
    let root = tmp.path().join("profile");

    let profile = Profile::open(&root).expect("open は成功する");

    for kind in DataKind::ALL.iter().copied() {
        let dir = profile.data_dir(kind);
        assert!(
            dir.is_dir(),
            "{:?} 用ディレクトリが存在しない: {dir:?}",
            kind
        );
    }
}

/// PROF-1: 一度 `Profile` を drop してから同じルートへ再度 `open` しても
/// 成功する（`profile.lock` は drop 時にアンロックされるため、2 つの
/// ハンドルを同時に持たない本テストの形は成立する）。
#[test]
fn prof_1_reopen_after_drop_succeeds() {
    let tmp = TempDir::new();
    let root = tmp.path().join("profile");

    let profile = Profile::open(&root).expect("初回 open は成功する");
    drop(profile);

    let reopened = Profile::open(&root).expect("再 open も成功する");
    assert_eq!(reopened.root(), root.as_path());
    for kind in DataKind::ALL.iter().copied() {
        assert!(reopened.data_dir(kind).is_dir());
    }
}

/// 受入基準 1（PROF-1、TASK-50（50.3）・#178）: 同じルートへ二重に `open`
/// すると、公開 API だけを通して `ProfileError::Locked` が返る。
#[test]
fn prof_1_double_open_same_root_returns_locked_error() {
    let tmp = TempDir::new();
    let root = tmp.path().join("profile");

    let first = Profile::open(&root).expect("1 回目の open は成功する");
    let err = Profile::open(&root).expect_err("2 回目の open はロックで失敗する");
    assert!(matches!(err, ProfileError::Locked { .. }));

    drop(first);
}

/// PROF-1（TASK-50（50.3）・#178）: ルートが異なれば同時に `open` できる
/// （`profile.lock` はルートごとに独立している）。
#[test]
fn prof_1_open_different_roots_concurrently_succeeds() {
    let tmp = TempDir::new();
    let root_a = tmp.path().join("profile-a");
    let root_b = tmp.path().join("profile-b");

    let profile_a = Profile::open(&root_a).expect("profile-a の open は成功する");
    let profile_b = Profile::open(&root_b).expect("profile-b の open は成功する");

    assert_eq!(profile_a.root(), root_a.as_path());
    assert_eq!(profile_b.root(), root_b.as_path());
}

// ---------------------------------------------------------------------
// 受入基準 A（PROF-1、TASK-50（50.4）・#179）: ルートと 4 サブディレクトリの
// パーミッションが厳密に 0o700 であること。
// ---------------------------------------------------------------------

/// PROF-1: 新規作成時、ルート・4 サブディレクトリが厳密に `0o700`、
/// `profile.lock` が `0o600` になる。`src/profile.rs` の
/// `prof_1_open_sets_mode_0700_on_unix` は最小限の確認であり、本テストは
/// 全 `DataKind` と `profile.lock` を含めて網羅的に固定する。
#[test]
fn prof_1_open_sets_exact_mode_0700_on_root_and_all_data_dirs() {
    let tmp = TempDir::new();
    let root = tmp.path().join("profile");

    let profile = Profile::open(&root).expect("open は成功する");

    assert_eq!(
        mode_of(profile.root()),
        0o700,
        "root のパーミッションが 0o700 でない"
    );
    for kind in DataKind::ALL.iter().copied() {
        let mode = mode_of(&profile.data_dir(kind));
        assert_eq!(
            mode, 0o700,
            "{kind:?} 用ディレクトリのパーミッションが 0o700 でない（実際: {mode:o}）"
        );
    }
    assert_eq!(
        mode_of(&root.join("profile.lock")),
        0o600,
        "profile.lock のパーミッションが 0o600 でない"
    );
}

/// PROF-1: 既存のルート・4 サブディレクトリが `0o777`（umask が緩い環境の
/// 代替）で作られていても、`open` はハンドル基準の `fchmod` で厳密に
/// `0o700` へ締め直す。umask の変更はプロセス全体・並列テストへ影響し、
/// 変えるには新規依存が要るため使わず、代わりに `set_permissions` で
/// 事前条件を作る。
#[test]
fn prof_1_open_tightens_permissive_preexisting_dirs_to_0700() {
    let tmp = TempDir::new();
    let root = tmp.path().join("profile");

    std::fs::create_dir_all(&root).expect("ルート作成");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777))
        .expect("ルートのパーミッション設定");
    for kind in DataKind::ALL.iter().copied() {
        let dir = root.join(kind.dir_name());
        std::fs::create_dir_all(&dir).expect("サブディレクトリ作成");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777))
            .expect("サブディレクトリのパーミッション設定");
    }

    let profile = Profile::open(&root).expect("open は成功する");

    assert_eq!(mode_of(profile.root()), 0o700);
    for kind in DataKind::ALL.iter().copied() {
        assert_eq!(
            mode_of(&profile.data_dir(kind)),
            0o700,
            "{kind:?} が 0o700 に締め直されない"
        );
    }
}

/// PROF-1: ファイル作成後・drop して再 open した後でも、ルートと
/// 4 サブディレクトリのパーミッションは `0o700` のまま維持される。
#[test]
fn prof_1_mode_0700_is_kept_after_file_creation_and_reopen() {
    let tmp = TempDir::new();
    let root = tmp.path().join("profile");

    {
        let profile = Profile::open(&root).expect("open は成功する");
        for kind in DataKind::ALL.iter().copied() {
            profile
                .create_file_in(kind, std::ffi::OsStr::new("data.bin"))
                .unwrap_or_else(|e| panic!("{kind:?} へのファイル作成が失敗した: {e:?}"));
        }
        assert_eq!(mode_of(profile.root()), 0o700);
        for kind in DataKind::ALL.iter().copied() {
            assert_eq!(mode_of(&profile.data_dir(kind)), 0o700);
        }
    }

    let reopened = Profile::open(&root).expect("再 open も成功する");
    assert_eq!(mode_of(reopened.root()), 0o700);
    for kind in DataKind::ALL.iter().copied() {
        assert_eq!(
            mode_of(&reopened.data_dir(kind)),
            0o700,
            "{kind:?} が再 open 後も 0o700 でない"
        );
    }
}

// ---------------------------------------------------------------------
// 受入基準 B（PROF-1、TASK-50（50.4）・#179）: 各データ種別への書き込みが
// 対応するサブディレクトリの配下にだけ置かれること。
// ---------------------------------------------------------------------

/// PROF-1: 同じファイル名で全 `DataKind` へ別々のマーカー内容を書き込み、
/// ルート配下に実在する通常ファイルの集合が「各サブディレクトリ配下の
/// そのファイル ＋ `profile.lock`」だけと完全一致することを確認する。
/// 存在チェックではなく集合の一致で確認するのは、4 サブディレクトリの外
/// （ルート直下など）に想定外のファイルが書かれていないことも同時に
/// 示すため。あわせて各ファイルの内容が対応する kind のマーカーと一致し、
/// 他の kind の内容と混ざっていないこと、パーミッションが `0o600` である
/// ことも確認する。
#[test]
fn prof_1_each_data_kind_is_written_only_under_its_subdirectory() {
    let tmp = TempDir::new();
    let root = tmp.path().join("profile");
    let profile = Profile::open(&root).expect("open は成功する");
    let name = "data.bin";

    let mut expected = BTreeSet::new();
    expected.insert(PathBuf::from("profile.lock"));

    for kind in DataKind::ALL.iter().copied() {
        let marker = format!("marker-{}", kind.dir_name());
        let mut file = profile
            .create_file_in(kind, std::ffi::OsStr::new(name))
            .unwrap_or_else(|e| panic!("{kind:?} へのファイル作成が失敗した: {e:?}"));
        file.write_all(marker.as_bytes())
            .unwrap_or_else(|e| panic!("{kind:?} への書き込みが失敗した: {e}"));
        file.sync_all()
            .unwrap_or_else(|e| panic!("{kind:?} の sync_all が失敗した: {e}"));

        expected.insert(PathBuf::from(kind.dir_name()).join(name));
    }
    // `DataKind::ALL` を使わず、PROF-1 が定める 4 サブディレクトリ名を
    // 独立に固定する。`expected` の構築自体が `DataKind::ALL` を走査して
    // 作られているため、`DataKind::ALL.len()` を期待値の算出にも使うと、
    // 将来 `ALL` から種別が漏れても両者が揃って縮み検出できない
    // （codex review 指摘 P2 対応）。
    let expected_dir_names: BTreeSet<&str> = ["cookies", "storage", "cache", "history"]
        .into_iter()
        .collect();
    assert_eq!(
        expected_dir_names.len(),
        4,
        "PROF-1 が定めるサブディレクトリは cookies/storage/cache/history の 4 種"
    );
    assert_eq!(
        expected.len(),
        expected_dir_names.len() + 1,
        "期待集合の要素数が PROF-1 の 4 種＋1（profile.lock）と一致しない \
         （kind の追加漏れを検出するための固定チェック）"
    );

    let actual = collect_regular_files(&root);
    assert_eq!(
        actual, expected,
        "ルート配下の通常ファイル集合が想定と一致しない（4 サブディレクトリの外・\
         意図しない場所への書き込みを検出する）"
    );

    for kind in DataKind::ALL.iter().copied() {
        let path = profile.data_dir(kind).join(name);
        let content =
            std::fs::read(&path).unwrap_or_else(|e| panic!("{path:?} の読み込みが失敗した: {e}"));
        let expected_marker = format!("marker-{}", kind.dir_name());
        assert_eq!(
            content,
            expected_marker.as_bytes(),
            "{kind:?} の内容が他の kind と混ざっている可能性がある"
        );
        assert_eq!(
            mode_of(&path),
            0o600,
            "{kind:?} 配下のファイルのパーミッションが 0o600 でない"
        );
    }
}

/// PROF-1: kind ごとに異なるファイル名で書き込み、各サブディレクトリの
/// `read_dir` 結果がその kind 自身のファイル名だけであることを確認する。
/// 他の kind のファイルが見えないことを示す（受入基準 B の別角度からの
/// 固定）。
#[test]
fn prof_1_data_dir_listing_contains_only_its_own_files() {
    let tmp = TempDir::new();
    let root = tmp.path().join("profile");
    let profile = Profile::open(&root).expect("open は成功する");

    for kind in DataKind::ALL.iter().copied() {
        let file_name = format!("{}-only.bin", kind.dir_name());
        profile
            .create_file_in(kind, std::ffi::OsStr::new(&file_name))
            .unwrap_or_else(|e| panic!("{kind:?} へのファイル作成が失敗した: {e:?}"));
    }

    for kind in DataKind::ALL.iter().copied() {
        let expected_name = format!("{}-only.bin", kind.dir_name());
        let names: BTreeSet<String> = std::fs::read_dir(profile.data_dir(kind))
            .unwrap_or_else(|e| panic!("{:?} の read_dir が失敗した: {e}", profile.data_dir(kind)))
            .map(|entry| {
                entry
                    .unwrap_or_else(|e| panic!("read_dir エントリ取得が失敗した: {e}"))
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let mut expected_names = BTreeSet::new();
        expected_names.insert(expected_name);
        assert_eq!(
            names, expected_names,
            "{kind:?} のディレクトリ一覧に他 kind のファイルが混ざっている、\
             または自身のファイルが見当たらない"
        );
    }
}
