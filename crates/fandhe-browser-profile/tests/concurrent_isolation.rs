//! 並行アクセステストハーネス（`PROF-3`・TASK-52（52.1）・#184、MS-3）。
//!
//! `PROF-3` は「5 つの `Profile` インスタンスを `Barrier` で同時に開始させ、
//! それぞれ 100 回ずつ並行に書き込んだとき、データ混線 0 件・競合による
//! panic/クラッシュ 0 件であること」を定める（PoC-7
//! `test_five_profiles_concurrent_no_crash_no_mixup` が土台）。本ファイルは
//! そのハーネス機構（複数プロファイルの生成・`Barrier` による同時開始・
//! 書き込みループ・結果の構造化収集・panic の捕捉）のみを実装し、
//! **`PROF-3` を検証済みとは主張しない**（REPAIR-3・code-comment-style.md）。
//! 混線 0 件・クラッシュ 0 件の assert は後続の TASK-52（52.2）・#185 が
//! `run_concurrent_writes` の戻り値を読み戻して追加する。
//!
//! 本体（`src/`）は変更しない。新規外部依存も追加しない
//! （dependency-policy.md）。
//!
//! Windows では ACL 隔離（`XOS-7`〜`XOS-10`）が未実装のため `Profile::open`
//! は常に `ProfileError::Unsupported` を返す（`tests/profile_open.rs` と同じ
//! 契約。この契約は `src/profile.rs` の `#[cfg(windows)]` テストが確認して
//! いる）。本ファイルはディレクトリが実際に作られ書き込まれることを前提と
//! するため Unix 専用とする。

#![cfg(unix)]

use fandhe_browser_profile::{DataKind, Profile};
use std::ffi::OsStr;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// `PROF-3` が定める最低並行数（「5 並行以上で複数 Profile へ同時に書き込める
/// こと」）。
const MIN_CONCURRENCY: usize = 5;
/// `PROF-3` が定める検証対象のプロファイル数。
const PROFILE_COUNT: usize = 5;
/// `PROF-3` が定める、1 プロファイルあたりの書き込み回数。
const WRITES_PER_PROFILE: usize = 100;
/// 全プロファイル・全 `DataKind` で共通のファイル名。どのワーカーも同じ
/// 相対パス（`<kind のディレクトリ>/concurrent.dat`）へ書き込むため、52.2 で
/// 他プロファイルのペイロードが混入していないかを検出できる。
const TARGET_FILE_NAME: &str = "concurrent.dat";

/// テスト用の一時ディレクトリ。drop 時に再帰削除する（`tests/profile_open.rs`
/// と同じパターン。tempfile 等の外部依存は追加しない方針。
/// dependency-policy.md）。
struct TempDir {
    path: PathBuf,
}

static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

impl TempDir {
    fn new() -> Self {
        let n = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        // macOS の `/var` -> `/private/var` のような OS 標準 symlink を
        // 経由すると `Profile::open` の厳格な symlink 検証（PR #437
        // レビュー指摘）に弾かれるため、基点を事前に正規化する
        // （`tests/profile_open.rs` と同じ理由）。
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let path = base.join(format!(
            "fandhe-profile-concurrent-test-{}-{n}",
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

/// ハーネス実行時の並行度設定。
struct HarnessConfig {
    profiles: usize,
    writes_per_profile: usize,
}

/// 1 ワーカー（1 プロファイル）の実行結果（`PROF-3`・TASK-52（52.1））。
///
/// 52.2（#185）が読み戻す情報の発生源。フィールドは本ファイルの §T1 で
/// すべて読む（dead_code を避けるため。52.2 でしか使わない情報はここでは
/// 定義しない）。
struct WorkerReport {
    index: usize,
    /// このワーカーが開いたプロファイルのルート（表示・52.2 の読み戻し用）。
    root: PathBuf,
    writes_attempted: usize,
    writes_succeeded: usize,
    /// 書き込み失敗（`ProfileError`・`io::Error` の `Display`）。成功に
    /// 丸めず、件数を正確に数えるため記録する（security.md「偽装の禁止」）。
    write_errors: Vec<String>,
    /// 各 `DataKind` について最後に書き込みに成功したペイロード。
    /// 52.2 での期待値の発生源になる。
    last_payloads: Vec<(DataKind, String)>,
}

/// ワーカーの終わり方。panic・open 失敗を `Completed` に丸めない
/// （security.md「偽装の禁止」: 検出回避として作用する成功の一律返却を
/// 避ける方針と同様、ハーネス自身も失敗を成功として報告しない）。
enum WorkerOutcome {
    Completed(WorkerReport),
    OpenFailed { index: usize, error: String },
    Panicked { index: usize, message: String },
}

/// `config` の並行数を検証する。`PROF-3` は「5 並行以上」を要求するため、
/// これを下回る設定でハーネスを呼ぶことはテストコードの誤りとみなし、
/// テストヘルパーとして `panic!` で打ち切る。
fn validate_config(config: &HarnessConfig) {
    assert!(
        config.profiles >= MIN_CONCURRENCY,
        "PROF-3 は並行プロファイル数を {MIN_CONCURRENCY} 以上要求する（got: {}）",
        config.profiles
    );
    assert!(
        config.writes_per_profile > 0,
        "writes_per_profile は 1 以上である必要がある"
    );
}

/// `base` 配下に `config.profiles` 個のプロファイルを作り、`Barrier` で
/// 同時に開始させて、それぞれ `config.writes_per_profile` 回ずつ並行に
/// 書き込む（`PROF-3`・TASK-52（52.1））。
///
/// 処理の流れ（不変条件を含む）:
/// 1. `std::thread::scope` の中で `config.profiles` 本のワーカーを spawn
///    する。`Barrier::new(config.profiles)` はスコープ内で参照を共有する
///    （`Arc` は不要）。
/// 2. 各ワーカー `i` は `base.join("profile-{i}")` に `Profile::open` する。
///    **open の成否に関わらず `barrier.wait()` をちょうど 1 回呼ぶ**。open が
///    失敗したワーカーが wait せずに return すると、残りのワーカーが
///    永久にブロックする（libtest にはテスト単位のタイムアウトがないため、
///    ここが deadlock の最大の危険箇所になる）。open が失敗したワーカーは
///    wait の後で `OpenFailed` を返す。この不変条件は `Profile::open` が
///    パニックせず `Result` を返すこと（coding-rust.md「ライブラリコードでは
///    `Result` を返し、panic させない」）を前提にしている。`open` 自体が
///    パニックした場合はこの wait に到達できず、残りのワーカーがブロック
///    したままになる（52.2 が検出したい `Panicked` そのものではなく、
///    ハーネス側の deadlock として現れる点に注意）。
/// 3. 書き込みは `DataKind::ALL` を順に巡回しながら `writes_per_profile` 回
///    行う。1 回の書き込みは
///    `create_file_in` → `set_len(0)`（過去の内容の残留を防ぐ。
///    `create_file_in` は `RDWR|CREATE` で開くため切り詰めない） →
///    `seek(Start(0))` → `write_all` の順に行う。52.2 が読み戻すのは
///    同一プロセス内（同じページキャッシュ）のため `sync_data` によるディスク
///    同期は不要と判断し、行わない（3 OS CI・特に macOS での所要時間を
///    抑えるため。#184 レビュー指摘）。失敗しても打ち切らず
///    `write_errors` に記録し、件数を正確に数える。
/// 4. 全ハンドルを `join()` する。パニックは伝播させず `Panicked` として記録
///    する。テストは dev/test プロファイル（unwind）でビルドされるため、
///    `[profile.release]` の `panic = "abort"` はここには効かない。スレッドの
///    join 結果で panic を観測しているだけで `catch_unwind` は使わない
///    （coding-rust.md と矛盾しない）。
/// 5. 戻り値は `index` の昇順に並べて返す。
fn run_concurrent_writes(base: &Path, config: &HarnessConfig) -> Vec<WorkerOutcome> {
    validate_config(config);

    let barrier = Barrier::new(config.profiles);

    thread::scope(|scope| {
        let handles: Vec<_> = (0..config.profiles)
            .map(|index| {
                let barrier = &barrier;
                let writes_per_profile = config.writes_per_profile;
                let root = base.join(format!("profile-{index}"));
                scope.spawn(move || -> WorkerOutcome {
                    let profile = match Profile::open(&root) {
                        Ok(profile) => profile,
                        Err(err) => {
                            // open に失敗しても、他のワーカーを deadlock
                            // させないため必ず wait する。
                            barrier.wait();
                            return WorkerOutcome::OpenFailed {
                                index,
                                error: err.to_string(),
                            };
                        }
                    };

                    barrier.wait();

                    let mut writes_succeeded = 0usize;
                    let mut write_errors = Vec::new();
                    let mut last_payloads: Vec<(DataKind, String)> = Vec::new();

                    for j in 0..writes_per_profile {
                        let Some(&kind) = DataKind::ALL.get(j % DataKind::ALL.len()) else {
                            // `DataKind::ALL` は空になり得ないが、外部入力に
                            // 準じた扱いとして `.get()` で明示的に処理する
                            // （coding-rust.md「エラーハンドリング」）。
                            write_errors.push("DataKind::ALL is unexpectedly empty".to_string());
                            continue;
                        };
                        let payload = format!("profile-{index}:write-{j:04}\n");
                        match write_once(&profile, kind, payload.as_bytes()) {
                            Ok(()) => {
                                writes_succeeded += 1;
                                if let Some(slot) =
                                    last_payloads.iter_mut().find(|(k, _)| *k == kind)
                                {
                                    slot.1 = payload;
                                } else {
                                    last_payloads.push((kind, payload));
                                }
                            }
                            Err(message) => write_errors.push(message),
                        }
                    }

                    // `profile` を明示的に drop してロックを解放してから
                    // 結果を返す（他ワーカーの join・後続の再 open に影響
                    // させないため）。
                    drop(profile);

                    WorkerOutcome::Completed(WorkerReport {
                        index,
                        root,
                        writes_attempted: writes_per_profile,
                        writes_succeeded,
                        write_errors,
                        last_payloads,
                    })
                })
            })
            .collect();

        let mut outcomes: Vec<WorkerOutcome> = handles
            .into_iter()
            .enumerate()
            .map(|(index, handle)| match handle.join() {
                Ok(outcome) => outcome,
                Err(payload) => WorkerOutcome::Panicked {
                    index,
                    message: panic_message(&payload),
                },
            })
            .collect();

        outcomes.sort_by_key(|outcome| match outcome {
            WorkerOutcome::Completed(report) => report.index,
            WorkerOutcome::OpenFailed { index, .. } => *index,
            WorkerOutcome::Panicked { index, .. } => *index,
        });
        outcomes
    })
}

/// 1 回分の書き込みを行う。`profile.create_file_in` が `RDWR|CREATE` で開き
/// 切り詰めないため、短い内容で上書きしたときに古いバイトが残らないよう
/// `set_len(0)` で明示的に切り詰めてから書く（52.2 の読み戻しで最終内容を
/// 一意にするために必須）。
fn write_once(profile: &Profile, kind: DataKind, payload: &[u8]) -> Result<(), String> {
    let mut file = profile
        .create_file_in(kind, OsStr::new(TARGET_FILE_NAME))
        .map_err(|err| err.to_string())?;
    file.set_len(0).map_err(|err| err.to_string())?;
    file.seek(SeekFrom::Start(0))
        .map_err(|err| err.to_string())?;
    file.write_all(payload).map_err(|err| err.to_string())?;
    Ok(())
}

/// `thread::Result` の `Err` ペイロード（`Box<dyn Any + Send>`）から panic
/// メッセージを取り出す。`&str`・`String` のいずれでもない場合は固定文言を
/// 返す（外部入力ではないが、想定外の型を弾く防御的な処理）。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "worker thread panicked with a non-string payload".to_string()
    }
}

// ---------------------------------------------------------------------
// 52.1 のテスト: ハーネスの機構確認（具体値で assert する）。
//
// 注意: ここでの `last_payloads` の assert はハーネスが記録した値の正しさの
// 確認であり、ディスク上の分離（混線が起きていないか）の検証ではない。
// ディスクの読み戻しによる混線確認は 52.2（#185）の範囲。
// ---------------------------------------------------------------------

/// `PROF-3`・TASK-52（52.1）・#184: `PROFILE_COUNT` 個のプロファイルを
/// `WRITES_PER_PROFILE` 回ずつ並行に書き込ませると、5 件すべてが
/// `Completed` で返り、書き込み試行・成功回数、記録されたペイロードが
/// 期待どおりであること。
#[test]
fn prof_3_harness_runs_five_profiles_concurrently() {
    let tmp = TempDir::new();
    let config = HarnessConfig {
        profiles: PROFILE_COUNT,
        writes_per_profile: WRITES_PER_PROFILE,
    };

    let outcomes = run_concurrent_writes(tmp.path(), &config);

    assert_eq!(outcomes.len(), PROFILE_COUNT, "outcome の件数が想定と違う");

    let mut seen_roots = std::collections::BTreeSet::new();
    for (expected_index, outcome) in outcomes.into_iter().enumerate() {
        let report = match outcome {
            WorkerOutcome::Completed(report) => report,
            WorkerOutcome::OpenFailed { index, error } => {
                panic!("worker {index} open failed: {error}")
            }
            WorkerOutcome::Panicked { index, message } => {
                panic!("worker {index} panicked: {message}")
            }
        };

        assert_eq!(report.index, expected_index, "index が昇順に並んでいない");
        assert_eq!(
            report.root,
            tmp.path().join(format!("profile-{expected_index}")),
            "root が想定のパスと一致しない"
        );
        assert!(
            seen_roots.insert(report.root.clone()),
            "root が他のワーカーと重複している: {:?}",
            report.root
        );
        assert_eq!(
            report.writes_attempted, WRITES_PER_PROFILE,
            "worker {expected_index}: writes_attempted が想定と違う"
        );
        assert_eq!(
            report.writes_succeeded, WRITES_PER_PROFILE,
            "worker {expected_index}: writes_succeeded が想定と違う"
        );
        assert!(
            report.write_errors.is_empty(),
            "worker {expected_index}: write_errors が空でない: {:?}",
            report.write_errors
        );

        // `WRITES_PER_PROFILE`（100）回を `DataKind::ALL`（4 種）で巡回した
        // 場合、各 kind の最後の j は 96..99 のいずれかになる。
        assert_eq!(
            report.last_payloads.len(),
            DataKind::ALL.len(),
            "worker {expected_index}: last_payloads が DataKind::ALL の全種を含まない"
        );
        for (kind_index, &kind) in DataKind::ALL.iter().enumerate() {
            let last_j = last_write_index_for_kind(kind_index, DataKind::ALL.len());
            let expected_payload = format!("profile-{expected_index}:write-{last_j:04}\n");
            let actual_payload = report
                .last_payloads
                .iter()
                .find(|(k, _)| *k == kind)
                .map(|(_, payload)| payload.as_str());
            assert_eq!(
                actual_payload,
                Some(expected_payload.as_str()),
                "worker {expected_index}: {kind:?} の last_payloads が期待値と一致しない"
            );
        }
    }
}

/// `WRITES_PER_PROFILE` 回を `DataKind::ALL` で巡回したとき、`kind_index`
/// 番目の種別が最後に選ばれる `j` を計算する（`j % kind_count == kind_index`
/// を満たす最大の `j < WRITES_PER_PROFILE`）。テストの期待値をハードコード
/// せず、定数から導出することで `WRITES_PER_PROFILE`/`DataKind::ALL` の
/// 変更に追随させる。
fn last_write_index_for_kind(kind_index: usize, kind_count: usize) -> usize {
    let last_full_cycle_start = (WRITES_PER_PROFILE / kind_count) * kind_count;
    if last_full_cycle_start + kind_index < WRITES_PER_PROFILE {
        last_full_cycle_start + kind_index
    } else {
        last_full_cycle_start + kind_index - kind_count
    }
}

/// `PROF-3`・TASK-52（52.1）・#184: 最低並行数（5）を超える設定でも
/// ハーネスが動作すること（「5 以上」を満たすことの確認）。実行時間を
/// 抑えるため書き込み回数は小さい値にする。
#[test]
fn prof_3_harness_supports_more_than_minimum_concurrency() {
    let tmp = TempDir::new();
    let profiles = 8;
    let writes_per_profile = 20;
    let config = HarnessConfig {
        profiles,
        writes_per_profile,
    };

    let outcomes = run_concurrent_writes(tmp.path(), &config);

    assert_eq!(outcomes.len(), profiles, "outcome の件数が想定と違う");
    for (expected_index, outcome) in outcomes.into_iter().enumerate() {
        match outcome {
            WorkerOutcome::Completed(report) => {
                assert_eq!(report.index, expected_index);
                assert_eq!(report.writes_attempted, writes_per_profile);
                assert_eq!(report.writes_succeeded, writes_per_profile);
            }
            WorkerOutcome::OpenFailed { index, error } => {
                panic!("worker {index} open failed: {error}")
            }
            WorkerOutcome::Panicked { index, message } => {
                panic!("worker {index} panicked: {message}")
            }
        }
    }
}

/// `PROF-3`・TASK-52（52.1）・#184: `MIN_CONCURRENCY`（5）を下回る並行数は
/// ハーネスの前提条件違反として拒否する。テストヘルパーの誤用（テスト
/// コード自身の不変条件違反であり外部入力ではない）とみなし、`Result` で
/// 呼び出し元に判断を委ねるより安全側（より厳密な方）である `panic!` で
/// 即座に打ち切る設計を選んだ。
#[test]
#[should_panic(expected = "PROF-3 は並行プロファイル数を 5 以上要求する")]
fn prof_3_harness_rejects_concurrency_below_minimum() {
    let tmp = TempDir::new();
    let config = HarnessConfig {
        profiles: 4,
        writes_per_profile: 1,
    };

    // 並行数の検証は書き込みより前に行われるため、実際にワーカーを走らせる
    // 前に panic する。
    let _ = run_concurrent_writes(tmp.path(), &config);
}

/// `PROF-3`・TASK-52（52.1）・#184: `Profile::open` に失敗したワーカーが
/// `barrier.wait()` を経て `OpenFailed` を返し、他のワーカーを deadlock
/// させずに `Completed` として完走させられること。§T2 のドキュメント
/// コメントが「deadlock の最大の危険箇所」と明記する経路（open 失敗後の
/// `barrier.wait()`）を実際に踏む。`profile-0` の root パスをあらかじめ
/// 通常ファイルとして作っておくことで、`Profile::open` を
/// `Err(ProfileError::InvalidLayout)` にする
/// （`src/profile.rs` の `prof_1_open_fails_when_root_is_a_file` と同じ手法）。
#[test]
fn prof_3_harness_open_failure_does_not_deadlock_other_workers() {
    let tmp = TempDir::new();
    std::fs::create_dir_all(tmp.path()).expect("ベースディレクトリの作成");
    let failing_root = tmp.path().join("profile-0");
    std::fs::write(&failing_root, b"not a directory").expect("root 用ファイルの作成");

    let config = HarnessConfig {
        profiles: PROFILE_COUNT,
        writes_per_profile: WRITES_PER_PROFILE,
    };

    // このテストが（タイムアウトせず）完走すること自体が、open 失敗した
    // ワーカーが他のワーカーを deadlock させないことの証明になる。
    let outcomes = run_concurrent_writes(tmp.path(), &config);

    assert_eq!(outcomes.len(), PROFILE_COUNT, "outcome の件数が想定と違う");

    for (expected_index, outcome) in outcomes.into_iter().enumerate() {
        if expected_index == 0 {
            match outcome {
                WorkerOutcome::OpenFailed { index, error } => {
                    assert_eq!(index, 0, "open 失敗ワーカーの index が想定と違う");
                    assert!(!error.is_empty(), "エラーメッセージが空");
                }
                WorkerOutcome::Completed(report) => {
                    panic!(
                        "worker 0 は open 失敗するはずが Completed で返った（index={}）",
                        report.index
                    )
                }
                WorkerOutcome::Panicked { index, message } => {
                    panic!("worker {index} が想定外に panic した: {message}")
                }
            }
        } else {
            match outcome {
                WorkerOutcome::Completed(report) => {
                    assert_eq!(report.index, expected_index);
                    assert_eq!(report.writes_attempted, WRITES_PER_PROFILE);
                    assert_eq!(
                        report.writes_succeeded, WRITES_PER_PROFILE,
                        "worker {expected_index}: writes_succeeded が想定と違う"
                    );
                    assert!(
                        report.write_errors.is_empty(),
                        "worker {expected_index}: write_errors が空でない: {:?}",
                        report.write_errors
                    );
                }
                WorkerOutcome::OpenFailed { index, error } => {
                    panic!("worker {index} open failed: {error}")
                }
                WorkerOutcome::Panicked { index, message } => {
                    panic!("worker {index} panicked: {message}")
                }
            }
        }
    }
}
