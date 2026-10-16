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
//! `is_file` / `is_dir` だけで行う。標準パス以外の Visual Studio は、環境変数 `VSINSTALLDIR` と
//! Visual Studio Installer の状態ファイル（`%ProgramData%\Microsoft\VisualStudio\Packages\_Instances\*\state.json`
//! の `installationPath`）を読み取って探索対象に加える。SDK・MSVC は版名のディレクトリだけでなく
//! 必須ヘッダー・ライブラリ・`cl.exe` の実体まで確認する。そのため既知の限界がある。Microsoft Store の App Execution Alias
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
/// 状態ファイル 1 件で読む最大バイト数。
#[cfg_attr(not(any(test, target_os = "windows")), allow(dead_code))]
const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;

/// ホストアーキテクチャの SDK / MSVC ディレクトリ名（`x64`・`arm64`・`x86`）。
fn host_arch_dir() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86" => "x86",
        _ => "x64",
    }
}

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
        let mut vs_installs = std::env::var_os("VSINSTALLDIR")
            .map(|v| vec![PathBuf::from(v)])
            .unwrap_or_default();
        if let Some(pd) = std::env::var_os("ProgramData") {
            vs_installs.extend(installer_state_install_paths(&PathBuf::from(pd)));
        }
        windows_report(pf86.as_deref(), pf.as_deref(), &vs_installs, &path_dirs)
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
    vs_installs: &[PathBuf],
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
    let mut msvc_roots = msvc_roots_under(&vs_roots);
    for install in vs_installs {
        let r = install.join("VC").join("Tools").join("MSVC");
        if r.is_dir() && !msvc_roots.contains(&r) {
            msvc_roots.push(r);
        }
    }
    let (msvc, v143_dirs) = detect_msvc_v143(&msvc_roots);
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

/// Visual Studio Installer の状態ファイル群から `installationPath` を集める（読み取り専用）。
///
/// `<ProgramData>\Microsoft\VisualStudio\Packages\_Instances\<id>\state.json` を対象にする。
/// 標準パス以外（例: `D:\VS`）へ導入した環境を検出するため。JSON パーサー依存を避け、
/// キー直後の文字列リテラルだけを取り出す（失敗したら何も返さない）。
#[cfg_attr(not(any(test, target_os = "windows")), allow(dead_code))]
pub(crate) fn installer_state_install_paths(program_data: &Path) -> Vec<PathBuf> {
    let instances = program_data
        .join("Microsoft")
        .join("VisualStudio")
        .join("Packages")
        .join("_Instances");
    let mut out = Vec::new();
    for (_, dir) in list_names(&instances) {
        let state = dir.join("state.json");
        let Ok(meta) = std::fs::metadata(&state) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_STATE_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&state) else {
            continue;
        };
        if let Some(p) = extract_json_string(&text, "installationPath") {
            let p = PathBuf::from(p);
            if p.is_absolute() && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// `"key" : "value"` の最初の値を取り出す（`\\`・`\"`・`\/` のみ解除。他のエスケープは失敗扱い）。
#[cfg_attr(not(any(test, target_os = "windows")), allow(dead_code))]
fn extract_json_string(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let rest = text[text.find(&needle)? + needle.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let mut val = String::new();
    let mut chars = rest.chars();
    loop {
        match chars.next()? {
            '"' => return Some(val),
            '\\' => match chars.next()? {
                c @ ('\\' | '"' | '/') => val.push(c),
                _ => return None,
            },
            c => val.push(c),
        }
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
///
/// 版名のディレクトリであることに加え、`Include\<版>\um\Windows.h` と
/// `Lib\<版>\um\<arch>\kernel32.lib`（`Lib` は `Include` の兄弟）の実体を確認する。
pub(crate) fn detect_windows_sdk(include_root: &Path) -> PrerequisiteStatus {
    let lib_root = include_root.parent().map(|p| p.join("Lib"));
    let best = list_names(include_root)
        .into_iter()
        .filter(|(_, path)| path.is_dir())
        .filter(|(n, path)| {
            path.join("um").join("Windows.h").is_file()
                && lib_root.as_ref().is_some_and(|l| {
                    l.join(n)
                        .join("um")
                        .join(host_arch_dir())
                        .join("kernel32.lib")
                        .is_file()
                })
        })
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
                // 空ディレクトリ・ATL だけが残った toolset を弾くため、コンパイラの実体まで確認する。
                if mm >= MSVC_V143_MIN
                    && mm < MSVC_V143_MAX_EXCL
                    && path.is_dir()
                    && msvc_cl_exe(&path).is_file()
                {
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

/// toolset 内のホスト向け `cl.exe` のパス（`bin\Host<arch>\<arch>\cl.exe`）。
fn msvc_cl_exe(toolset: &Path) -> PathBuf {
    let a = host_arch_dir();
    toolset
        .join("bin")
        .join(format!("Host{a}"))
        .join(a)
        .join("cl.exe")
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

    /// `<root>/Include` と `<root>/Lib` に実体付きの SDK 版を作り、`Include` を返す。
    fn make_sdk(root: &Path, ver: &str) -> PathBuf {
        touch(&root.join("Include").join(ver).join("um").join("Windows.h"));
        touch(
            &root
                .join("Lib")
                .join(ver)
                .join("um")
                .join(host_arch_dir())
                .join("kernel32.lib"),
        );
        root.join("Include")
    }
    fn make_toolset(msvc_root: &Path, ver: &str) -> PathBuf {
        let d = msvc_root.join(ver);
        touch(&msvc_cl_exe(&d));
        d
    }

    #[test]
    fn cli2_sdk_picks_highest_version_at_or_above_minimum() {
        let t = TempDir::new();
        let inc = make_sdk(&t.owned, "10.0.22621.0");
        make_sdk(&t.owned, "10.0.17763.0");
        mkdir(&inc.join("wdf"));
        assert_eq!(
            detect_windows_sdk(&inc),
            PrerequisiteStatus::Satisfied {
                found: "10.0.22621.0".into()
            }
        );
    }

    #[test]
    fn cli2_sdk_missing_when_only_old_or_no_root() {
        let t = TempDir::new();
        let inc = make_sdk(&t.owned, "10.0.17763.0");
        assert_eq!(detect_windows_sdk(&inc), sdk_missing());
        assert_eq!(detect_windows_sdk(&t.owned.join("nope")), sdk_missing());
    }

    #[test]
    fn cli2_sdk_file_empty_dir_or_missing_lib_is_missing() {
        let t = TempDir::new();
        let inc = t.owned.join("Include");
        touch(&inc.join("10.0.22621.0")); // 通常ファイル
        mkdir(&inc.join("10.0.26100.0")); // 空ディレクトリ
        touch(&inc.join("10.0.19041.0/um/Windows.h")); // Lib 側が無い
        assert_eq!(detect_windows_sdk(&inc), sdk_missing());
    }

    #[test]
    fn cli2_msvc_file_empty_dir_or_atl_only_is_missing() {
        let t = TempDir::new();
        touch(&t.owned.join("14.44.35207")); // 通常ファイル
        mkdir(&t.owned.join("14.43.1")); // 空ディレクトリ
        touch(&t.owned.join("14.42.1/atlmfc/include/atlbase.h")); // ATL のみ
        let (st, dirs) = detect_msvc_v143(std::slice::from_ref(&t.owned));
        assert_eq!(
            st,
            PrerequisiteStatus::Missing {
                expected: "MSVC v143 toolset (14.30 to 14.49)"
            }
        );
        assert!(dirs.is_empty());
    }

    #[test]
    fn cli2_installer_state_parsing() {
        assert_eq!(
            extract_json_string(r#"{"installationPath" : "D:\\VS"}"#, "installationPath"),
            Some("D:\\VS".to_string())
        );
        assert_eq!(
            extract_json_string(r#"{"x":"y"}"#, "installationPath"),
            None
        );
        assert_eq!(
            extract_json_string(r#"{"installationPath":"a\u0041"}"#, "installationPath"),
            None
        );
        let t = TempDir::new();
        let inst = t
            .owned
            .join("Microsoft/VisualStudio/Packages/_Instances/abc123");
        mkdir(&inst);
        std::fs::write(inst.join("state.json"), br#"{"installationPath":"rel/x"}"#).expect("write");
        assert!(installer_state_install_paths(&t.owned).is_empty());
    }

    #[test]
    fn cli2_custom_vs_install_dir_is_searched() {
        let t = TempDir::new();
        make_sdk(&t.owned.join("pf86/Windows Kits/10"), "10.0.26100.0");
        let vs = t.owned.join("D/VS");
        let d = make_toolset(&vs.join("VC/Tools/MSVC"), "14.44.35207");
        touch(&d.join("atlmfc/include/atlbase.h"));
        let r = windows_report(Some(&t.owned.join("pf86")), None, &[vs], &[]);
        assert!(matches!(
            r.checks[1].status,
            PrerequisiteStatus::Satisfied { .. }
        ));
        assert!(matches!(
            r.checks[2].status,
            PrerequisiteStatus::Satisfied { .. }
        ));
    }

    #[test]
    fn cli2_msvc_outside_v143_range_is_missing_and_atl_not_checked() {
        let t = TempDir::new();
        make_toolset(&t.owned, "14.29.30133");
        make_toolset(&t.owned, "14.50.1");
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
        make_toolset(&t.owned, "14.44.35207");
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
        make_toolset(&t.owned, "14.44.35207");
        make_toolset(&t.owned, "14.38.33130");
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
        let r = windows_report(None, None, &[], &[]);
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
        make_sdk(&pf86.join("Windows Kits/10"), "10.0.26100.0");
        let msvc = pf.join("Microsoft Visual Studio/18/Community/VC/Tools/MSVC");
        let d = make_toolset(&msvc, "14.44.35207");
        touch(&d.join("atlmfc/include/atlbase.h"));
        let bin = t.owned.join("bin");
        touch(&bin.join("python.exe"));
        touch(&bin.join("uv.exe"));
        let r = windows_report(Some(&pf86), Some(&pf), &[], &[bin]);
        assert_eq!(r.missing().count(), 0);
        assert!(
            r.checks
                .iter()
                .all(|c| matches!(c.status, PrerequisiteStatus::Satisfied { .. }))
        );
    }
}
