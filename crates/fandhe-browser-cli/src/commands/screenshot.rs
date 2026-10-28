//! スクリーンショット取得コマンド `screenshot`（TASK-47.5・Issue #286・`CLI-1`・MS-5）。
//!
//! `screenshot [--timeout <SECONDS>] [--] <URL> <OUTPUT>` はプロセス内で描画ホスト（`render_host` が解決する
//! `HostRenderer`）へ URL を 1 回読み込ませ、PNG を `OUTPUT` に書いて 1 行を stdout へ出し終了する
//! （one-shot）。実行中の `launch` インスタンスや直前の `navigate` で開いたページは参照できない
//! （navigate.rs と同じ理由。ai に `/ai/navigate` が無く、CDP クライアントには新規依存が要るため）。
//! 取得は描画ホストが自分で行うため、`navigate::fetch_page` は使わず二重取得しない。
//!
//! 処理順は fail-fast で副作用を最後に置く: 引数解析 → 出力先パス検証（ファイルは作らない）→
//! URL の事前検証（SSRF 含む）→ レンダラー解決 → load → capture → 排他的書き込み。
//! 出力先の誤りは描画ホストの有無によらず決定的に失敗させるため、ホスト起動より前に検証する。
//! 上書きは拒否する（`create_new` = O_EXCL / CREATE_NEW）。検証と書き込みの間の競合や
//! シンボリックリンク経由の既存ファイル破壊を、Linux・macOS・Windows で同じく防ぐ安全側の判断。
//! `--timeout <SECONDS>`（#1301）はページ読み込み（`load`）の上限を 1〜60 秒の 10 進整数で指定する。
//! 未指定はホスト既定（30 秒）。効くのは `load` だけで、スクリーンショットの段階の上限（20 秒）・
//! ホスト起動の `hello` 待ち（10 秒）は変えない。上限を超える値は丸めず usage エラーで拒否する。
//! `--timeout` が上限を延ばすのは遅いページ（react.dev 等）が既定の 30 秒を超えるときのためで、
//! 上限の最大は render プロトコルの `MAX_LOAD_TIMEOUT_MS`（60 秒。`RENDER-4`・設計 §2.4）に揃える。
//! 出力は常に PNG で、拡張子は強制しない。`cfg(target_os)` 分岐は置かず 3 OS で同一挙動（`CLI-1`）。
//!
//! # スタブについて
//!
//! 以下は未実装・暫定で、実装済みを装わない（REPAIR-3）。
//!
//! - 出力形式 `width=<W> height=<H> bytes=<N> url=<final_url>` は暫定。`CLI-3` の確定と合わせて見直す
//! - `--profile-dir`（TASK-47.6・#287）はプロファイルを開いて `profile.lock` を保持するだけ。Cookie 連携は TASK-116・`PROF-7`
//! - `--full-page`: v1 の描画ホストが未対応（固定ビューポート 1280x800。`servo-host-design.md` §2.3・
//!   `SEC-2`）のため、解析時に usage エラーで拒否する。出力は常にビューポートの画像。ホストが対応したら
//!   `ScreenshotOptions::with_full_page` を配線する（`RENDER-1`・`REPAIR-3`）
//! - `--timeout` は `load` のみ。スクリーンショット段階・ホスト起動の上限は固定のまま（設定化は未定）。
//!   `--timeout=<N>` 形式は未対応（`--profile-dir`・`--port` と同じく別引数の値だけ受け付ける）
//! - 上書きオプション・stdout への出力（`-`）・クリップやスケールの指定は未対応
//! - `screenshot --help`: ハンドラは出力先を受け取らないため未対応（トップレベルの `--help` には載る）
//! - DNS 名がプライベート IP へ解決される場合とリバインディングは `HostRenderer` の既知の制約
//!   （設計 `servo-host-design.md` §2.5・§13 のオーナー判断待ち）

use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{MAIN_SEPARATOR, Path, PathBuf};

use fandhe_browser_core::check_network_target_url;
use fandhe_browser_core::render::{
    ImageFormat, PageRef, RenderError, Renderer, Screenshot, ScreenshotOptions,
};
use fandhe_browser_render::protocol::{MAX_LOAD_TIMEOUT_MS, MAX_URL_BYTES};

use super::navigate::NavigateError;
use super::profile::{self, ProfileSelection};
use super::{CommandArgs, CommandError, CommandOutcome, CommandSpec, UsageError};
use crate::render_host;

/// 登録表（[`super::COMMANDS`]）へ載せる仕様。
pub(crate) const SPEC: CommandSpec = CommandSpec {
    name: "screenshot",
    summary: "Render a URL once and save a PNG screenshot to a file",
    run,
};

/// `screenshot` の解析済みオプション。#287 のプロファイル指定の拡張点。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) struct ScreenshotCommandOptions {
    /// 描画対象 URL。
    pub(crate) url: String,
    /// 出力先ファイルパス（PNG）。
    pub(crate) output: PathBuf,
    /// プロファイルの選択（`--profile-dir`）。
    pub(crate) profile: ProfileSelection,
    /// ページ読み込み（`load`）の上限（ミリ秒。`--timeout` の秒数を変換済み）。
    /// `None` はホスト既定（30 秒）。スクリーンショット段階の上限には効かない。
    pub(crate) load_timeout_ms: Option<u32>,
}

/// `--timeout` のオプション名。
const TIMEOUT_OPTION: &str = "--timeout";
/// `--timeout` の値の名前（欠落エラーの表示用）。
const TIMEOUT_VALUE_NAME: &str = "SECONDS";
/// `--timeout` の最大秒数。render プロトコルの上限（`MAX_LOAD_TIMEOUT_MS`）から導く。
const MAX_TIMEOUT_SECS: u32 = MAX_LOAD_TIMEOUT_MS / 1000;

/// `--timeout` の値（秒）を `load` の上限（ミリ秒）へ変換する（副作用なし・外部入力）。
///
/// ASCII 数字だけの 10 進整数で 1〜[`MAX_TIMEOUT_SECS`] のものだけ受け付ける。`+`・`-`・小数・空白・
/// 非 ASCII 数字・空文字は拒否し、`0` 以外の先頭ゼロも `--port` と揃えて拒否する。範囲外は丸めず拒否する。
fn parse_timeout_secs(value: &str) -> Result<u32, UsageError> {
    let invalid = UsageError::InvalidOptionValue {
        option: TIMEOUT_OPTION,
    };
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(invalid);
    }
    if bytes.len() > 1 && bytes.first() == Some(&b'0') {
        return Err(invalid);
    }
    match value.parse::<u32>() {
        Ok(secs) if (1..=MAX_TIMEOUT_SECS).contains(&secs) => Ok(secs.saturating_mul(1000)),
        _ => Err(invalid),
    }
}

/// `--timeout <SECONDS>` を 1 つ消費する。該当しなければ `Ok(false)`。重複指定・値の欠落は拒否する。
/// 書式の扱いは `--profile-dir`（[`profile::take_profile_dir`]）に揃える。
fn take_timeout<'a>(
    arg: &str,
    iter: &mut impl Iterator<Item = &'a String>,
    current: &mut Option<u32>,
) -> Result<bool, UsageError> {
    if arg != TIMEOUT_OPTION {
        return Ok(false);
    }
    if current.is_some() {
        return Err(UsageError::UnexpectedArgument(arg.to_string()));
    }
    let value = iter
        .next()
        .ok_or(UsageError::MissingArgument(TIMEOUT_VALUE_NAME))?;
    if value.starts_with('-') {
        // 後続のオプションを値として飲み込まない（`-5` などの符号つきも値の欠落として拒否する）。
        return Err(UsageError::MissingArgument(TIMEOUT_VALUE_NAME));
    }
    *current = Some(parse_timeout_secs(value)?);
    Ok(true)
}

/// `screenshot` の引数を解析する（副作用なし）。`--` より後ろはすべて位置引数として扱う。
pub(crate) fn parse_screenshot_args(
    args: &CommandArgs,
) -> Result<ScreenshotCommandOptions, UsageError> {
    let mut positionals: Vec<&String> = Vec::new();
    let mut only_positionals = false;
    let mut profile = ProfileSelection::OsDefault;
    let mut load_timeout_ms = None;
    let mut iter = args.as_slice().iter();
    while let Some(arg) = iter.next() {
        if only_positionals || !arg.starts_with('-') {
            positionals.push(arg);
            continue;
        }
        if profile::take_profile_dir(arg, &mut iter, &mut profile)? {
            continue;
        }
        if take_timeout(arg, &mut iter, &mut load_timeout_ms)? {
            continue;
        }
        match arg.as_str() {
            "--" => only_positionals = true,
            "--full-page" => return Err(UsageError::UnsupportedOption("--full-page")),
            _ => return Err(UsageError::UnknownOption(arg.clone())),
        }
    }
    let mut pos = positionals.into_iter();
    let url = pos.next().ok_or(UsageError::MissingArgument("URL"))?;
    let output = pos.next().ok_or(UsageError::MissingArgument("OUTPUT"))?;
    if let Some(extra) = pos.next() {
        return Err(UsageError::UnexpectedArgument(extra.clone()));
    }
    Ok(ScreenshotCommandOptions {
        url: url.clone(),
        output: PathBuf::from(output),
        profile,
        load_timeout_ms,
    })
}

/// 出力先パスの不備。パスは表示に含めない（端末出力の安全）。
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum OutputPathError {
    /// 空文字列。
    Empty,
    /// ファイル名を指していない（末尾が区切り文字・`..`・ルート等）。
    NotAFilePath,
    /// 親ディレクトリが存在しない。
    ParentNotFound,
    /// 親がディレクトリでない。
    ParentNotDirectory,
    /// 出力先がディレクトリとして存在する。
    IsDirectory,
    /// 出力先が既に存在する（ぶら下がりシンボリックリンクを含む）。
    AlreadyExists,
    /// 上記以外の I/O 失敗（権限等）。
    Io(io::Error),
}

impl fmt::Display for OutputPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "output path is empty",
            Self::NotAFilePath => "output path does not name a file",
            Self::ParentNotFound => "output directory does not exist",
            Self::ParentNotDirectory => "output directory is not a directory",
            Self::IsDirectory => "output path is a directory",
            Self::AlreadyExists => "output file already exists",
            Self::Io(_) => "cannot access output path",
        })
    }
}

impl std::error::Error for OutputPathError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// 出力先パスを検証する。ファイルは作らない（書き込み時の `create_new` が最終防御）。
pub(crate) fn validate_output_path(path: &Path) -> Result<(), OutputPathError> {
    let text = path.as_os_str().to_string_lossy();
    if text.is_empty() {
        return Err(OutputPathError::Empty);
    }
    // `Path::file_name` は末尾の区切り文字を無視するため、文字列で明示的に拒否する。
    // Windows の `\\?\` 付きパス（verbatim）では `..` が通常の名前として解釈され `file_name` が
    // `Some("..")` を返すため、末尾の生セグメントも文字列で明示的に検査して 3 OS で揃える。
    let last_segment = text.rsplit(['/', MAIN_SEPARATOR]).next().unwrap_or("");
    if path.file_name().is_none()
        || last_segment == ".."
        || text.ends_with('/')
        || text.ends_with(MAIN_SEPARATOR)
    {
        return Err(OutputPathError::NotAFilePath);
    }
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match std::fs::metadata(parent) {
        Ok(md) if md.is_dir() => {}
        Ok(_) => return Err(OutputPathError::ParentNotDirectory),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(OutputPathError::ParentNotFound);
        }
        Err(e) => return Err(OutputPathError::Io(e)),
    }
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.is_dir() => Err(OutputPathError::IsDirectory),
        Ok(_) => Err(OutputPathError::AlreadyExists),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(OutputPathError::Io(e)),
    }
}

/// `screenshot` の失敗。入力値・ホスト由来の詳細文字列は表示に含めない。
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum ScreenshotError {
    /// URL の長さ・形式・SSRF 検査の失敗（文言は navigate と揃える）。
    Navigate(NavigateError),
    /// `about:blank` は描画対象の文書が無い。
    NoDocument,
    /// URL に userinfo（`user:pass@`）が含まれる。render プロトコルが拒否する入力のため事前に弾く。
    UserinfoNotAllowed,
    /// 出力先パスの不備。
    OutputPath(OutputPathError),
    /// 描画の失敗。
    Render(RenderError),
    /// PNG 以外の形式が返った。
    UnsupportedFormat,
    /// ファイル書き込みの失敗。
    Write(io::Error),
}

impl fmt::Display for ScreenshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Navigate(e) => write!(f, "{e}"),
            Self::NoDocument => f.write_str(
                "screenshot requires a fetched document (about:blank has no content)",
            ),
            Self::UserinfoNotAllowed => f.write_str("URL must not contain userinfo"),
            Self::OutputPath(e) => write!(f, "{e}"),
            Self::Render(e) => f.write_str(match e {
                RenderError::RenderingDisabled => {
                    "rendering is not available (render host not found; set FANDHE_BROWSER_RENDER_HOST)"
                }
                RenderError::Timeout => "render timed out",
                RenderError::HostUnavailable => "render host is unavailable",
                RenderError::ProtocolMismatch => "render host protocol mismatch",
                RenderError::PageNotLoaded => "page could not be loaded for rendering",
                _ => "rendering failed",
            }),
            Self::UnsupportedFormat => f.write_str("unsupported screenshot image format"),
            Self::Write(_) => f.write_str("failed to write screenshot file"),
        }
    }
}

impl std::error::Error for ScreenshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Navigate(e) => Some(e),
            Self::OutputPath(e) => Some(e),
            Self::Render(e) => Some(e),
            Self::Write(e) => Some(e),
            Self::NoDocument | Self::UserinfoNotAllowed | Self::UnsupportedFormat => None,
        }
    }
}

/// 取得結果。将来の拡張に備え構造体にする（REPAIR-4）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) struct ScreenshotOutput {
    /// リダイレクト後の最終 URL（ホストが userinfo 拒否で検証済み）。
    pub(crate) final_url: String,
    /// 画像の幅（ピクセル）。
    pub(crate) width: u32,
    /// 画像の高さ（ピクセル）。
    pub(crate) height: u32,
    /// 書き込んだ PNG のバイト数。
    pub(crate) bytes: usize,
}

impl ScreenshotOutput {
    /// 暫定の 1 行形式で書き出す（スタブ節参照）。出力パスは表示しない。
    pub(crate) fn write_line(&self, out: &mut impl Write) -> io::Result<()> {
        writeln!(
            out,
            "width={} height={} bytes={} url={}",
            self.width, self.height, self.bytes, self.final_url
        )
    }
}

/// URL の事前検証。長さ → `about:blank` → scheme・IP リテラル・localhost（SSRF 既定拒否）。
/// ネットワークには出ない。
pub(crate) fn precheck_url(url: &str) -> Result<(), ScreenshotError> {
    if url.len() > MAX_URL_BYTES {
        return Err(ScreenshotError::Navigate(NavigateError::UrlTooLong {
            limit: MAX_URL_BYTES,
        }));
    }
    if url == "about:blank" {
        return Err(ScreenshotError::NoDocument);
    }
    check_network_target_url(url, false)
        .map_err(|e| ScreenshotError::Navigate(NavigateError::Fetch(e)))?;
    // render の `LoadParams` は authority 内の `@` を拒否する（userinfo 禁止）。同じ条件で事前に弾き、
    // ホスト起動後の汎用 `rendering failed` にしない。資格情報の値は表示しない。
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
        .unwrap_or("");
    if rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .contains('@')
    {
        return Err(ScreenshotError::UserinfoNotAllowed);
    }
    Ok(())
}

/// 中核。URL 検証 → load → capture。レンダラーを注入できる形にしてモックでテストする。
pub(crate) fn capture_with(
    renderer: &dyn Renderer,
    options: &ScreenshotCommandOptions,
) -> Result<(Screenshot, String), ScreenshotError> {
    precheck_url(&options.url)?;
    let load = renderer
        .load(&PageRef::new(options.url.clone()))
        .map_err(ScreenshotError::Render)?;
    let shot = renderer
        .capture_screenshot(&ScreenshotOptions::new())
        .map_err(ScreenshotError::Render)?;
    match shot.format {
        ImageFormat::Png => {}
        _ => return Err(ScreenshotError::UnsupportedFormat),
    }
    Ok((shot, load.final_url().to_string()))
}

/// PNG を排他的に書く（既存ファイルは上書きしない）。失敗時は自分が作ったファイルだけ削除する。
pub(crate) fn write_png(path: &Path, shot: &Screenshot) -> Result<usize, ScreenshotError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| match e.kind() {
            io::ErrorKind::AlreadyExists => {
                ScreenshotError::OutputPath(OutputPathError::AlreadyExists)
            }
            _ => ScreenshotError::Write(e),
        })?;
    let written = file.write_all(&shot.data).and_then(|()| file.sync_all());
    drop(file);
    match written {
        Ok(()) => Ok(shot.data.len()),
        Err(e) => {
            let _ = std::fs::remove_file(path);
            Err(ScreenshotError::Write(e))
        }
    }
}

/// サブコマンド `screenshot` のハンドラ。usage・出力先・URL の誤りではホストを起動しない。
fn run(args: CommandArgs) -> Result<CommandOutcome, CommandError> {
    let options = parse_screenshot_args(&args)?;
    validate_output_path(&options.output)
        .map_err(|e| CommandError::Screenshot(ScreenshotError::OutputPath(e)))?;
    // プロファイル指定の検証（終了コード 2）も副作用・ホスト起動の前に確定させる。
    profile::build_store(&options.profile)?;
    precheck_url(&options.url)?;
    // 指定時のみ開き、コマンド終了までロックを保持する（出力ファイルの書き込みより前）。
    let _profile = profile::open_explicit(&options.profile)?;
    // `--timeout` は `load` の上限だけを変える（スクリーンショット段階の 20 秒は変えない）。
    let renderer =
        render_host::renderer_from_process_with_load_timeout(None, options.load_timeout_ms)?;
    let (shot, final_url) = capture_with(&*renderer, &options)?;
    let bytes = write_png(&options.output, &shot)?;
    ScreenshotOutput {
        final_url,
        width: shot.width,
        height: shot.height,
        bytes,
    }
    .write_line(&mut io::stdout().lock())
    .map_err(CommandError::Output)?;
    Ok(CommandOutcome::Completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::COMMANDS;
    use crate::server::tests::support::TempDir;
    use fandhe_browser_core::Error;
    use fandhe_browser_core::render::{
        BoundingBox, DisabledRenderer, ElementRef, ElementVisibility, PageLoad,
    };
    use std::sync::Mutex;

    fn args(v: &[&str]) -> CommandArgs {
        CommandArgs::new(v.iter().map(|s| s.to_string()).collect())
    }

    fn opts(url: &str) -> ScreenshotCommandOptions {
        ScreenshotCommandOptions {
            url: url.to_string(),
            output: PathBuf::from("out.png"),
            profile: ProfileSelection::OsDefault,
            load_timeout_ms: None,
        }
    }

    type LoadFn = fn() -> Result<PageLoad, RenderError>;

    /// 呼び出しを記録するモック。
    struct Mock {
        load_result: LoadFn,
        loads: Mutex<Vec<String>>,
        full_pages: Mutex<Vec<bool>>,
    }

    impl Mock {
        fn ok() -> Self {
            Self::with(|| Ok(PageLoad::new("https://a.example/")))
        }
        fn with(load_result: LoadFn) -> Self {
            Self {
                load_result,
                loads: Mutex::new(Vec::new()),
                full_pages: Mutex::new(Vec::new()),
            }
        }
    }

    impl Renderer for Mock {
        fn load(&self, page: &PageRef) -> Result<PageLoad, RenderError> {
            self.loads.lock().unwrap().push(page.url().to_string());
            (self.load_result)()
        }
        fn capture_screenshot(&self, o: &ScreenshotOptions) -> Result<Screenshot, RenderError> {
            self.full_pages.lock().unwrap().push(o.full_page);
            Ok(Screenshot::new(
                ImageFormat::Png,
                2,
                3,
                vec![0x89, b'P', b'N', b'G', 1, 2, 3],
            ))
        }
        fn element_visibility(&self, _: &ElementRef) -> Result<ElementVisibility, RenderError> {
            Err(RenderError::ElementNotFound)
        }
        fn bounding_box(&self, _: &ElementRef) -> Result<BoundingBox, RenderError> {
            Err(RenderError::ElementNotFound)
        }
    }

    /// `CLI-1`: screenshot は登録表にちょうど 1 件ある。
    #[test]
    fn cli1_screenshot_is_registered_once() {
        assert_eq!(
            COMMANDS.iter().filter(|c| c.name == "screenshot").count(),
            1
        );
    }

    /// `CLI-1`: 引数解析の正常系。
    #[test]
    fn cli1_screenshot_parse_accepts_profile_dir() {
        let o = parse_screenshot_args(&args(&[
            "--profile-dir",
            "d",
            "https://a.example/",
            "o.png",
        ]))
        .expect("ok");
        assert_eq!(o.profile, ProfileSelection::Explicit("d".into()));
    }

    #[test]
    fn cli1_screenshot_parse_accepts_valid_forms() {
        let base = ScreenshotCommandOptions {
            url: "https://a.example/".into(),
            output: PathBuf::from("o.png"),
            profile: ProfileSelection::OsDefault,
            load_timeout_ms: None,
        };
        assert_eq!(
            parse_screenshot_args(&args(&["https://a.example/", "o.png"])).unwrap(),
            base
        );
        let dashed = parse_screenshot_args(&args(&["--", "https://a.example/", "-out.png"]));
        assert_eq!(dashed.unwrap().output, PathBuf::from("-out.png"));
    }

    /// `CLI-1`: 引数解析の異常系。
    #[test]
    fn cli1_screenshot_parse_rejects_invalid_forms() {
        assert_eq!(
            parse_screenshot_args(&args(&[])).unwrap_err(),
            UsageError::MissingArgument("URL")
        );
        assert_eq!(
            parse_screenshot_args(&args(&["https://a.example/"])).unwrap_err(),
            UsageError::MissingArgument("OUTPUT")
        );
        assert_eq!(
            parse_screenshot_args(&args(&["u", "o", "x"])).unwrap_err(),
            UsageError::UnexpectedArgument("x".into())
        );
        assert_eq!(
            parse_screenshot_args(&args(&["--bogus", "u", "o"])).unwrap_err(),
            UsageError::UnknownOption("--bogus".into())
        );
        assert_eq!(
            parse_screenshot_args(&args(&["--full-page=1", "u", "o"])).unwrap_err(),
            UsageError::UnknownOption("--full-page=1".into())
        );
        let unsupported = UsageError::UnsupportedOption("--full-page");
        for form in [
            ["--full-page", "u", "o"],
            ["u", "--full-page", "o"],
            ["u", "o", "--full-page"],
        ] {
            assert_eq!(
                parse_screenshot_args(&args(&form)).unwrap_err(),
                unsupported
            );
        }
        // `--` より後ろは位置引数として扱うため拒否されない。
        assert!(parse_screenshot_args(&args(&["--", "--full-page", "o"])).is_ok());
    }

    fn timeout_of(v: &[&str]) -> Result<Option<u32>, UsageError> {
        parse_screenshot_args(&args(v)).map(|o| o.load_timeout_ms)
    }

    /// `CLI-1`・#1301: `--timeout` の正常値（境界 1・60 を含む）は秒からミリ秒へ変換され、未指定は `None`。
    #[test]
    fn cli1_screenshot_timeout_accepts_valid_seconds() {
        for (given, ms) in [("1", 1_000), ("30", 30_000), ("60", 60_000), ("9", 9_000)] {
            let o =
                parse_screenshot_args(&args(&["--timeout", given, "https://a.example/", "o.png"]))
                    .expect(given);
            assert_eq!(o.load_timeout_ms, Some(ms), "{given}");
            assert_eq!(o.url, "https://a.example/");
        }
        assert_eq!(timeout_of(&["u", "o"]), Ok(None));
        // 位置引数の後ろ・`--profile-dir` との併用・`--` の直前でも解析できる。
        assert_eq!(timeout_of(&["u", "--timeout", "5", "o"]), Ok(Some(5_000)));
        assert_eq!(
            timeout_of(&["--profile-dir", "p", "--timeout", "7", "u", "o"]),
            Ok(Some(7_000))
        );
        assert_eq!(
            timeout_of(&["--timeout", "60", "--", "u", "o"]),
            Ok(Some(60_000))
        );
    }

    /// `CLI-1`・#1301: 範囲外・非数・符号・小数・空文字は丸めず `InvalidOptionValue`（終了コード 2）。
    #[test]
    fn cli1_screenshot_timeout_rejects_invalid_values() {
        for bad in [
            "0",
            "61",
            "100",
            "99999999999999999999",
            "abc",
            "10s",
            "+5",
            "5.0",
            "1.5",
            " 5",
            "5 ",
            "",
            "05",
            "00",
            "0x10",
            "\u{665}",
        ] {
            let e = timeout_of(&["--timeout", bad, "u", "o"]).expect_err(bad);
            assert_eq!(
                e,
                UsageError::InvalidOptionValue {
                    option: "--timeout"
                },
                "{bad:?}"
            );
            let ce = CommandError::from(e);
            assert_eq!(
                ce.to_string(),
                "invalid value for option '--timeout'; run 'fandhe-browser --help' for usage"
            );
            assert_eq!(
                format!("{:?}", ce.exit_code()),
                format!("{:?}", std::process::ExitCode::from(2))
            );
        }
    }

    /// `CLI-1`・#1301: 値の欠落（末尾・後続のオプション・符号つき）と重複指定、`--timeout=N` 形式は usage エラー。
    #[test]
    fn cli1_screenshot_timeout_rejects_missing_duplicate_and_equals_form() {
        let missing = UsageError::MissingArgument("SECONDS");
        for a in [
            vec!["u", "o", "--timeout"],
            vec!["--timeout", "--profile-dir", "p", "u", "o"],
            vec!["--timeout", "-5", "u", "o"],
            vec!["--timeout", "--", "u", "o"],
        ] {
            assert_eq!(timeout_of(&a), Err(missing.clone()), "{a:?}");
        }
        assert_eq!(
            timeout_of(&["--timeout", "5", "--timeout", "6", "u", "o"]),
            Err(UsageError::UnexpectedArgument("--timeout".into()))
        );
        assert_eq!(
            timeout_of(&["--timeout=5", "u", "o"]),
            Err(UsageError::UnknownOption("--timeout=5".into()))
        );
    }

    /// `CLI-1`・#1301: `--` より後ろの `--timeout` は位置引数として扱い、オプションとして解釈しない。
    #[test]
    fn cli1_screenshot_timeout_after_double_dash_is_positional() {
        let o = parse_screenshot_args(&args(&["--", "--timeout", "o.png"])).expect("ok");
        assert_eq!(o.url, "--timeout");
        assert_eq!(o.output, PathBuf::from("o.png"));
        assert_eq!(o.load_timeout_ms, None);
        // `--timeout` は OUTPUT、`5` は余分な位置引数になる（値としては消費されない）。
        assert_eq!(
            parse_screenshot_args(&args(&["--", "u", "--timeout", "5"])).unwrap_err(),
            UsageError::UnexpectedArgument("5".into())
        );
    }

    /// `CLI-1`: 存在しないディレクトリ配下は panic せずエラーになる。
    #[test]
    fn cli1_screenshot_output_parent_missing() {
        let dir = TempDir::new();
        let err = validate_output_path(&dir.owned.join("no-such").join("o.png")).unwrap_err();
        assert!(matches!(err, OutputPathError::ParentNotFound), "{err:?}");
        assert_eq!(err.to_string(), "output directory does not exist");
        let cmd = CommandError::Screenshot(ScreenshotError::OutputPath(err));
        assert_eq!(cmd.exit_code(), std::process::ExitCode::FAILURE);
    }

    /// `CLI-1`: 出力先パスのその他の不備。
    #[test]
    fn cli1_screenshot_output_path_rejections() {
        let dir = TempDir::new();
        let file = dir.owned.join("f.png");
        std::fs::write(&file, b"keep").unwrap();
        assert!(matches!(
            validate_output_path(&file.join("o.png")).unwrap_err(),
            OutputPathError::ParentNotDirectory
        ));
        assert!(matches!(
            validate_output_path(&dir.owned).unwrap_err(),
            OutputPathError::IsDirectory
        ));
        assert!(matches!(
            validate_output_path(&file).unwrap_err(),
            OutputPathError::AlreadyExists
        ));
        assert!(matches!(
            validate_output_path(Path::new("")).unwrap_err(),
            OutputPathError::Empty
        ));
        // Windows の verbatim パスへ `join("..")` すると `..` が正規化されて親ディレクトリになるため、
        // 文字列で連結して `..` 終端のパスを組み立てる。
        let dotdot = PathBuf::from(format!("{}{MAIN_SEPARATOR}..", dir.owned.display()));
        assert!(matches!(
            validate_output_path(&dotdot).unwrap_err(),
            OutputPathError::NotAFilePath
        ));
        let trailing = format!("{}{MAIN_SEPARATOR}x{MAIN_SEPARATOR}", dir.owned.display());
        assert!(matches!(
            validate_output_path(Path::new(&trailing)).unwrap_err(),
            OutputPathError::NotAFilePath
        ));
        // 空の親は `.` として扱い、cwd に無い名前は通る（書き込みはしない）。
        assert!(validate_output_path(Path::new("fandhe-cli-nonexistent-out.png")).is_ok());
        assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    }

    /// `CLI-1`: ぶら下がりシンボリックリンクも既存扱いにする（unix のみ）。
    #[cfg(unix)]
    #[test]
    fn cli1_screenshot_output_dangling_symlink_is_rejected() {
        let dir = TempDir::new();
        let link = dir.owned.join("link.png");
        std::os::unix::fs::symlink(dir.owned.join("target-missing"), &link).unwrap();
        assert!(matches!(
            validate_output_path(&link).unwrap_err(),
            OutputPathError::AlreadyExists
        ));
    }

    /// `CLI-1`: 成功モックから final_url・寸法・バイト列が返り、ビューポートのみを要求する。
    #[test]
    fn cli1_screenshot_capture_with_returns_result_and_requests_viewport_only() {
        let mock = Mock::ok();
        let (shot, url) = capture_with(&mock, &opts("https://a.example/")).unwrap();
        assert_eq!(url, "https://a.example/");
        assert_eq!((shot.width, shot.height), (2, 3));
        assert_eq!(shot.data, vec![0x89, b'P', b'N', b'G', 1, 2, 3]);
        assert_eq!(*mock.full_pages.lock().unwrap(), vec![false]);
        assert_eq!(*mock.loads.lock().unwrap(), vec!["https://a.example/"]);
    }

    /// `CLI-1`: 無効レンダラーは成功を装わず固定文言で失敗する。
    #[test]
    fn cli1_screenshot_disabled_renderer_fails_closed() {
        let err = capture_with(&DisabledRenderer::new(), &opts("https://a.example/")).unwrap_err();
        assert!(matches!(
            err,
            ScreenshotError::Render(RenderError::RenderingDisabled)
        ));
        assert_eq!(
            err.to_string(),
            "rendering is not available (render host not found; set FANDHE_BROWSER_RENDER_HOST)"
        );
    }

    /// `CLI-1`: load 失敗の表示は固定文言で、Internal の詳細を含まない。
    #[test]
    fn cli1_screenshot_load_failures_have_fixed_messages() {
        let cases: [(LoadFn, &str); 3] = [
            (|| Err(RenderError::Timeout), "render timed out"),
            (
                || Err(RenderError::HostUnavailable),
                "render host is unavailable",
            ),
            (
                || Err(RenderError::Internal("/secret/path".into())),
                "rendering failed",
            ),
        ];
        for (f, expected) in cases {
            let err = capture_with(&Mock::with(f), &opts("https://a.example/")).unwrap_err();
            assert_eq!(err.to_string(), expected);
            assert!(!format!("{err:?}").contains("secret"));
        }
    }

    /// `CLI-1`: URL の事前検証に落ちたらレンダラーの load を呼ばない。
    #[test]
    fn cli1_screenshot_precheck_rejects_before_load() {
        let long = format!("https://a.example/{}", "a".repeat(MAX_URL_BYTES));
        let cases: Vec<(String, &str)> = vec![
            ("file:///etc/passwd".into(), "unsupported URL scheme"),
            ("http://127.0.0.1:9/".into(), "private or loopback"),
            ("not a url".into(), "invalid URL"),
            (long, "URL is too long (limit 2048 bytes)"),
            (
                "about:blank".into(),
                "screenshot requires a fetched document (about:blank has no content)",
            ),
        ];
        for (url, needle) in cases {
            let mock = Mock::ok();
            let err = capture_with(&mock, &opts(&url)).unwrap_err();
            assert!(err.to_string().contains(needle), "{url}: {err}");
            assert!(mock.loads.lock().unwrap().is_empty());
        }
        assert!(matches!(
            precheck_url("file:///etc/passwd").unwrap_err(),
            ScreenshotError::Navigate(NavigateError::Fetch(Error::DisallowedScheme { .. }))
        ));
        for url in [
            "https://user:pass@example.com/",
            "https://user@example.com/",
        ] {
            let mock = Mock::ok();
            let err = capture_with(&mock, &opts(url)).unwrap_err();
            assert!(matches!(err, ScreenshotError::UserinfoNotAllowed), "{err}");
            assert_eq!(err.to_string(), "URL must not contain userinfo");
            assert!(mock.loads.lock().unwrap().is_empty());
        }
        assert!(matches!(
            precheck_url("http://127.0.0.1:9/").unwrap_err(),
            ScreenshotError::Navigate(NavigateError::Fetch(Error::DisallowedAddress { .. }))
        ));
    }

    /// `CLI-1`: 書き込みはバイト列どおりで、既存ファイルは上書きしない。
    #[test]
    fn cli1_screenshot_write_png_is_exclusive() {
        let dir = TempDir::new();
        let path = dir.owned.join("o.png");
        let shot = Screenshot::new(ImageFormat::Png, 2, 3, vec![1, 2, 3, 4]);
        assert_eq!(write_png(&path, &shot).unwrap(), 4);
        assert_eq!(std::fs::read(&path).unwrap(), vec![1, 2, 3, 4]);
        let other = Screenshot::new(ImageFormat::Png, 1, 1, vec![9]);
        let err = write_png(&path, &other).unwrap_err();
        assert!(matches!(
            err,
            ScreenshotError::OutputPath(OutputPathError::AlreadyExists)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), vec![1, 2, 3, 4]);
    }

    /// `CLI-1`: 暫定出力行の形式。
    #[test]
    fn cli1_screenshot_write_line_format() {
        let out = ScreenshotOutput {
            final_url: "https://a.example/".into(),
            width: 2,
            height: 3,
            bytes: 7,
        };
        let mut buf = Vec::new();
        out.write_line(&mut buf).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "width=2 height=3 bytes=7 url=https://a.example/\n"
        );
    }
}
