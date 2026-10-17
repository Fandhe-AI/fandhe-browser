//! Windows 限定: 基準ディレクトリが環境変数 `LOCALAPPDATA` ではなく既知フォルダ API
//! （`FOLDERID_LocalAppData`）で決まることの結合テスト（Issue #951。ビヘイビア `PROF-6`・
//! `XOS-7`・`XOS-10`、TASK-63 の前提・MS-5）。
//!
//! 親の環境は変えず（edition 2024 の `set_var` は `unsafe` で、並列の他テストへ漏れるため）、
//! 自分自身（`current_exe()`）を子プロセスとして再実行し、環境変数は `Command::env` /
//! `env_remove` で子にだけ渡す（`lock_cross_platform.rs` と同じ方式）。結果は終了コードで返し、
//! 想定外のエラーを成功扱いにしない。公開 API（`Profile::open`・`OsDefaultStore`）だけを使う。
//!
//! ## シナリオ
//!
//! - (a) `LOCALAPPDATA` を、実在し末尾が `AppData\Local` の偽の場所へ向ける。変更前のコードなら
//!   受理していた条件のため、変更の有無を判別できる。`Profile::open` は `Unsupported`、偽の場所には
//!   何も作られず、既定ルートも偽の場所の配下にならない。
//! - (b) `LOCALAPPDATA` を外す。既知フォルダ配下（`TEMP`）のルートで `Profile::open` が成功する。
//! - (c) `USERPROFILE` を偽の場所へ向ける観測。レジストリの値は `%USERPROFILE%\AppData\Local`
//!   （REG_EXPAND_SZ）で、`SHGetKnownFolderPath` がプロセスの環境で展開するかは一次情報で
//!   確定していない。観測結果（基準が動いたか）は標準エラーへ出し、CI の記録に残す
//!   （`REPAIR-3`: 未確定の挙動を断定しない）。観測は 3 値（基準が動かない・動いた・既知フォルダ
//!   が解決不能で fail-closed）のいずれかであることだけを要求し、想定外のエラーは失敗にする。
//!   展開先 `%USERPROFILE%\AppData\Local` が存在しないと、`KF_FLAG_DEFAULT` の
//!   `SHGetKnownFolderPath` は検証に失敗し `resolve_root` が `DefaultRootUnavailable` になる
//!   （main の CI で観測。run 39009500988）。これは偽の場所へ書かない側の結果で正当。

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use fandhe_browser_profile::{OsDefaultStore, Profile, ProfileError, ProfileStore};

const ENV_MODE: &str = "FANDHE_PROFILE_KF_CHILD_MODE";
const ENV_ROOT: &str = "FANDHE_PROFILE_KF_ROOT";

const MODE_FAKE_LOCALAPPDATA: &str = "fake_localappdata";
const MODE_NO_LOCALAPPDATA: &str = "no_localappdata";
const MODE_FAKE_USERPROFILE: &str = "fake_userprofile";

/// 期待どおり（(a): 拒否され何も作られない。(b): 開けて `profile.lock` ができる）。
const EXIT_OK: i32 = 10;
/// (c): 基準が `USERPROFILE` に従わなかった（既知フォルダのまま）。
const EXIT_BASE_UNCHANGED: i32 = 11;
/// (c): 基準が `USERPROFILE` に従った。
const EXIT_BASE_FOLLOWED: i32 = 12;
/// (c): 既知フォルダが解決できず `DefaultRootUnavailable`（fail-closed。偽の場所へは書かない）。
const EXIT_BASE_UNRESOLVED: i32 = 14;
/// 想定外のエラー・前提違反。
const EXIT_UNEXPECTED: i32 = 13;

/// 子のエントリ。環境変数が無い通常の `cargo test` では何もせず成功する。
#[test]
fn known_folder_child_entry() {
    let (Some(mode), Some(root)) = (std::env::var_os(ENV_MODE), std::env::var_os(ENV_ROOT)) else {
        return;
    };
    let root = PathBuf::from(root);
    let code = match mode.to_str() {
        Some(MODE_FAKE_LOCALAPPDATA) => child_fake_localappdata(&root),
        Some(MODE_NO_LOCALAPPDATA) => child_no_localappdata(&root),
        Some(MODE_FAKE_USERPROFILE) => child_fake_userprofile(&root),
        _ => EXIT_UNEXPECTED,
    };
    std::process::exit(code);
}

fn child_fake_localappdata(root: &Path) -> i32 {
    // 偽の基準の配下へ開こうとして `Unsupported` になり、何も作られないこと。
    match Profile::open(root) {
        Err(ProfileError::Unsupported { .. }) => {}
        _ => return EXIT_UNEXPECTED,
    }
    if root.exists() {
        return EXIT_UNEXPECTED;
    }
    // 既定ルートも偽の場所（環境変数が指す先）の配下にならない。
    let Some(fake_base) = root.parent() else {
        return EXIT_UNEXPECTED;
    };
    match OsDefaultStore::new().resolve_root() {
        Ok(resolved) if !resolved.path().starts_with(fake_base) => EXIT_OK,
        _ => EXIT_UNEXPECTED,
    }
}

fn child_no_localappdata(root: &Path) -> i32 {
    if std::env::var_os("LOCALAPPDATA").is_some() {
        return EXIT_UNEXPECTED;
    }
    match Profile::open(root) {
        Ok(_profile) if root.join("profile.lock").is_file() => EXIT_OK,
        _ => EXIT_UNEXPECTED,
    }
}

fn child_fake_userprofile(fake_profile: &Path) -> i32 {
    let resolved = match OsDefaultStore::new().resolve_root() {
        Ok(resolved) => resolved,
        Err(ProfileError::DefaultRootUnavailable { .. }) => return EXIT_BASE_UNRESOLVED,
        Err(_) => return EXIT_UNEXPECTED,
    };
    if resolved.path().starts_with(fake_profile) {
        EXIT_BASE_FOLLOWED
    } else {
        EXIT_BASE_UNCHANGED
    }
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 後始末付きの作業ディレクトリ（`std::fs` のみ。外部依存を足さない）。
struct Scratch(PathBuf);

impl Scratch {
    fn under(base: &Path) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        Scratch(base.join(format!("fandhe-951-{}-{n}", std::process::id())))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_child(mode: &str, root: &Path, tweak: impl FnOnce(&mut Command)) -> i32 {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    cmd.args(["known_folder_child_entry", "--exact", "--test-threads=1"])
        .env(ENV_MODE, mode)
        .env(ENV_ROOT, root);
    tweak(&mut cmd);
    cmd.status()
        .expect("spawn child")
        .code()
        .expect("child exit code")
}

/// 親（環境を変えていない）の既知フォルダの実体の推定。`TEMP` は通常
/// `...\AppData\Local\Temp` なので、その 1 つ上。
fn real_base() -> PathBuf {
    std::fs::canonicalize(std::env::temp_dir())
        .expect("canonicalize TEMP")
        .parent()
        .expect("TEMP parent")
        .to_path_buf()
}

/// (a) 環境変数を偽の場所へ向けても基準は変わらない。
#[test]
fn prof6_xos10_env_var_cannot_redirect_base_directory() {
    let mut drive = PathBuf::from(std::env::var_os("SystemDrive").expect("SystemDrive"));
    drive.push("\\");
    let scratch = Scratch::under(&drive);
    let fake = scratch.0.join("AppData").join("Local");
    std::fs::create_dir_all(&fake).expect("create fake AppData\\Local");
    let fake_canon = std::fs::canonicalize(&fake).expect("canonicalize fake");
    let real = real_base();
    assert!(
        !fake_canon.starts_with(&real) && !real.starts_with(&fake_canon),
        "テストの前提: 偽の場所 ({fake_canon:?}) が実体 ({real:?}) と無関係であること"
    );

    let root = fake.join("p");
    let code = run_child(MODE_FAKE_LOCALAPPDATA, &root, |c| {
        c.env("LOCALAPPDATA", &fake);
    });
    assert_eq!(code, EXIT_OK, "(a) の子の終了コード");
    assert!(!root.exists(), "偽の場所には何も作られない");
}

/// (b) 環境変数を外しても、既知フォルダ配下のルートは開ける。
#[test]
fn xos7_open_succeeds_without_localappdata_env() {
    let scratch = Scratch::under(&std::env::temp_dir());
    let root = scratch.0.join("p");
    let code = run_child(MODE_NO_LOCALAPPDATA, &root, |c| {
        c.env_remove("LOCALAPPDATA");
    });
    assert_eq!(code, EXIT_OK, "(b) の子の終了コード");
    assert!(root.join("profile.lock").is_file());
}

/// (c) `USERPROFILE` を偽の場所へ向けたときの基準の動きを観測する（標準エラーに記録）。
#[test]
fn xos7_observe_userprofile_expansion_of_known_folder() {
    let scratch = Scratch::under(&std::env::temp_dir());
    let fake_profile = scratch.0.join("fakeprofile");
    std::fs::create_dir_all(&fake_profile).expect("create fake profile");
    let code = run_child(MODE_FAKE_USERPROFILE, &fake_profile, |c| {
        c.env("USERPROFILE", &fake_profile);
    });
    match code {
        EXIT_BASE_UNCHANGED => {
            eprintln!("[known-folder-observation] USERPROFILE does NOT move the base directory")
        }
        EXIT_BASE_FOLLOWED => {
            eprintln!("[known-folder-observation] USERPROFILE DOES move the base directory")
        }
        EXIT_BASE_UNRESOLVED => {
            eprintln!(
                "[known-folder-observation] USERPROFILE expansion made the known folder unresolvable (fail-closed)"
            )
        }
        other => panic!("(c) の子が想定外の終了コード {other}"),
    }
}
