//! 要素取得コマンド `query`（TASK-47.4・Issue #285・`CLI-1`・MS-5）。
//!
//! `query [--all | --attr NAME] [--] <URL> <SELECTOR>` はプロセス内で core の [`Fetcher`] により URL を
//! 1 回取得し、HTML をパースして CSS セレクタで照会し、結果を stdout に出して終了する（one-shot）。
//! PoC-2 の CLI にあった `text`（既定）・`texts`（`--all`）・`attr`（`--attr NAME`）の 3 機能に対応する。
//! 実行中の `launch` インスタンスへは接続しない（理由は [`super::navigate`] の module doc を参照）ため、
//! 直前の `navigate` で開いたページは参照できない。
//!
//! 取得は [`super::navigate::fetch_page`] に一本化している（URL 長検査・SSRF 検証を経由しない経路を作らない）。
//! `FetchOptions::default()`（ループバック・プライベートアドレスを拒否する安全側の既定）で行い、
//! 緩和オプションは提供しない。4xx/5xx の本文も照会対象とする（`navigate` と同じ扱い）。
//! 照会は core の `query_selector_str` / `query_selector_all_str` に任せ、セレクタ長の上限と未対応構文の
//! 検査も core が行う。`cfg(target_os)` 分岐は置かず 3 OS で同一の引数体系・挙動とする（`CLI-1`）。
//!
//! 出力は JSON 文字列リテラルを 1 値 1 行で出す。埋め込み改行を含むテキストでも `--all` の境界が曖昧にならず、
//! untrusted なページ由来の制御文字（ANSI エスケープ等）が端末へ生で届かない。
//!
//! # スタブについて
//!
//! 以下は未実装・暫定で、実装済みを装わない（REPAIR-3）。
//!
//! - 出力形式（JSON 文字列リテラルの 1 行 1 値）は暫定。`CLI-3` の出力形式の確定と合わせて見直し、
//!   その際に上限値も見直す（`--all` は件数 [`MAX_RESULT_COUNT`]・合計バイト数 [`MAX_RESULT_BYTES`] を超えるとエラー）
//! - `--profile`: #287（TASK-47.6）で [`QueryOptions`] に追加する
//! - ページ JS の実行（`[js] execute_page_scripts`）: 未対応。JS が生成する要素は取れない
//! - 文字コード判定（`CORE-5` (7)）: 未対応。本文は lossy な UTF-8 として扱う
//! - `query --help`: ハンドラは出力先を受け取らないため未対応（トップレベルの `--help` には載る）

use std::fmt;
use std::io::{self, Write};

use fandhe_browser_core::{
    Document, Error, FetchOptions, Fetcher, ParseOptions, parse_document, query_selector_all_str,
    query_selector_str,
};

use super::navigate::{self, FetchedPage, NavigateError};
use super::{CommandArgs, CommandError, CommandOutcome, CommandSpec, UsageError};

/// 登録表（[`super::COMMANDS`]）へ載せる仕様。
pub(crate) const SPEC: CommandSpec = CommandSpec {
    name: "query",
    summary: "Fetch a URL once and print text or an attribute of elements matching a CSS selector",
    run,
};

/// 属性名の最大バイト長（巨大引数の拒否）。
const MAX_ATTR_NAME_LEN: usize = 256;

/// `--all` が保持する結果の合計バイト数の上限（入れ子 HTML による子孫テキストの複製増幅で OOM させない）。
const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;

/// `--all` が保持する結果の最大件数。
const MAX_RESULT_COUNT: usize = 10_000;

/// 取得する内容の種別。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum QueryMode {
    /// 最初に一致した要素の `text_content`（PoC-2 の `text`）。
    FirstText,
    /// 一致した全要素の `text_content`（PoC-2 の `texts`）。
    AllTexts,
    /// 最初に一致した要素の属性値（PoC-2 の `attr`）。
    Attribute(String),
}

/// `query` の解析済みオプション。#287 のプロファイル指定の拡張点。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) struct QueryOptions {
    /// 取得先 URL。
    pub(crate) url: String,
    /// CSS セレクタ。
    pub(crate) selector: String,
    /// 取得する内容の種別。
    pub(crate) mode: QueryMode,
}

/// `query` の引数を解析する（副作用なし）。`--` より後ろはすべて位置引数として扱う。
pub(crate) fn parse_query_args(args: &CommandArgs) -> Result<QueryOptions, UsageError> {
    let mut all = false;
    let mut attr: Option<String> = None;
    let mut positionals: Vec<&String> = Vec::new();
    let mut only_positionals = false;
    let mut iter = args.as_slice().iter();
    while let Some(arg) = iter.next() {
        if only_positionals || !arg.starts_with('-') {
            positionals.push(arg);
            continue;
        }
        match arg.as_str() {
            "--" => only_positionals = true,
            "--all" => {
                if all {
                    return Err(UsageError::UnexpectedArgument(arg.clone()));
                }
                if attr.is_some() {
                    return Err(UsageError::ConflictingOptions {
                        first: "--attr",
                        second: "--all",
                    });
                }
                all = true;
            }
            "--attr" => {
                if attr.is_some() {
                    return Err(UsageError::UnexpectedArgument(arg.clone()));
                }
                if all {
                    return Err(UsageError::ConflictingOptions {
                        first: "--all",
                        second: "--attr",
                    });
                }
                let name = iter.next().ok_or(UsageError::MissingArgument("NAME"))?;
                if name.is_empty() || name.len() > MAX_ATTR_NAME_LEN {
                    return Err(UsageError::InvalidOptionValue { option: "--attr" });
                }
                attr = Some(name.clone());
            }
            _ => return Err(UsageError::UnknownOption(arg.clone())),
        }
    }
    let mut pos = positionals.into_iter();
    let url = pos.next().ok_or(UsageError::MissingArgument("URL"))?;
    let selector = pos.next().ok_or(UsageError::MissingArgument("SELECTOR"))?;
    if let Some(extra) = pos.next() {
        return Err(UsageError::UnexpectedArgument(extra.clone()));
    }
    let mode = match (all, attr) {
        (_, Some(name)) => QueryMode::Attribute(name),
        (true, None) => QueryMode::AllTexts,
        (false, None) => QueryMode::FirstText,
    };
    Ok(QueryOptions {
        url: url.clone(),
        selector: selector.clone(),
        mode,
    })
}

/// `query` の失敗。URL・セレクタ・属性名は表示に含めない（端末出力の安全・userinfo 漏えい防止）。
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum QueryError {
    /// 取得系の失敗（表示は [`NavigateError`] の文言を流用する）。
    Navigate(NavigateError),
    /// `about:blank` は取得しないため照会できる文書が無い。
    NoDocument,
    /// セレクタの不正・未対応・照会の失敗。
    Selector(Error),
    /// HTML のパース失敗。
    Parse(Error),
    /// セレクタに一致する要素が無い。
    ElementNotFound,
    /// 一致した要素に指定の属性が無い。
    AttributeNotFound,
    /// `--all` の結果が件数または合計バイト数の上限を超えた。
    ResultTooLarge,
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Navigate(e) => write!(f, "{e}"),
            Self::NoDocument => {
                f.write_str("query requires a fetched document (about:blank has no content)")
            }
            Self::Selector(Error::InvalidInput { .. }) => f.write_str("invalid selector"),
            Self::Selector(Error::Unsupported { .. }) => f.write_str("unsupported selector syntax"),
            Self::Selector(_) => f.write_str("query failed"),
            Self::Parse(_) => f.write_str("failed to parse HTML"),
            Self::ElementNotFound => f.write_str("no element matches the selector"),
            Self::AttributeNotFound => f.write_str("attribute not found on the matched element"),
            Self::ResultTooLarge => f.write_str("query result exceeds the size limit"),
        }
    }
}

impl std::error::Error for QueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Navigate(e) => Some(e),
            Self::Selector(e) | Self::Parse(e) => Some(e),
            Self::NoDocument
            | Self::ElementNotFound
            | Self::AttributeNotFound
            | Self::ResultTooLarge => None,
        }
    }
}

impl From<NavigateError> for QueryError {
    fn from(e: NavigateError) -> Self {
        Self::Navigate(e)
    }
}

/// 照会結果。将来の拡張に備え構造体にする（REPAIR-4）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) struct QueryOutput {
    /// 取得した値（文書順）。
    pub(crate) values: Vec<String>,
}

impl QueryOutput {
    /// 暫定形式（1 値 1 行の JSON 文字列リテラル。スタブ節参照）で書き出す。
    pub(crate) fn write_lines(&self, out: &mut impl Write) -> io::Result<()> {
        for v in &self.values {
            let mut line = String::new();
            write_json_string(&mut line, v);
            line.push('\n');
            out.write_all(line.as_bytes())?;
        }
        Ok(())
    }
}

/// `s` を JSON 文字列リテラルとして `out` へ追記する（依存なしの手書き）。
///
/// `"`・`\`・C0 制御に加え、端末や行処理を乱し得る U+007F・C1 制御・U+2028/U+2029 もエスケープする。
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20
                || (0x7f..=0x9f).contains(&(c as u32))
                || c == '\u{2028}'
                || c == '\u{2029}' =>
            {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// パース済み文書を照会して値列を返す。ネットワークに依存しない中核。
///
/// 一致なし（`--all` の 0 件を含む）は [`QueryError::ElementNotFound`]、
/// `--attr` で属性が無い場合は [`QueryError::AttributeNotFound`]。
pub(crate) fn query_document(
    doc: &Document,
    mode: &QueryMode,
    selector: &str,
) -> Result<Vec<String>, QueryError> {
    match mode {
        QueryMode::AllTexts => {
            let ids =
                query_selector_all_str(doc, doc.root(), selector).map_err(QueryError::Selector)?;
            if ids.is_empty() {
                return Err(QueryError::ElementNotFound);
            }
            if ids.len() > MAX_RESULT_COUNT {
                return Err(QueryError::ResultTooLarge);
            }
            // 入れ子で子孫テキストが要素ごとに複製されるため、累計が上限を超えた時点で打ち切る。
            let mut total = 0usize;
            let mut values = Vec::with_capacity(ids.len());
            for id in ids {
                let text = doc.text_content(id).unwrap_or_default();
                total = total.saturating_add(text.len());
                if total > MAX_RESULT_BYTES {
                    return Err(QueryError::ResultTooLarge);
                }
                values.push(text);
            }
            Ok(values)
        }
        QueryMode::FirstText | QueryMode::Attribute(_) => {
            let id = query_selector_str(doc, doc.root(), selector)
                .map_err(QueryError::Selector)?
                .ok_or(QueryError::ElementNotFound)?;
            match mode {
                QueryMode::Attribute(name) => doc
                    .attribute(id, name)
                    .map(|v| vec![v.to_string()])
                    .ok_or(QueryError::AttributeNotFound),
                _ => Ok(vec![doc.text_content(id).unwrap_or_default()]),
            }
        }
    }
}

/// 取得 → パース → 照会の一連の処理。
pub(crate) async fn query_with(
    fetcher: &Fetcher,
    options: &QueryOptions,
) -> Result<QueryOutput, QueryError> {
    let response = match navigate::fetch_page(fetcher, &options.url).await? {
        FetchedPage::Blank => return Err(QueryError::NoDocument),
        FetchedPage::Fetched(r) => r,
    };
    let html = response.body_text_lossy();
    let parsed = parse_document(&html, &ParseOptions::default()).map_err(QueryError::Parse)?;
    let values = query_document(&parsed.document, &options.mode, &options.selector)?;
    Ok(QueryOutput { values })
}

/// サブコマンド `query` のハンドラ。usage エラーではランタイムも Fetcher も作らない。
fn run(args: CommandArgs) -> Result<CommandOutcome, CommandError> {
    let options = parse_query_args(&args)?;
    let fetcher = Fetcher::new(FetchOptions::default())
        .map_err(|e| QueryError::Navigate(NavigateError::from(e)))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| QueryError::Navigate(NavigateError::Runtime(e)))?;
    let output = runtime.block_on(query_with(&fetcher, &options))?;
    output
        .write_lines(&mut io::stdout().lock())
        .map_err(CommandError::Output)?;
    Ok(CommandOutcome::Completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::{block_on, loopback_fetcher, serve_once};

    fn args(v: &[&str]) -> CommandArgs {
        CommandArgs::new(v.iter().map(|s| s.to_string()).collect())
    }

    fn doc(html: &str) -> Document {
        parse_document(html, &ParseOptions::default())
            .expect("parses")
            .document
    }

    const PAGE: &str = "<html><body><h1>Title</h1><ul><li>1</li><li>2</li></ul>\
                        <a href=\"/next\">n</a><a>bare</a></body></html>";

    #[test]
    fn cli1_query_is_registered() {
        let n = crate::commands::COMMANDS
            .iter()
            .filter(|c| c.name == "query")
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn cli1_query_parses_default_all_and_attr() {
        let o = parse_query_args(&args(&["https://a.example/", "h1"])).expect("ok");
        assert_eq!(o.url, "https://a.example/");
        assert_eq!(o.selector, "h1");
        assert_eq!(o.mode, QueryMode::FirstText);
        let o = parse_query_args(&args(&["--all", "https://a.example/", "li"])).expect("ok");
        assert_eq!(o.mode, QueryMode::AllTexts);
        let o =
            parse_query_args(&args(&["--attr", "href", "https://a.example/", "a"])).expect("ok");
        assert_eq!(o.mode, QueryMode::Attribute("href".into()));
    }

    #[test]
    fn cli1_query_accepts_trailing_options_and_double_dash() {
        let o = parse_query_args(&args(&["https://a.example/", "li", "--all"])).expect("ok");
        assert_eq!(o.mode, QueryMode::AllTexts);
        let o = parse_query_args(&args(&["--", "https://a.example/", "-x"])).expect("ok");
        assert_eq!(o.selector, "-x");
    }

    #[test]
    fn cli1_query_reports_missing_arguments() {
        assert_eq!(
            parse_query_args(&args(&[])).expect_err("url"),
            UsageError::MissingArgument("URL")
        );
        assert_eq!(
            parse_query_args(&args(&["https://a.example/"])).expect_err("selector"),
            UsageError::MissingArgument("SELECTOR")
        );
        assert_eq!(
            parse_query_args(&args(&["--attr"])).expect_err("name"),
            UsageError::MissingArgument("NAME")
        );
    }

    #[test]
    fn cli1_query_rejects_extra_unknown_and_conflicting() {
        assert_eq!(
            parse_query_args(&args(&["u", "s", "extra"])).expect_err("extra"),
            UsageError::UnexpectedArgument("extra".into())
        );
        assert_eq!(
            parse_query_args(&args(&["--bogus", "u", "s"])).expect_err("opt"),
            UsageError::UnknownOption("--bogus".into())
        );
        assert_eq!(
            parse_query_args(&args(&["--attr=href", "u", "s"])).expect_err("eq form"),
            UsageError::UnknownOption("--attr=href".into())
        );
        assert_eq!(
            parse_query_args(&args(&["--all", "--attr", "href", "u", "a"])).expect_err("conflict"),
            UsageError::ConflictingOptions {
                first: "--all",
                second: "--attr"
            }
        );
        assert_eq!(
            parse_query_args(&args(&["--attr", "href", "--all", "u", "a"])).expect_err("conflict"),
            UsageError::ConflictingOptions {
                first: "--attr",
                second: "--all"
            }
        );
        assert_eq!(
            parse_query_args(&args(&["--all", "--all", "u", "s"])).expect_err("dup"),
            UsageError::UnexpectedArgument("--all".into())
        );
    }

    #[test]
    fn cli1_query_rejects_invalid_attr_name() {
        let invalid = UsageError::InvalidOptionValue { option: "--attr" };
        assert_eq!(
            parse_query_args(&args(&["--attr", "", "u", "s"])).expect_err("empty"),
            invalid
        );
        let long = "a".repeat(MAX_ATTR_NAME_LEN + 1);
        assert_eq!(
            parse_query_args(&args(&["--attr", &long, "u", "s"])).expect_err("long"),
            invalid
        );
    }

    #[test]
    fn cli1_query_json_string_escapes() {
        let esc = |s: &str| {
            let mut o = String::new();
            write_json_string(&mut o, s);
            o
        };
        assert_eq!(esc("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(esc("x\ny\tz"), "\"x\\ny\\tz\"");
        assert_eq!(esc("\u{1b}[31m"), "\"\\u001b[31m\"");
        assert_eq!(esc("\u{9b}"), "\"\\u009b\"");
        assert_eq!(esc("\u{2028}"), "\"\\u2028\"");
        assert_eq!(esc("日本語"), "\"日本語\"");
    }

    #[test]
    fn cli1_query_first_text() {
        let v = query_document(&doc(PAGE), &QueryMode::FirstText, "h1").expect("ok");
        assert_eq!(v, vec!["Title".to_string()]);
    }

    #[test]
    fn cli1_query_all_texts_in_document_order() {
        let v = query_document(&doc(PAGE), &QueryMode::AllTexts, "li").expect("ok");
        assert_eq!(v, vec!["1".to_string(), "2".to_string()]);
    }

    #[test]
    fn cli1_query_attribute() {
        let v = query_document(&doc(PAGE), &QueryMode::Attribute("href".into()), "a").expect("ok");
        assert_eq!(v, vec!["/next".to_string()]);
    }

    #[test]
    fn cli1_query_not_found_is_error() {
        let e = query_document(&doc(PAGE), &QueryMode::FirstText, "table").expect_err("none");
        assert!(matches!(e, QueryError::ElementNotFound));
        assert_eq!(e.to_string(), "no element matches the selector");
        let e = query_document(&doc(PAGE), &QueryMode::AllTexts, "table").expect_err("none");
        assert!(matches!(e, QueryError::ElementNotFound));
    }

    #[test]
    fn cli1_query_missing_attribute_is_error() {
        let e = query_document(&doc(PAGE), &QueryMode::Attribute("href".into()), "h1")
            .expect_err("no attr");
        assert!(matches!(e, QueryError::AttributeNotFound));
        assert_eq!(e.to_string(), "attribute not found on the matched element");
    }

    #[test]
    fn cli1_query_invalid_selector_is_error() {
        let e = query_document(&doc(PAGE), &QueryMode::FirstText, "a[").expect_err("bad");
        assert!(matches!(
            e,
            QueryError::Selector(Error::InvalidInput { .. })
        ));
        assert_eq!(e.to_string(), "invalid selector");
        let long = "a".repeat(4097);
        let e = query_document(&doc(PAGE), &QueryMode::FirstText, &long).expect_err("long");
        assert!(matches!(
            e,
            QueryError::Selector(Error::InvalidInput { .. })
        ));
    }

    #[test]
    fn cli1_query_with_fetches_and_queries() {
        let port = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 27\r\nConnection: close\r\n\r\n<html><h1>Hello</h1></html>",
        );
        let o = QueryOptions {
            url: format!("http://127.0.0.1:{port}/"),
            selector: "h1".into(),
            mode: QueryMode::FirstText,
        };
        let out = block_on(query_with(&loopback_fetcher(), &o)).expect("ok");
        assert_eq!(out.values, vec!["Hello".to_string()]);
        let mut buf = Vec::new();
        out.write_lines(&mut buf).expect("write");
        assert_eq!(buf, b"\"Hello\"\n");
    }

    #[test]
    fn cli1_query_with_queries_404_body() {
        let port = serve_once(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 12\r\nConnection: close\r\n\r\n<p>gone</p>\n",
        );
        let o = QueryOptions {
            url: format!("http://127.0.0.1:{port}/x"),
            selector: "p".into(),
            mode: QueryMode::FirstText,
        };
        let out = block_on(query_with(&loopback_fetcher(), &o)).expect("ok");
        assert_eq!(out.values, vec!["gone".to_string()]);
    }

    #[test]
    fn cli1_query_with_rejects_loopback_and_file_by_default() {
        let f = Fetcher::new(FetchOptions::default()).expect("fetcher");
        let mk = |url: &str| QueryOptions {
            url: url.into(),
            selector: "h1".into(),
            mode: QueryMode::FirstText,
        };
        let e = block_on(query_with(&f, &mk("http://127.0.0.1:9/"))).expect_err("ssrf");
        assert!(matches!(
            e,
            QueryError::Navigate(NavigateError::Fetch(Error::DisallowedAddress { .. }))
        ));
        let e = block_on(query_with(&f, &mk("file:///etc/passwd"))).expect_err("scheme");
        assert!(matches!(
            e,
            QueryError::Navigate(NavigateError::Fetch(Error::DisallowedScheme { .. }))
        ));
    }

    #[test]
    fn cli1_query_with_about_blank_has_no_document() {
        let f = Fetcher::new(FetchOptions::default()).expect("fetcher");
        let o = QueryOptions {
            url: "about:blank".into(),
            selector: "h1".into(),
            mode: QueryMode::FirstText,
        };
        let e = block_on(query_with(&f, &o)).expect_err("blank");
        assert!(matches!(e, QueryError::NoDocument));
    }

    #[test]
    fn cli1_query_all_rejects_nested_text_amplification() {
        // 入れ子 div の最内にテキストを置くと、各祖先の text_content が同じ本文を複製する。
        let depth = 200;
        let text = "x".repeat(100 * 1024);
        let html = format!(
            "{}{}{}",
            "<div>".repeat(depth),
            text,
            "</div>".repeat(depth)
        );
        let d = doc(&html);
        let e = query_document(&d, &QueryMode::AllTexts, "div").expect_err("too large");
        assert!(matches!(e, QueryError::ResultTooLarge));
        assert_eq!(e.to_string(), "query result exceeds the size limit");
    }

    #[test]
    fn cli1_query_all_rejects_too_many_results() {
        let html = format!("<body>{}</body>", "<i></i>".repeat(MAX_RESULT_COUNT + 1));
        let d = doc(&html);
        let e = query_document(&d, &QueryMode::AllTexts, "i").expect_err("too many");
        assert!(matches!(e, QueryError::ResultTooLarge));
    }

    #[test]
    fn cli1_query_all_output_is_one_json_line_per_value() {
        let out = QueryOutput {
            values: vec!["1".into(), "2".into()],
        };
        let mut buf = Vec::new();
        out.write_lines(&mut buf).expect("write");
        assert_eq!(buf, b"\"1\"\n\"2\"\n");
    }

    #[test]
    fn cli1_query_not_found_exits_with_failure() {
        let ce = CommandError::from(QueryError::ElementNotFound);
        assert_eq!(
            format!("{:?}", ce.exit_code()),
            format!("{:?}", std::process::ExitCode::FAILURE)
        );
    }
}
