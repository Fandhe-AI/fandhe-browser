//! OS 固有ビルド前提条件の検出（TASK-48.1・Issue #290・`CLI-2`・`XOS-6`・MS-5）。
//!
//! 役割: Windows で Servo ホスト（`servo-host/`。別 workspace）をソースからビルドするために
//! 必要な前提（Windows SDK・MSVC v143・v143 向け C++ ATL・Python・`uv`）が揃っているかを、
//! 読み取り専用で検出して [`SetupReport`] にまとめる。CLI バイナリ自体は Servo をコンパイルしない
//! ため、これらは日常操作コマンド（launch・navigate・query・screenshot・`--profile-dir`）の実行には
//! 不要で、`CLI-2` に従いセットアップ専用の `setup` コマンド（`commands::setup`）からだけ呼ぶ。
//! 日常操作コマンドの経路からは呼ばない（構造テスト `cli2_daily_command_sources_*` で固定）。
//!
//! 検出は外部プロセスを起動せず（`vswhere`・`python --version` も使わない）、ディレクトリ列挙と
//! `is_file` だけで行う。そのため既知の限界がある。Microsoft Store の App Execution Alias
//! （`WindowsApps` 配下のスタブ）を実体と区別できず、Python・`uv` の版の下限も検査しない。
//!
//! 閾値の同期点: `.github/workflows/ci.yml` の `windows-servo-prereqs` ジョブと
//! `docs/setup/windows.md`。
//!
//! # スタブについて
//!
//! Windows 以外の OS では検査項目が空の [`SetupReport`] を返す（合格を装わず「検査していない」ことを
//! 出力側で示す。REPAIR-3）。Linux・macOS の Servo ホストのビルド前提の検査は将来仕様
//! （`CLI-2`・`XOS-6`）。`cfg(target_os)` による分岐は [`detect_current`] だけに局所化する。

use std::fmt;
use std::path::{Path, PathBuf};

/// 1 ディレクトリで読むエントリ数の上限（巨大ディレクトリによる DoS 防止）。
const MAX_DIR_ENTRIES: usize = 4096;
/// 走査する PATH 要素数の上限。
const MAX_PATH_ENTRIES: usize = 1024;
/// Windows SDK の下限（同期点: ci.yml `MIN_WINDOWS_SDK`）。
const MIN_WINDOWS_SDK: [u32; 4] = [10, 0, 19041, 0];
/// MSVC v143 とみなす toolset の範囲 `[14.30, 14.50)`（同期点: ci.yml）。
const MSVC_V143_MIN: (u32, u32) = (14, 30);
const MSVC_V143_MAX_EXCL: (u32, u32) = (14, 50);

/// 前提条件の識別子。表示名は固定の英語 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum PrerequisiteId {
    WindowsSdk,
    MsvcV143,
    AtlV143,
    Python,
    Uv,
}

impl fmt::Display for PrerequisiteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WindowsSdk => "windows-sdk",
            Self::MsvcV143 => "msvc-v143",
            Self::AtlV143 => "atl-v143",
            Self::Python => "python",
            Self::Uv => "uv",
        })
    }
}

/// 前提 1 件の判定結果。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum PrerequisiteStatus {
    /// 満たされている（見つかった版。数値成分から組み立てた文字列か固定文言）。
    Satisfied { found: String },
    /// 不足している（期待する条件）。
    Missing { expected: &'static str },
    /// 判定できなかった（理由。例: 依存する前提が無い）。
    NotChecked { reason: &'static str },
}

/// 前提 1 件の検査結果と案内。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrerequisiteCheck {
    pub(crate) id: PrerequisiteId,
    pub(crate) status: PrerequisiteStatus,
    /// 導入方法の案内（英語の固定文言）。
    pub(crate) hint: &'static str,
}

/// 検出結果一式。`checks` が空なら、その OS では何も検査していない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetupReport {
    pub(crate) os: &'static str,
    pub(crate) checks: Vec<PrerequisiteCheck>,
}

impl SetupReport {
    /// 不足（`Missing`）している前提の ID を順に返す。
    pub(crate) fn missing(&self) -> impl Iterator<Item = PrerequisiteId> + '_ {
        self.checks
            .iter()
            .filter(|c| matches!(c.status, PrerequisiteStatus::Missing { .. }))
            .map(|c| c.id)
    }
}

/// 実行中の OS に対する検出。`commands::setup` からだけ呼ばれる。
pub(crate) fn detect_current() -> SetupReport {
    #[cfg(target_os = "windows")]
    {
        let pf86 = std::env::var_os("ProgramFiles(x86)").map(PathBuf::from);
        let pf = std::env::var_os("ProgramFiles").map(PathBuf::from);
        let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).take(MAX_PATH_ENTRIES).collect())
            .unwrap_or_default();
        windows_report(pf86.as_deref(), pf.as_deref(), &path_dirs)
    }
    #[cfg(not(target_os = "windows"))]
    {
        SetupReport {
            os: std::env::consts::OS,
            checks: Vec::new(),
        }
    }
}

/// Windows 向けの検出を合成する（全 OS でコンパイルし、偽ディレクトリ木でテストする）。
// 非 Windows のバイナリでは `detect_current` から呼ばれずテストだけが使う。
#[cfg_attr(not(any(test, target_os = "windows")), allow(dead_code))]
pub(crate) fn windows_report(
    program_files_x86: Option<&Path>,
    program_files: Option<&Path>,
    path_dirs: &[PathBuf],
) -> SetupReport {
    let sdk = match program_files_x86 {
        Some(p) => detect_windows_sdk(&p.join("Windows Kits").join("10").join("Include")),
        None => sdk_missing(),
    };
    let vs_roots: Vec<PathBuf> = [program_files, program_files_x86]
        .into_iter()
        .flatten()
        .map(|p| p.join("Microsoft Visual Studio"))
        .collect();
    let (msvc, v143_dirs) = detect_msvc_v143(&msvc_roots_under(&vs_roots));
    let atl = detect_atl_v143(&v143_dirs);
    let python = find_executable(path_dirs, &["python.exe", "python3.exe", "py.exe"]);
    let uv = find_executable(path_dirs, &["uv.exe"]);
    let mk = |id, status, hint| PrerequisiteCheck { id, status, hint };
    SetupReport {
        os: "windows",
        checks: vec![
            mk(
                PrerequisiteId::WindowsSdk,
                sdk,
                "install the Windows 10/11 SDK via Visual Studio Installer; see docs/setup/windows.md",
            ),
            mk(
                PrerequisiteId::MsvcV143,
                msvc,
                "install 'MSVC v143 C++ build tools' via Visual Studio Installer; see docs/setup/windows.md",
            ),
            mk(
                PrerequisiteId::AtlV143,
                atl,
                "install 'C++ ATL for latest v143 build tools' via Visual Studio Installer; see docs/setup/windows.md",
            ),
            mk(
                PrerequisiteId::Python,
                python,
                "install Python 3 and add it to PATH; see docs/setup/windows.md",
            ),
            mk(
                PrerequisiteId::Uv,
                uv,
                "install uv and add it to PATH; see docs/setup/windows.md",
            ),
        ],
    }
}

fn sdk_missing() -> PrerequisiteStatus {
    PrerequisiteStatus::Missing {
        expected: "Windows SDK 10.0.19041.0 or newer",
    }
}

/// 直下エントリ（UTF-8 名のもの）を上限付きで集める。読めなければ空。
fn list_names(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.take(MAX_DIR_ENTRIES)
        .filter_map(|e| e.ok())
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .collect()
}

/// `a.b.c...` を `N` 個の数値成分に解釈する。成分数違い・非数値は `None`。
fn parse_version<const N: usize>(s: &str) -> Option<[u32; N]> {
    let mut out = [0u32; N];
    let mut parts = s.split('.');
    for slot in &mut out {
        *slot = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

/// Windows SDK を `Include` 直下の版ディレクトリから検出する。
pub(crate) fn detect_windows_sdk(include_root: &Path) -> PrerequisiteStatus {
    let best = list_names(include_root)
        .into_iter()
        .filter_map(|(n, _)| parse_version::<4>(&n))
        .filter(|v| *v >= MIN_WINDOWS_SDK)
        .max();
    match best {
        Some([a, b, c, d]) => PrerequisiteStatus::Satisfied {
            found: format!("{a}.{b}.{c}.{d}"),
        },
        None => sdk_missing(),
    }
}

/// `VC\Tools\MSVC` 群から v143 toolset を検出し、見つかった toolset ディレクトリ（新しい順）も返す。
pub(crate) fn detect_msvc_v143(msvc_roots: &[PathBuf]) -> (PrerequisiteStatus, Vec<PathBuf>) {
    let mut found: Vec<([u32; 3], PathBuf)> = Vec::new();
    for root in msvc_roots {
        for (name, path) in list_names(root) {
            if let Some(v) = parse_version::<3>(&name) {
                let mm = (v[0], v[1]);
                if mm >= MSVC_V143_MIN && mm < MSVC_V143_MAX_EXCL {
                    found.push((v, path));
                }
            }
        }
    }
    found.sort_by_key(|a| std::cmp::Reverse(a.0));
    let status = match found.first() {
        Some(([a, b, c], _)) => PrerequisiteStatus::Satisfied {
            found: format!("{a}.{b}.{c}"),
        },
        None => PrerequisiteStatus::Missing {
            expected: "MSVC v143 toolset (14.30 to 14.49)",
        },
    };
    (status, found.into_iter().map(|(_, p)| p).collect())
}

/// v143 toolset のうち自身の `atlmfc\include\atlbase.h` を持つものがあるかを判定する。
pub(crate) fn detect_atl_v143(v143_dirs: &[PathBuf]) -> PrerequisiteStatus {
    if v143_dirs.is_empty() {
        return PrerequisiteStatus::NotChecked {
            reason: "MSVC v143 toolset not found",
        };
    }
    // `v143_dirs` は新しい版が先頭。最初に ATL を持つものを採用する。
    for dir in v143_dirs {
        if dir
            .join("atlmfc")
            .join("include")
            .join("atlbase.h")
            .is_file()
        {
            let ver = dir
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(parse_version::<3>)
                .map(|[a, b, c]| format!("{a}.{b}.{c}"))
                .unwrap_or_else(|| "unknown".to_string());
            return PrerequisiteStatus::Satisfied { found: ver };
        }
    }
    PrerequisiteStatus::Missing {
        expected: "C++ ATL for v143 (atlmfc/include/atlbase.h)",
    }
}

/// Visual Studio ルート群の下の `<版>\<エディション>\VC\Tools\MSVC`（固定深さ 2）を列挙する。
pub(crate) fn msvc_roots_under(vs_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in vs_roots {
        for (_, ver_dir) in list_names(root) {
            for (_, edition_dir) in list_names(&ver_dir) {
                let msvc = edition_dir.join("VC").join("Tools").join("MSVC");
                if msvc.is_dir() {
                    out.push(msvc);
                }
            }
        }
    }
    out
}

/// PATH の各ディレクトリに指定名の通常ファイルがあるかを見る（実行はしない）。
pub(crate) fn find_executable(dirs: &[PathBuf], names: &[&str]) -> PrerequisiteStatus {
    for dir in dirs.iter().take(MAX_PATH_ENTRIES) {
        for name in names {
            if dir.join(name).is_file() {
                return PrerequisiteStatus::Satisfied {
                    found: (*name).to_string(),
                };
            }
        }
    }
    PrerequisiteStatus::Missing {
        expected: "executable found on PATH",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tests::support::TempDir;

    fn mkdir(p: &Path) {
        std::fs::create_dir_all(p).expect("mkdir");
    }
    fn touch(p: &Path) {
        mkdir(p.parent().expect("parent"));
        std::fs::write(p, b"").expect("touch");
    }

    #[test]
    fn cli2_sdk_picks_highest_version_at_or_above_minimum() {
        let t = TempDir::new();
        for v in ["10.0.22621.0", "10.0.17763.0", "wdf"] {
            mkdir(&t.owned.join(v));
        }
        assert_eq!(
            detect_windows_sdk(&t.owned),
            PrerequisiteStatus::Satisfied {
                found: "10.0.22621.0".into()
            }
        );
    }

    #[test]
    fn cli2_sdk_missing_when_only_old_or_no_root() {
        let t = TempDir::new();
        mkdir(&t.owned.join("10.0.17763.0"));
        assert_eq!(detect_windows_sdk(&t.owned), sdk_missing());
        assert_eq!(detect_windows_sdk(&t.owned.join("nope")), sdk_missing());
    }

    #[test]
    fn cli2_msvc_outside_v143_range_is_missing_and_atl_not_checked() {
        let t = TempDir::new();
        mkdir(&t.owned.join("14.29.30133"));
        mkdir(&t.owned.join("14.50.1"));
        let (st, dirs) = detect_msvc_v143(std::slice::from_ref(&t.owned));
        assert_eq!(
            st,
            PrerequisiteStatus::Missing {
                expected: "MSVC v143 toolset (14.30 to 14.49)"
            }
        );
        assert!(matches!(
            detect_atl_v143(&dirs),
            PrerequisiteStatus::NotChecked { .. }
        ));
    }

    #[test]
    fn cli2_atl_missing_when_header_absent() {
        let t = TempDir::new();
        mkdir(&t.owned.join("14.44.35207"));
        let (st, dirs) = detect_msvc_v143(std::slice::from_ref(&t.owned));
        assert_eq!(
            st,
            PrerequisiteStatus::Satisfied {
                found: "14.44.35207".into()
            }
        );
        assert!(matches!(
            detect_atl_v143(&dirs),
            PrerequisiteStatus::Missing { .. }
        ));
    }

    #[test]
    fn cli2_atl_found_in_older_v143_toolset() {
        let t = TempDir::new();
        mkdir(&t.owned.join("14.44.35207"));
        touch(&t.owned.join("14.38.33130/atlmfc/include/atlbase.h"));
        let (_, dirs) = detect_msvc_v143(std::slice::from_ref(&t.owned));
        assert_eq!(
            detect_atl_v143(&dirs),
            PrerequisiteStatus::Satisfied {
                found: "14.38.33130".into()
            }
        );
    }

    #[test]
    fn cli2_msvc_roots_scan_version_and_edition_levels() {
        let t = TempDir::new();
        let vs = t.owned.join("Microsoft Visual Studio");
        mkdir(&vs.join("2022/BuildTools/VC/Tools/MSVC"));
        mkdir(&vs.join("18/Community/VC/Tools/MSVC"));
        mkdir(&vs.join("Installer"));
        let mut roots = msvc_roots_under(std::slice::from_ref(&vs));
        roots.sort();
        let mut expected = vec![
            vs.join("18/Community/VC/Tools/MSVC"),
            vs.join("2022/BuildTools/VC/Tools/MSVC"),
        ];
        expected.sort();
        assert_eq!(roots, expected);
    }

    #[test]
    fn cli2_find_executable_requires_regular_file() {
        let t = TempDir::new();
        let a = t.owned.join("a");
        let b = t.owned.join("b");
        touch(&a.join("python.exe"));
        mkdir(&b.join("uv.exe"));
        let dirs = vec![a, b];
        assert_eq!(
            find_executable(&dirs, &["python.exe"]),
            PrerequisiteStatus::Satisfied {
                found: "python.exe".into()
            }
        );
        assert!(matches!(
            find_executable(&dirs, &["uv.exe"]),
            PrerequisiteStatus::Missing { .. }
        ));
    }

    #[test]
    fn cli2_windows_report_without_inputs_is_unsatisfied_in_fixed_order() {
        let r = windows_report(None, None, &[]);
        let ids: Vec<String> = r.checks.iter().map(|c| c.id.to_string()).collect();
        assert_eq!(
            ids,
            ["windows-sdk", "msvc-v143", "atl-v143", "python", "uv"]
        );
        let missing: Vec<String> = r.missing().map(|i| i.to_string()).collect();
        assert_eq!(missing, ["windows-sdk", "msvc-v143", "python", "uv"]);
        assert!(matches!(
            r.checks[2].status,
            PrerequisiteStatus::NotChecked { .. }
        ));
    }

    #[test]
    fn cli2_windows_report_all_satisfied() {
        let t = TempDir::new();
        let pf86 = t.owned.join("pf86");
        let pf = t.owned.join("pf");
        mkdir(&pf86.join("Windows Kits/10/Include/10.0.26100.0"));
        touch(&pf.join(
            "Microsoft Visual Studio/18/Community/VC/Tools/MSVC/14.44.35207/atlmfc/include/atlbase.h",
        ));
        let bin = t.owned.join("bin");
        touch(&bin.join("python.exe"));
        touch(&bin.join("uv.exe"));
        let r = windows_report(Some(&pf86), Some(&pf), &[bin]);
        assert_eq!(r.missing().count(), 0);
        assert!(
            r.checks
                .iter()
                .all(|c| matches!(c.status, PrerequisiteStatus::Satisfied { .. }))
        );
    }
}
