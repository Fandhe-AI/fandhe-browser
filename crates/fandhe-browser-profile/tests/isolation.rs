//! `PROF-2`（TASK-51（51.1）・#181）が要求する「異なるルートで開いた 2 つの
//! `Profile` に並行書き込みしても、プロファイル間のデータ漏洩が 0 件」の
//! 入力パターンと fixture を定義するモジュール。
//!
//! ## 本ファイルの責務と 51.2（#182）との分担
//!
//! 本ファイル（TASK-51.1）が持つのは次の 3 点のみ:
//!
//! 1. 「アカウント A・B が同じサイトへアクセスする」状況を模した入力パターン
//!    表（[`all_cases`]。12 ケース、`P2-01`〜`P2-12`）
//!    と、その fixture（[`TempDir`]・[`open_profile_pair`]・[`snapshot`]）
//! 2. 漏洩の判定ロジック（[`Leak`]・[`find_leaks`]）
//! 3. 上記 1・2 それ自体の妥当性を確認するメタテスト（`prof_2_` 接頭辞）
//!
//! 実際に [`fandhe_browser_profile::Profile::create_file_in`] を呼んで
//! パターンを適用し、別スレッド・`Barrier` 同期での並行書き込みを行い、
//! 全ケースで漏洩 0 件をアサートするのは TASK-51.2（#182）の範囲であり、
//! 本ファイルはそれを行わない（したがって `apply_case` のような適用ヘルパーは
//! ここでは定義しない。定義して使わないと `-D warnings` の dead_code で
//! 落ちるため）。
//!
//! ## 「漏洩」の定義（[`find_leaks`] が判定する範囲）
//!
//! あるプロファイルのスナップショット（[`snapshot`]）について、以下のいずれかが
//! 1 件でもあれば漏洩とする。
//!
//! - (a) いずれかのファイル内容に、相手アカウントのマーカー文字列が部分一致で
//!   現れる（`foreign_markers`）
//! - (b) 相手アカウントだけが書くはずの名前のファイルが存在する
//!   （`foreign_only_paths`。例: `P2-08` で B 側に A のファイルが現れる）
//!
//! 判定できない場合（読み込み失敗等）は panic し、0 件に丸めない
//! （fail-closed。`coding-rust.md`）。
//!
//! ## `create_file_in` が `TRUNC` しないことと上書き手順（51.2 への申し送り）
//!
//! [`fandhe_browser_profile::Profile::create_file_in`] は
//! `RDWR | CREATE | NOFOLLOW | CLOEXEC | NONBLOCK` で開き、`TRUNC` を
//! 付けない。`P2-09`（上書き）を適用する 51.2 は、2 回目の書き込み前に
//! `File::set_len(0)` を呼んでから `write_all` する必要がある。そうしないと
//! 1 回目の内容が末尾に残り、「最終状態 = 最後に書いた値」という
//! `all_cases()` の期待（`steps_a` の末尾要素が最終状態）と食い違う。
//!
//! ## Windows を対象外にする理由
//!
//! `Profile::open` は Windows では ACL 隔離が未実装のため常に
//! `ProfileError::Unsupported` を返す（`XOS-7`〜`XOS-10`）。本ファイルは
//! 実際に `Profile::open`・ファイル作成を行う内容のため Unix 専用とする。
//! 3 OS での分離テスト実行（`XOS-10`）は Windows ACL 実装後の課題であり、
//! 本タスクのスコープ外。

#![cfg(unix)]

use fandhe_browser_profile::{DataKind, Profile, sanitize_component};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// `PROF-2` が定める、並行書き込みケース（`P2-12`）でスレッドごとに書き込む
/// 回数（各 200 回。PoC-7 で漏洩 0 件を確認済みの回数を踏襲する）。
/// 51.2 はこの定数を使って `Barrier` 同期後のループ回数を決める。
const PROF_2_CONCURRENT_WRITES: usize = 200;

/// ペイロード長の上限（無制限確保による DoS を防ぐ。coding-rust.md）。
const MAX_PAYLOAD_LEN: usize = 64 * 1024;

/// `PROF-2` が定める、想定する入力パターンの件数（受入条件「10 ケース以上」を
/// 満たすことを固定値として明示する）。
const EXPECTED_CASE_COUNT: usize = 12;

/// `Profile::open` を呼ぶ 2 つのアカウントの識別子（同じサイトへ別アカウントで
/// アクセスする状況を模す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Account {
    A,
    B,
}

/// 1 件の書き込みが持つ内容。マーカー文字列単体か、マーカーを含みつつ
/// 指定長まで埋めた内容かを選べる（`P2-11` の境界サイズケース用）。
#[derive(Debug, Clone)]
enum Payload {
    /// マーカー文字列そのものを内容とする。
    Marker(String),
    /// `marker` を先頭に含み、`len` バイトぴったりまで埋めた内容とする。
    Repeated { marker: String, len: usize },
}

impl Payload {
    /// この内容に埋め込まれたマーカー文字列を返す（漏洩判定・マーカー一意性
    /// 確認の両方で使う）。
    fn marker(&self) -> &str {
        match self {
            Payload::Marker(marker) => marker,
            Payload::Repeated { marker, .. } => marker,
        }
    }

    /// 実際にファイルへ書き込むバイト列を組み立てる。
    fn bytes(&self) -> Vec<u8> {
        match self {
            Payload::Marker(marker) => marker.as_bytes().to_vec(),
            Payload::Repeated { marker, len } => {
                let mut buf = marker.as_bytes().to_vec();
                assert!(
                    buf.len() <= *len,
                    "marker が要求 len より長い: marker={marker:?} len={len}"
                );
                while buf.len() < *len {
                    buf.push(b'x');
                }
                buf
            }
        }
    }
}

/// 1 件の書き込みを表す（`kind` ディレクトリ配下に `name` というファイルを
/// 作り、`payload` を書き込む）。51.2 が
/// `profile.create_file_in(entry.kind, OsStr::new(&entry.name))` へ渡す想定の
/// 最小単位。
#[derive(Debug, Clone)]
struct Entry {
    kind: DataKind,
    name: String,
    payload: Payload,
}

/// マーカー文字列を組み立てる（`dummy-acct-<who>-<id>` 形式）。実トークンに
/// 見える値を避けるため常に `dummy-` 接頭辞を付ける（security.md「秘密情報の
/// 混入防止」）。
fn marker(who: &str, id: &str) -> String {
    format!("dummy-acct-{who}-{id}")
}

/// [`Entry`] を組み立てる（`Payload::Marker` 版）。
fn entry(kind: DataKind, name: &str, marker: String) -> Entry {
    Entry {
        kind,
        name: name.to_string(),
        payload: Payload::Marker(marker),
    }
}

/// `P2-11`（境界サイズ）用の、ちょうど 255 バイトの名前を組み立てる
/// （`sanitize_component` が許す上限ぴったり）。
fn name_255_bytes() -> String {
    let prefix = "p2-11-boundary-";
    format!("{prefix}{}", "a".repeat(255 - prefix.len()))
}

/// 1 回の書き込みで終わるか、別スレッドで `Barrier` 同期しつつ繰り返すかを
/// 表す。`Sequential` は [`IsolationCase::steps_a`]/`steps_b` をそのまま順に
/// 適用する想定、`BarrierParallel` は `writes_per_profile` 回、両アカウントの
/// スレッドが `Barrier::wait` した直後から書き込みを始める想定（51.2 が
/// 実装する。ここでは回数の契約だけを固定する）。
#[derive(Debug, Clone, Copy)]
enum Mode {
    Sequential,
    BarrierParallel { writes_per_profile: usize },
}

/// 1 つの入力パターン（`PROF-2` が要求する「異なるアカウントで同じサイトへ
/// アクセスする」状況の 1 ケース）。
#[derive(Debug, Clone)]
struct IsolationCase {
    /// ケース識別子（`P2-01`〜`P2-12`）。
    id: &'static str,
    /// このケースが確認する観点の説明（日本語。テスト失敗時のメッセージに
    /// 使う）。
    description: &'static str,
    mode: Mode,
    /// アカウント A が行う書き込み（適用順。`P2-09` は同名への 2 回目が
    /// 最終状態になる）。
    steps_a: Vec<Entry>,
    /// アカウント B が行う書き込み（空なら B は何も書かない。`P2-08`）。
    steps_b: Vec<Entry>,
    /// 両アカウントの書き込みの間で、一度 `Profile` を drop してから
    /// 再度 `Profile::open` し直すことを 51.2 に要求するかどうか
    /// （再 open 後も隔離が保たれることの確認。`P2-01` のみ `true`）。
    reopen_between: bool,
}

/// `PROF-2` の入力パターン表（`P2-01`〜`P2-12`）。
///
/// 各ケースの `steps_a`・`steps_b` は「同じ `(kind, name)` に異なるマーカーを
/// 書く」ことで、2 アカウントが同じサイトへアクセスする状況を表す
/// （`Profile::create_file_in` が単一ファイル名しか受け付けないため、
/// 「サイト」はデータ種別ディレクトリ直下の 1 ファイルに対応させる）。
/// `const` ではなく関数にしているのは、`P2-11` の 255 バイト名・64 KiB
/// ペイロードを素直な実行時コード（`String::repeat`）で組み立てるため
/// （`const` コンテキストでは使えない）。
fn all_cases() -> Vec<IsolationCase> {
    vec![
        IsolationCase {
            id: "P2-01",
            description: "Cookie・同名: 両アカウントが cookies/example.com に別セッション値を書く",
            mode: Mode::Sequential,
            steps_a: vec![entry(
                DataKind::Cookies,
                "example.com",
                marker("a", "p2-01"),
            )],
            steps_b: vec![entry(
                DataKind::Cookies,
                "example.com",
                marker("b", "p2-01"),
            )],
            reopen_between: true,
        },
        IsolationCase {
            id: "P2-02",
            description: "Storage・同名: 両アカウントが同じオリジン相当の名前に別値を書く",
            mode: Mode::Sequential,
            steps_a: vec![entry(
                DataKind::Storage,
                "https_example.com_8443",
                marker("a", "p2-02"),
            )],
            steps_b: vec![entry(
                DataKind::Storage,
                "https_example.com_8443",
                marker("b", "p2-02"),
            )],
            reopen_between: false,
        },
        IsolationCase {
            id: "P2-03",
            description: "Cache・同名: 同じ URL 相当のキャッシュエントリ名に別本文を書く",
            mode: Mode::Sequential,
            steps_a: vec![entry(
                DataKind::Cache,
                "https_example.com_index.html",
                marker("a", "p2-03"),
            )],
            steps_b: vec![entry(
                DataKind::Cache,
                "https_example.com_index.html",
                marker("b", "p2-03"),
            )],
            reopen_between: false,
        },
        IsolationCase {
            id: "P2-04",
            description: "History・同名: 同じサイトの履歴エントリ名に別値を書く",
            mode: Mode::Sequential,
            steps_a: vec![entry(
                DataKind::History,
                "example.com_visit-0001",
                marker("a", "p2-04"),
            )],
            steps_b: vec![entry(
                DataKind::History,
                "example.com_visit-0001",
                marker("b", "p2-04"),
            )],
            reopen_between: false,
        },
        IsolationCase {
            id: "P2-05",
            description: "全 4 kind 同時: 1 サイト分を kind をまたいで書く（混線検出）",
            mode: Mode::Sequential,
            steps_a: vec![
                entry(
                    DataKind::Cookies,
                    "example.net",
                    marker("a", "p2-05-cookies"),
                ),
                entry(
                    DataKind::Storage,
                    "https_example.net_443",
                    marker("a", "p2-05-storage"),
                ),
                entry(
                    DataKind::Cache,
                    "https_example.net_index.html",
                    marker("a", "p2-05-cache"),
                ),
                entry(
                    DataKind::History,
                    "example.net_visit-0001",
                    marker("a", "p2-05-history"),
                ),
            ],
            steps_b: vec![
                entry(
                    DataKind::Cookies,
                    "example.net",
                    marker("b", "p2-05-cookies"),
                ),
                entry(
                    DataKind::Storage,
                    "https_example.net_443",
                    marker("b", "p2-05-storage"),
                ),
                entry(
                    DataKind::Cache,
                    "https_example.net_index.html",
                    marker("b", "p2-05-cache"),
                ),
                entry(
                    DataKind::History,
                    "example.net_visit-0001",
                    marker("b", "p2-05-history"),
                ),
            ],
            reopen_between: false,
        },
        IsolationCase {
            id: "P2-06",
            description: "複数サイト: 3 サイト x Cookie・Storage",
            mode: Mode::Sequential,
            steps_a: vec![
                entry(DataKind::Cookies, "example.com", marker("a", "p2-06-1")),
                entry(
                    DataKind::Storage,
                    "https_example.com_443",
                    marker("a", "p2-06-1"),
                ),
                entry(DataKind::Cookies, "example.org", marker("a", "p2-06-2")),
                entry(
                    DataKind::Storage,
                    "https_example.org_443",
                    marker("a", "p2-06-2"),
                ),
                entry(
                    DataKind::Cookies,
                    "shop.example.net",
                    marker("a", "p2-06-3"),
                ),
                entry(
                    DataKind::Storage,
                    "https_shop.example.net_443",
                    marker("a", "p2-06-3"),
                ),
            ],
            steps_b: vec![
                entry(DataKind::Cookies, "example.com", marker("b", "p2-06-1")),
                entry(
                    DataKind::Storage,
                    "https_example.com_443",
                    marker("b", "p2-06-1"),
                ),
                entry(DataKind::Cookies, "example.org", marker("b", "p2-06-2")),
                entry(
                    DataKind::Storage,
                    "https_example.org_443",
                    marker("b", "p2-06-2"),
                ),
                entry(
                    DataKind::Cookies,
                    "shop.example.net",
                    marker("b", "p2-06-3"),
                ),
                entry(
                    DataKind::Storage,
                    "https_shop.example.net_443",
                    marker("b", "p2-06-3"),
                ),
            ],
            reopen_between: false,
        },
        IsolationCase {
            id: "P2-07",
            description: "サブドメイン差: 似た名前（apex/www/api）でも混線しない",
            mode: Mode::Sequential,
            steps_a: vec![
                entry(DataKind::Cookies, "example.com", marker("a", "p2-07-apex")),
                entry(
                    DataKind::Cookies,
                    "www.example.com",
                    marker("a", "p2-07-www"),
                ),
                entry(
                    DataKind::Cookies,
                    "api.example.com",
                    marker("a", "p2-07-api"),
                ),
            ],
            steps_b: vec![
                entry(DataKind::Cookies, "example.com", marker("b", "p2-07-apex")),
                entry(
                    DataKind::Cookies,
                    "www.example.com",
                    marker("b", "p2-07-www"),
                ),
                entry(
                    DataKind::Cookies,
                    "api.example.com",
                    marker("b", "p2-07-api"),
                ),
            ],
            reopen_between: false,
        },
        IsolationCase {
            id: "P2-08",
            description: "片側のみ書き込み: A だけが書き、B の 4 データディレクトリは空のまま",
            mode: Mode::Sequential,
            steps_a: vec![entry(
                DataKind::Cookies,
                "example.com",
                marker("a", "p2-08"),
            )],
            steps_b: vec![],
            reopen_between: false,
        },
        IsolationCase {
            id: "P2-09",
            description: "上書き: A が同名へ 2 回書き、最終状態（末尾要素）だけが残る",
            mode: Mode::Sequential,
            steps_a: vec![
                entry(DataKind::Cookies, "example.com", marker("a", "p2-09-first")),
                entry(DataKind::Cookies, "example.com", marker("a", "p2-09-final")),
            ],
            steps_b: vec![entry(
                DataKind::Cookies,
                "example.com",
                marker("b", "p2-09"),
            )],
            reopen_between: false,
        },
        IsolationCase {
            id: "P2-10",
            description: "大文字小文字だけ異なる名前（SID/sid）: プロファイルをまたいで衝突しない",
            mode: Mode::Sequential,
            steps_a: vec![entry(DataKind::Cookies, "SID", marker("a", "p2-10"))],
            steps_b: vec![entry(DataKind::Cookies, "sid", marker("b", "p2-10"))],
            reopen_between: false,
        },
        IsolationCase {
            id: "P2-11",
            description: "境界サイズ: 255 バイトの名前 + MAX_PAYLOAD_LEN のペイロード",
            mode: Mode::Sequential,
            steps_a: vec![Entry {
                kind: DataKind::Cache,
                name: name_255_bytes(),
                payload: Payload::Repeated {
                    marker: marker("a", "p2-11"),
                    len: MAX_PAYLOAD_LEN,
                },
            }],
            steps_b: vec![Entry {
                kind: DataKind::Cache,
                name: name_255_bytes(),
                payload: Payload::Repeated {
                    marker: marker("b", "p2-11"),
                    len: MAX_PAYLOAD_LEN,
                },
            }],
            reopen_between: false,
        },
        IsolationCase {
            id: "P2-12",
            description: "並行（PROF-2 本体）: 別スレッドで Barrier 同期しつつ各 200 回書く",
            mode: Mode::BarrierParallel {
                writes_per_profile: PROF_2_CONCURRENT_WRITES,
            },
            steps_a: vec![entry(DataKind::Cookies, "site-seq", marker("a", "p2-12"))],
            steps_b: vec![entry(DataKind::Cookies, "site-seq", marker("b", "p2-12"))],
            reopen_between: false,
        },
    ]
}

/// `PROF-2` が定める `DataKind` ごとの固定ディレクトリ名。`kind.dir_name()` を
/// 呼ばず本体実装から独立して固定する（`tests/profile_open.rs` の
/// `PROF_1_LAYOUT` と同じ理由。期待値と実測値の両方を本体実装経由で作ると、
/// 対応関係の入れ替わりを検出できなくなる）。
const ISOLATION_LAYOUT: &[(DataKind, &str)] = &[
    (DataKind::Cookies, "cookies"),
    (DataKind::Storage, "storage"),
    (DataKind::Cache, "cache"),
    (DataKind::History, "history"),
];

/// テスト用の一時ディレクトリ。drop 時に再帰削除する（tempfile 等の外部
/// 依存は追加しない方針。dependency-policy.md）。`tests/profile_open.rs` と
/// 同じパターンだが、ファイル間の衝突を避けるため接頭辞を変える。
struct TempDir {
    path: PathBuf,
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // macOS の `/var` -> `/private/var` のような OS 標準 symlink を
        // `Profile::open` の厳格な検証が弾いてしまうため、基点を
        // canonicalize してから使う（`tests/profile_open.rs` と同じ理由）。
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let path = base.join(format!(
            "fandhe-profile-isolation-test-{}-{n}",
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

/// `tmp` 配下に兄弟ルート（`alice`・`alice2`）で 2 つの `Profile` を開く。
///
/// ディレクトリ名を意図的に「一方が他方の文字列 prefix」（`alice` は
/// `alice2` の prefix）にし、`assert_within_root`（`strip_prefix` ベースの
/// 判定）が文字列 prefix だけで境界外を誤って許容しないことを、全ケース
/// 共通の前提として作り込む。
fn open_profile_pair(tmp: &TempDir) -> (Profile, Profile) {
    let root_a = tmp.path().join("alice");
    let root_b = tmp.path().join("alice2");
    let profile_a = Profile::open(&root_a).unwrap_or_else(|e| panic!("alice の open に失敗: {e}"));
    let profile_b = Profile::open(&root_b).unwrap_or_else(|e| panic!("alice2 の open に失敗: {e}"));
    (profile_a, profile_b)
}

/// `root` 直下で `Profile::open` が作る固定エントリ名（データディレクトリ 4
/// 種に加え、`crate::lock::LOCK_FILE_NAME` は本体実装から独立して固定する
/// ため文字列で持つ。[`ISOLATION_LAYOUT`] と同じ理由）。
const ROOT_LOCK_FILE_NAME: &str = "profile.lock";

/// `root` 配下を走査し、相対パスと内容の組を集める。
///
/// - ルート直下は `ROOT_LOCK_FILE_NAME`（`profile.lock`）と
///   [`ISOLATION_LAYOUT`] が定める 4 データディレクトリだけが存在してよい。
///   それ以外のエントリ（相手アカウントのデータが誤った場所に書かれた
///   ものを含む）が見つかったら panic する（fail-closed。相手のデータが
///   データディレクトリの外に漏れているケースを走査対象外にして見逃さない）。
/// - 4 データディレクトリはいずれも欠落を許さない。`Profile::open` は
///   常にこの 4 つを作る契約であり、欠落は「空のスナップショット」ではなく
///   異常（テスト環境か本体側の想定外の状態）として panic する（fail-closed。
///   欠落を 0 件に丸めると漏洩判定側が「両者とも空だから漏洩なし」と
///   誤認しうる）。
/// - 各データディレクトリ自身が symlink の場合も panic する
///   （[`collect_regular_files`] の symlink 拒否は子エントリにしか
///   適用されないため、走査開始ディレクトリはここで別途
///   `symlink_metadata` により検証する）。
///
/// symlink・通常ファイル/ディレクトリ以外のエントリを見つけたら panic する
/// （fail-closed。`Profile` が symlink を作らない契約である以上、見つかった
/// 場合はテスト環境か本体側の想定外の状態であり、0 件に丸めて見逃さない）。
/// 読み込むファイルサイズは [`MAX_PAYLOAD_LEN`] を上限として検証する。
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let known_dir_names: std::collections::BTreeSet<&str> =
        ISOLATION_LAYOUT.iter().map(|&(_, name)| name).collect();

    let root_entries =
        std::fs::read_dir(root).unwrap_or_else(|e| panic!("read_dir({root:?}) が失敗した: {e}"));
    for entry in root_entries {
        let entry = entry.unwrap_or_else(|e| panic!("{root:?} の read_dir 走査に失敗した: {e}"));
        let path = entry.path();
        let name = entry.file_name();
        if name == OsStr::new(ROOT_LOCK_FILE_NAME) {
            continue;
        }
        match name.to_str() {
            Some(name_str) if known_dir_names.contains(name_str) => {}
            _ => panic!(
                "root 直下に想定外のエントリを検出した（相手アカウントのデータが誤った場所に書かれていないか要確認）: {path:?}"
            ),
        }
    }

    let mut out = BTreeMap::new();
    for &(_, dir_name) in ISOLATION_LAYOUT {
        let dir = root.join(dir_name);
        let meta = std::fs::symlink_metadata(&dir).unwrap_or_else(|e| {
            panic!("必須のデータディレクトリが欠落している（空のスナップショット扱いにしない）: {dir:?} ({e})")
        });
        if meta.file_type().is_symlink() {
            panic!("走査開始ディレクトリが symlink になっている（辿らない）: {dir:?}");
        }
        if !meta.is_dir() {
            panic!("走査開始ディレクトリが通常ディレクトリではない: {dir:?}");
        }
        collect_regular_files(&dir, root, &mut out);
    }
    out
}

fn collect_regular_files(dir: &Path, root: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir({dir:?}) が失敗した: {e}"));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("{dir:?} の read_dir 走査に失敗した: {e}"));
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)
            .unwrap_or_else(|e| panic!("symlink_metadata({path:?}) が失敗した: {e}"));
        if meta.file_type().is_symlink() {
            panic!("想定外の symlink を検出した（辿らない）: {path:?}");
        } else if meta.is_dir() {
            collect_regular_files(&path, root, out);
        } else if meta.is_file() {
            let len = meta.len();
            assert!(
                len <= MAX_PAYLOAD_LEN as u64,
                "ファイルサイズが上限を超えている: {path:?} ({len} bytes)"
            );
            let content =
                std::fs::read(&path).unwrap_or_else(|e| panic!("read({path:?}) が失敗した: {e}"));
            let relative = path
                .strip_prefix(root)
                .unwrap_or_else(|e| panic!("strip_prefix({root:?}) が失敗した: {e}"))
                .to_path_buf();
            out.insert(relative, content);
        } else {
            panic!("想定外のエントリ種別を検出した: {path:?}");
        }
    }
}

/// 漏洩の内訳（[`find_leaks`] の判定基準 (a)・(b) に対応する）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum LeakKind {
    /// (a) ファイル内容に相手アカウントのマーカーが部分一致で現れた。
    ForeignMarker(String),
    /// (b) 相手アカウントだけが書くはずの名前のファイルが存在した。
    ForeignOnlyFile,
}

/// 検出した漏洩 1 件（REPAIR-4: 真偽値ではなく内訳を持つ構造体で返す）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Leak {
    owner: Account,
    relative_path: PathBuf,
    kind: LeakKind,
}

/// `owner` のスナップショットに、相手アカウントの痕跡（漏洩）が無いかを
/// 判定する（[`ProfileError`] のようなエラー型は返さず、判定できない場合は
/// 呼び出し元で panic させる。fail-closed）。
///
/// - `foreign_markers`: 相手アカウントが書き込んだマーカー文字列の一覧。
///   いずれかのファイル内容に部分一致で含まれていれば
///   [`LeakKind::ForeignMarker`] を記録する
/// - `foreign_only_paths`: 相手アカウントだけが書くはずの相対パスの一覧
///   （例: `P2-08` で B 側スナップショットに A のファイルが現れないこと）。
///   `owner` のスナップショットにこのパスが存在すれば
///   [`LeakKind::ForeignOnlyFile`] を記録する
fn find_leaks(
    owner: Account,
    snapshot: &BTreeMap<PathBuf, Vec<u8>>,
    foreign_markers: &[&str],
    foreign_only_paths: &[PathBuf],
) -> Vec<Leak> {
    let mut leaks = Vec::new();
    for (path, content) in snapshot {
        for marker in foreign_markers {
            if contains_subslice(content, marker.as_bytes()) {
                leaks.push(Leak {
                    owner,
                    relative_path: path.clone(),
                    kind: LeakKind::ForeignMarker((*marker).to_string()),
                });
            }
        }
    }
    for path in foreign_only_paths {
        if snapshot.contains_key(path) {
            leaks.push(Leak {
                owner,
                relative_path: path.clone(),
                kind: LeakKind::ForeignOnlyFile,
            });
        }
    }
    leaks
}

/// `needle` が `haystack` の部分列として（バイト完全一致で）現れるかを返す。
/// 大文字小文字は区別しない OS 由来の差異を吸収しない（`P2-10` のような
/// 大文字小文字だけの違いを見逃さないため、常にバイト完全一致で比較する）。
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// `PROF-2`・TASK-51（51.1）・#181: 受入条件（10 ケース以上の入力パターンが
/// 定義されていること）そのものを固定する。
#[test]
fn prof_2_case_table_defines_at_least_10_cases() {
    // `EXPECTED_CASE_COUNT >= 10` は定数同士の比較のため、実行時の assert では
    // なくコンパイル時の const block で検証する（clippy
    // `assertions_on_constants` 対応）。
    const { assert!(EXPECTED_CASE_COUNT >= 10) };

    let cases = all_cases();
    assert_eq!(cases.len(), EXPECTED_CASE_COUNT);
    for case in &cases {
        assert!(
            !case.description.is_empty(),
            "case {} の description が空になっている",
            case.id
        );
    }
    let reopen_count = cases.iter().filter(|case| case.reopen_between).count();
    assert_eq!(
        reopen_count, 1,
        "reopen_between=true のケースは P2-01 の 1 件のみを想定している"
    );
}

/// `PROF-2`・TASK-51（51.1）: ケース ID に重複が無い。
#[test]
fn prof_2_case_ids_are_unique() {
    let cases = all_cases();
    let ids: std::collections::BTreeSet<&str> = cases.iter().map(|case| case.id).collect();
    assert_eq!(ids.len(), cases.len(), "ケース ID に重複がある");
}

/// `PROF-2`・TASK-51（51.1）: 全エントリ名が `sanitize_component`（`PROF-4`）を
/// 通る。`create_file_in` の前提条件であり、51.2 を確実に動かすための
/// 最重要チェック。
#[test]
fn prof_2_all_entry_names_pass_sanitize_component() {
    for case in all_cases() {
        for entry in case.steps_a.iter().chain(case.steps_b.iter()) {
            sanitize_component(OsStr::new(&entry.name)).unwrap_or_else(|e| {
                panic!(
                    "case {}（{}）の name {:?} が sanitize_component を通らない: {e}",
                    case.id, case.description, entry.name
                )
            });
        }
    }
}

/// `PROF-2`・TASK-51（51.1）: A 側のマーカーが B 側のどのマーカーの部分文字列
/// にもならず、その逆もない（漏洩検出の感度を保証する）。
#[test]
fn prof_2_markers_are_distinct_between_accounts() {
    for case in all_cases() {
        let a_markers: Vec<&str> = case.steps_a.iter().map(|e| e.payload.marker()).collect();
        let b_markers: Vec<&str> = case.steps_b.iter().map(|e| e.payload.marker()).collect();
        for a_marker in &a_markers {
            for b_marker in &b_markers {
                assert!(
                    !contains_subslice(b_marker.as_bytes(), a_marker.as_bytes()),
                    "case {}: A のマーカー {a_marker:?} が B のマーカー {b_marker:?} に含まれる",
                    case.id
                );
                assert!(
                    !contains_subslice(a_marker.as_bytes(), b_marker.as_bytes()),
                    "case {}: B のマーカー {b_marker:?} が A のマーカー {a_marker:?} に含まれる",
                    case.id
                );
            }
        }
    }
}

/// `PROF-2`・TASK-51（51.1）: パターン表全体で [`ISOLATION_LAYOUT`] の 4 kind が
/// すべて使われており、`BarrierParallel { writes_per_profile: 200 }` の
/// ケースが 1 件以上ある。
#[test]
fn prof_2_case_table_covers_all_data_kinds_and_parallel_mode() {
    let cases = all_cases();
    let mut kinds_seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut parallel_seen = false;

    for case in &cases {
        for entry in case.steps_a.iter().chain(case.steps_b.iter()) {
            let dir_name = ISOLATION_LAYOUT
                .iter()
                .find(|(kind, _)| *kind == entry.kind)
                .map(|(_, name)| *name)
                .unwrap_or_else(|| panic!("ISOLATION_LAYOUT に {:?} が無い", entry.kind));
            kinds_seen.insert(dir_name);
        }
        if let Mode::BarrierParallel { writes_per_profile } = case.mode {
            assert_eq!(writes_per_profile, PROF_2_CONCURRENT_WRITES);
            parallel_seen = true;
        }
    }

    let expected: std::collections::BTreeSet<&str> =
        ISOLATION_LAYOUT.iter().map(|(_, name)| *name).collect();
    assert_eq!(kinds_seen, expected, "使われていない DataKind がある");
    assert!(parallel_seen, "BarrierParallel のケースが 1 件も無い");
    assert_eq!(PROF_2_CONCURRENT_WRITES, 200);
}

/// `PROF-2`・TASK-51（51.1）: 全ペイロードの長さが [`MAX_PAYLOAD_LEN`] 以下で、
/// `P2-11` はちょうど名前 255 バイト・ペイロード上限値ちょうどになっている。
#[test]
fn prof_2_payload_sizes_within_limit() {
    let mut p2_11_checked = false;
    for case in all_cases() {
        for entry in case.steps_a.iter().chain(case.steps_b.iter()) {
            let bytes = entry.payload.bytes();
            assert!(
                bytes.len() <= MAX_PAYLOAD_LEN,
                "case {} の payload が上限を超えている: {} bytes",
                case.id,
                bytes.len()
            );
            assert!(
                entry.name.len() <= 255,
                "case {} の name が 255 バイトを超えている",
                case.id
            );
        }
        if case.id == "P2-11" {
            for entry in case.steps_a.iter().chain(case.steps_b.iter()) {
                assert_eq!(
                    entry.name.len(),
                    255,
                    "P2-11 の name はちょうど 255 バイトを想定している"
                );
                match &entry.payload {
                    Payload::Repeated { len, .. } => {
                        assert_eq!(
                            *len, MAX_PAYLOAD_LEN,
                            "P2-11 の payload は上限ちょうどを想定している"
                        );
                    }
                    Payload::Marker(_) => panic!("P2-11 は Payload::Repeated を使う想定"),
                }
            }
            p2_11_checked = true;
        }
    }
    assert!(p2_11_checked, "P2-11 ケースが見つからない");
}

/// `PROF-2`・TASK-51（51.1）: 合成したインメモリのスナップショットに相手の
/// マーカーを 1 件埋め込むと、`Leak` が 1 件（owner・relative_path・marker の
/// 具体値つき）返る。
#[test]
fn prof_2_find_leaks_detects_foreign_marker() {
    let mut snap = BTreeMap::new();
    let path = PathBuf::from("cookies/example.com");
    let foreign_marker = "dummy-acct-b-p2-01";
    snap.insert(
        path.clone(),
        format!("session={foreign_marker}").into_bytes(),
    );

    let leaks = find_leaks(Account::A, &snap, &[foreign_marker], &[]);

    assert_eq!(leaks.len(), 1);
    assert_eq!(leaks[0].owner, Account::A);
    assert_eq!(leaks[0].relative_path, path);
    assert_eq!(
        leaks[0].kind,
        LeakKind::ForeignMarker(foreign_marker.to_string())
    );
}

/// `PROF-2`・TASK-51（51.1）: 汚染の無いスナップショットでは空の `Vec` が
/// 返る（自分自身のマーカーだけが含まれる場合を含む）。A 側・B 側の両方向で
/// 確認し、判定が owner の向きに依存しない（対称である）ことも併せて示す。
#[test]
fn prof_2_find_leaks_returns_empty_for_clean_snapshot() {
    let mut snap_a = BTreeMap::new();
    snap_a.insert(
        PathBuf::from("cookies/example.com"),
        b"dummy-acct-a-p2-01".to_vec(),
    );
    let leaks_a = find_leaks(Account::A, &snap_a, &["dummy-acct-b-p2-01"], &[]);
    assert!(leaks_a.is_empty());

    let mut snap_b = BTreeMap::new();
    snap_b.insert(
        PathBuf::from("cookies/example.com"),
        b"dummy-acct-b-p2-01".to_vec(),
    );
    let leaks_b = find_leaks(Account::B, &snap_b, &["dummy-acct-a-p2-01"], &[]);
    assert!(leaks_b.is_empty());
}

/// `PROF-2`・TASK-51（51.1）: `open_profile_pair` で 2 つとも `Ok` になり、
/// root が互いに異なり、一方が他方の配下にならず、開いた直後の
/// `snapshot` は両方とも空である（`profile.lock` を除外できていることも
/// 含む）。
#[test]
fn prof_2_fixture_opens_two_sibling_profiles() {
    let tmp = TempDir::new();
    let (profile_a, profile_b) = open_profile_pair(&tmp);

    assert_ne!(profile_a.root(), profile_b.root());
    assert!(!profile_a.root().starts_with(profile_b.root()));
    assert!(!profile_b.root().starts_with(profile_a.root()));

    assert!(snapshot(profile_a.root()).is_empty());
    assert!(snapshot(profile_b.root()).is_empty());
}
