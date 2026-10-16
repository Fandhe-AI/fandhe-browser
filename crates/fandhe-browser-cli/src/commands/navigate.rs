//! ナビゲーションコマンド `navigate`（TASK-47.3・Issue #284・`CLI-1`・MS-5）。
//!
//! `navigate <URL>` はプロセス内で core の [`Fetcher`] を構築して URL を 1 回取得し、最終 URL と
//! ステータスを stdout に 1 行出して終了する（one-shot）。実行中の `launch` インスタンスへ接続する
//! 方式は採らない（ai に `/ai/navigate` が無く、CDP クライアントには新規依存が要るため。#281 で依存追加は禁止）。
//! そのため後続の query（#285）・screenshot（#286）は「直前の CLI 呼び出しで開いたページ」を参照できない。
//! 取得の中核 [`fetch_page`] を `pub(crate)` で置き、query が再利用できるようにしている
//! （URL 長検査・SSRF 検証を経由しない取得経路を作らないため、取得はここに一本化する）。
//! screenshot は描画ホストが自分で取得するため `fetch_page` を使わず、URL 検査は core の
//! `check_network_target_url` と `HostRenderer` で行う。
//!
//! `cfg(target_os)` 分岐は置かず 3 OS で同一の引数体系・挙動とする（`CLI-1`）。
//! 取得は `FetchOptions::default()`（ループバック・プライベートアドレスを拒否する SSRF 安全側の既定）で行い、
//! 緩和オプションは提供しない。4xx/5xx は CDP の `Page.navigate` と同じく完了したナビゲーションとして
//! ステータス付きで成功終了する。接続・DNS・タイムアウト等の失敗は `Result` のエラー（終了コード 1）。
//!
//! # スタブについて
//!
//! 以下は未実装・暫定で、実装済みを装わない（REPAIR-3）。
//!
//! - 出力形式 `status=<code> url=<final_url>` は暫定。`CLI-3` の出力形式の確定と合わせて見直す
//! - `--profile`: #287（TASK-47.6）で [`NavigateOptions`] に追加する。Cookie 連携は TASK-116・`PROF-7`
//! - ページ JS の実行（`[js] execute_page_scripts`）: 未対応（HTML 本文も出力しない）
//! - `navigate --help`: ハンドラは出力先を受け取らないため未対応（トップレベルの `--help` には載る）

use std::fmt;
use std::io::{self, Write};

use fandhe_browser_core::{Error, FetchOptions, FetchResponse, Fetcher};

use super::{CommandArgs, CommandError, CommandOutcome, CommandSpec, UsageError};

/// 登録表（[`super::COMMANDS`]）へ載せる仕様。
pub(crate) const SPEC: CommandSpec = CommandSpec {
    name: "navigate",
    summary: "Fetch a URL once and print the final URL and status",
    run,
};

/// URL の最大バイト長。cdp の `target::MAX_URL_LEN`（crate ルートから再エクスポートされていない）と揃える。
const MAX_URL_LEN: usize = 8192;

/// `navigate` の解析済みオプション。#287 のプロファイル指定の拡張点。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) struct NavigateOptions {
    /// 取得先 URL。
    pub(crate) url: String,
}

/// `navigate` の引数を解析する（副作用なし）。
pub(crate) fn parse_navigate_args(args: &CommandArgs) -> Result<NavigateOptions, UsageError> {
    let rest = args.as_slice();
    match rest.first() {
        Some(a) if a.starts_with('-') => return Err(UsageError::UnknownOption(a.clone())),
        None => return Err(UsageError::MissingArgument("URL")),
        Some(_) => {}
    }
    match rest.get(1) {
        Some(a) if a.starts_with('-') => Err(UsageError::UnknownOption(a.clone())),
        Some(a) => Err(UsageError::UnexpectedArgument(a.clone())),
        None => Ok(NavigateOptions {
            url: rest.first().cloned().unwrap_or_default(),
        }),
    }
}

/// ナビゲーション結果。将来の拡張に備え構造体にする（REPAIR-4）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) struct NavigateOutput {
    /// リダイレクト後の最終 URL（userinfo 除去済み）。
    pub(crate) final_url: String,
    /// HTTP ステータス。`about:blank` は取得しないため `None`。
    pub(crate) status: Option<u16>,
}

impl NavigateOutput {
    /// 暫定の 1 行形式で書き出す（スタブ節参照）。
    pub(crate) fn write_line(&self, out: &mut impl Write) -> io::Result<()> {
        match self.status {
            Some(s) => writeln!(out, "status={s} url={}", self.final_url),
            None => writeln!(out, "url={}", self.final_url),
        }
    }
}

/// ナビゲーション失敗。入力 URL・アドレスは表示に含めない（端末出力の安全・userinfo 漏えい防止）。
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum NavigateError {
    /// URL が上限を超えた。
    UrlTooLong { limit: usize },
    /// 検証・取得の失敗。
    Fetch(Error),
    /// ランタイム構築の失敗。
    Runtime(io::Error),
}

impl fmt::Display for NavigateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UrlTooLong { limit } => write!(f, "URL is too long (limit {limit} bytes)"),
            Self::Fetch(e) => f.write_str(match e {
                Error::InvalidInput { .. } => "invalid URL",
                Error::DisallowedScheme { .. } => {
                    "unsupported URL scheme (only http, https and about:blank are allowed)"
                }
                Error::DisallowedAddress { .. } => {
                    "navigation to private or loopback address is not allowed"
                }
                Error::Timeout { .. } => "request timed out",
                Error::TooManyRedirects { .. } | Error::CrossOriginRedirect => "too many redirects",
                Error::ResponseTooLarge { .. } => "response too large",
                Error::Network { .. } | Error::TooManyConcurrentDnsResolutions { .. } => {
                    "could not reach host"
                }
                _ => "navigation failed",
            }),
            Self::Runtime(_) => f.write_str("failed to start async runtime"),
        }
    }
}

impl std::error::Error for NavigateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UrlTooLong { .. } => None,
            Self::Fetch(e) => Some(e),
            Self::Runtime(e) => Some(e),
        }
    }
}

impl From<Error> for NavigateError {
    fn from(e: Error) -> Self {
        Self::Fetch(e)
    }
}

/// [`fetch_page`] の結果。
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum FetchedPage {
    /// `about:blank`。取得せず、本文も無い。
    Blank,
    /// 取得済みの応答（4xx/5xx も完了したナビゲーションとして含む）。
    Fetched(FetchResponse),
}

/// 取得の中核。URL 長検査 → `about:blank` 特例 → 事前検証 → 取得 → 最終 URL 長検査。
///
/// `navigate_with` のほか、query（#285）・screenshot（#286）が呼ぶ。
pub(crate) async fn fetch_page(fetcher: &Fetcher, url: &str) -> Result<FetchedPage, NavigateError> {
    if url.len() > MAX_URL_LEN {
        return Err(NavigateError::UrlTooLong { limit: MAX_URL_LEN });
    }
    if url == "about:blank" {
        return Ok(FetchedPage::Blank);
    }
    fetcher.validate_url(url)?;
    let response = fetcher.get(url).await?;
    if response.final_url().len() > MAX_URL_LEN {
        return Err(NavigateError::UrlTooLong { limit: MAX_URL_LEN });
    }
    Ok(FetchedPage::Fetched(response))
}

/// `navigate` の中核。[`fetch_page`] の結果を最終 URL とステータスへ写す。
pub(crate) async fn navigate_with(
    fetcher: &Fetcher,
    url: &str,
) -> Result<NavigateOutput, NavigateError> {
    Ok(match fetch_page(fetcher, url).await? {
        FetchedPage::Blank => NavigateOutput {
            final_url: url.to_string(),
            status: None,
        },
        FetchedPage::Fetched(response) => NavigateOutput {
            final_url: response.final_url().to_string(),
            status: Some(response.status()),
        },
    })
}

/// サブコマンド `navigate` のハンドラ。usage エラーではランタイムも Fetcher も作らない。
fn run(args: CommandArgs) -> Result<CommandOutcome, CommandError> {
    let options = parse_navigate_args(&args)?;
    let fetcher = Fetcher::new(FetchOptions::default()).map_err(NavigateError::from)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(NavigateError::Runtime)?;
    let output = runtime.block_on(navigate_with(&fetcher, &options.url))?;
    output
        .write_line(&mut io::stdout().lock())
        .map_err(CommandError::Output)?;
    Ok(CommandOutcome::Completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::{block_on, loopback_fetcher, serve_once};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn args(v: &[&str]) -> CommandArgs {
        CommandArgs::new(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn cli1_navigate_is_registered() {
        let n = crate::commands::COMMANDS
            .iter()
            .filter(|c| c.name == "navigate")
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn cli1_navigate_requires_url() {
        let e = parse_navigate_args(&args(&[])).expect_err("missing");
        assert_eq!(e, UsageError::MissingArgument("URL"));
        let ce = CommandError::from(e);
        assert_eq!(
            ce.to_string(),
            "missing required argument 'URL'; run 'fandhe-browser --help' for usage"
        );
        assert_eq!(
            format!("{:?}", ce.exit_code()),
            format!("{:?}", std::process::ExitCode::from(2))
        );
    }

    #[test]
    fn cli1_navigate_rejects_unknown_option() {
        let e = parse_navigate_args(&args(&["--bogus"])).expect_err("option");
        assert_eq!(e, UsageError::UnknownOption("--bogus".into()));
    }

    #[test]
    fn cli1_navigate_rejects_extra_positional() {
        let e = parse_navigate_args(&args(&["https://a.example/", "extra"])).expect_err("extra");
        assert_eq!(e, UsageError::UnexpectedArgument("extra".into()));
    }

    #[test]
    fn cli1_navigate_parses_single_url() {
        let o = parse_navigate_args(&args(&["https://a.example/"])).expect("ok");
        assert_eq!(o.url, "https://a.example/");
    }

    #[test]
    fn cli1_navigate_invalid_url_is_error() {
        let f = Fetcher::new(FetchOptions::default()).expect("fetcher");
        let e = block_on(navigate_with(&f, "not a url")).expect_err("invalid");
        assert!(matches!(
            e,
            NavigateError::Fetch(Error::InvalidInput { .. })
        ));
        assert_eq!(e.to_string(), "invalid URL");
    }

    #[test]
    fn cli1_navigate_rejects_file_scheme() {
        let f = Fetcher::new(FetchOptions::default()).expect("fetcher");
        let e = block_on(navigate_with(&f, "file:///etc/passwd")).expect_err("scheme");
        assert!(matches!(
            e,
            NavigateError::Fetch(Error::DisallowedScheme { .. })
        ));
    }

    #[test]
    fn cli1_navigate_rejects_loopback_by_default() {
        let f = Fetcher::new(FetchOptions::default()).expect("fetcher");
        let e = block_on(navigate_with(&f, "http://127.0.0.1:9/")).expect_err("ssrf");
        assert!(matches!(
            e,
            NavigateError::Fetch(Error::DisallowedAddress { .. })
        ));
        assert_eq!(
            e.to_string(),
            "navigation to private or loopback address is not allowed"
        );
    }

    #[test]
    fn cli1_navigate_rejects_too_long_url() {
        let f = Fetcher::new(FetchOptions::default()).expect("fetcher");
        let url = format!("https://a.example/{}", "x".repeat(MAX_URL_LEN));
        let e = block_on(navigate_with(&f, &url)).expect_err("long");
        assert!(matches!(e, NavigateError::UrlTooLong { limit: 8192 }));
    }

    #[test]
    fn cli1_navigate_about_blank_skips_fetch() {
        let f = Fetcher::new(FetchOptions::default()).expect("fetcher");
        let o = block_on(navigate_with(&f, "about:blank")).expect("ok");
        assert_eq!(o.final_url, "about:blank");
        assert_eq!(o.status, None);
        let mut out = Vec::new();
        o.write_line(&mut out).expect("write");
        assert_eq!(out, b"url=about:blank\n");
    }

    #[test]
    fn cli1_navigate_success_prints_status_and_url() {
        let port =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        let url = format!("http://127.0.0.1:{port}/");
        let o = block_on(navigate_with(&loopback_fetcher(), &url)).expect("ok");
        assert_eq!(o.status, Some(200));
        assert_eq!(o.final_url, url);
        let mut out = Vec::new();
        o.write_line(&mut out).expect("write");
        assert_eq!(out, format!("status=200 url={url}\n").into_bytes());
    }

    #[test]
    fn cli1_navigate_404_is_completed_navigation() {
        let port =
            serve_once("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let url = format!("http://127.0.0.1:{port}/missing");
        let o = block_on(navigate_with(&loopback_fetcher(), &url)).expect("ok");
        assert_eq!(o.status, Some(404));
    }

    #[test]
    fn cli1_navigate_unreachable_is_error() {
        // 接続を受け付けて応答せず閉じる。ポートの所有をテスト中保持し、確定的に失敗させる。
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        thread::spawn(move || {
            if let Ok((s, _)) = listener.accept() {
                drop::<TcpStream>(s);
            }
        });
        let url = format!("http://127.0.0.1:{port}/");
        let e = block_on(navigate_with(&loopback_fetcher(), &url)).expect_err("network");
        assert!(
            matches!(e, NavigateError::Fetch(Error::Network { .. })),
            "{e:?}"
        );
        assert_eq!(e.to_string(), "could not reach host");
    }
}
