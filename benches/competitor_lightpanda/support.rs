//! `competitor_lightpanda` ベンチ本体（`../competitor_lightpanda.rs`）が使う
//! 純粋関数群。副作用（プロセス起動・ソケット通信）を持たないロジックだけを
//! ここへ切り出し、`[[test]]` ターゲット（`competitor_lightpanda_support`）として
//! 3 OS で unit test する（ci.md「3 OS CI」・coding-rust.md「テスト」）。
//!
//! TASK-84（84.1）・Issue #211。対応ビヘイビア: バイナリサイズ計測が使う
//! `PERF-1`、cold start 計測が使う `PERF-3`、アイドル RSS 計測が使う `PERF-6`、
//! MCP トークン削減率計測が使う `AISNAP-1`。
//!
//! この crate root は `[[test]]` ターゲット（`cargo test` 用の bin-like root）
//! としてもコンパイルされるため、`#[cfg(test)]` 外の関数もすべて `cfg(test)` の
//! テストから到達させる必要がある（到達しない関数・フィールドは dead_code エラーに
//! なる。unused であっても `pub` は免除されない）。

use std::collections::HashMap;

/// 複数回試行した計測値（ミリ秒・キロバイト等）から中央値を求める。
///
/// cold start（`PERF-3`）・アイドル RSS（`PERF-6`）はいずれも複数回試行して
/// 中央値を採用する方針（実装計画 3.2 節）のため、単位に依存しない `f64` で
/// 受け取る。空スライスは中央値を定義できないため `None` を返す。
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let len = sorted.len();
    let mid = len / 2;
    let value = if len.is_multiple_of(2) {
        let lower = *sorted.get(mid - 1)?;
        let upper = *sorted.get(mid)?;
        (lower + upper) / 2.0
    } else {
        *sorted.get(mid)?
    };
    Some(value)
}

/// 基準値（Chromium 実測値等）に対する削減率を百分率で返す。
///
/// `PERF-1`（バイナリサイズ）・`PERF-6`（アイドル RSS）の Chromium 比目標達成可否
/// 判定、`AISNAP-1`（トークン削減率）の算出で共通に使う。`baseline` が 0 以下の
/// ときは分母が意味を持たないため `None` を返す（fail-closed。coding-rust.md
/// 「外部入力の経路では unwrap を使わず明示的に処理する」に準じ、ここでは
/// 環境変数由来の基準値を外部入力として扱う）。
pub fn reduction_pct(baseline: f64, value: f64) -> Option<f64> {
    if baseline <= 0.0 {
        return None;
    }
    Some((1.0 - value / baseline) * 100.0)
}

/// 文字数から近似トークン数を見積もる（`ceil(chars / 4)`）。
///
/// `AISNAP-1` のトークン削減率計測で使う近似式（PoC-1 の `measure-chromium` と
/// 同じ近似。実装計画 3.2 節に明記の通り、cl100k 等の厳密なトークナイザ依存は
/// 導入しないため、この近似値と PoC-13 実測値（gpt-tokenizer 使用）は一致しない）。
pub fn approx_tokens(chars: usize) -> usize {
    chars.div_ceil(4)
}

/// HTTP レスポンスの先頭行（例: `HTTP/1.1 200 OK`）からステータスコードを取り出す。
///
/// cold start 計測（`PERF-3`）の readiness probe が `TcpStream` で受け取った
/// レスポンスの先頭行を渡す想定。書式に合わない行・範囲外の数値は `None` にし、
/// 呼び出し側のポーリングを継続させる（panic させない。coding-rust.md）。
pub fn parse_http_status(line: &str) -> Option<u16> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let mut parts = trimmed.split_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    let code = parts.next()?;
    code.parse::<u16>().ok()
}

/// unix `ps -o rss= -p <pid>` の標準出力（キロバイト単位の数値のみ・前後に空白を
/// 含み得る）を解釈する。
///
/// アイドル RSS 計測（`PERF-6`）が unix でのみ呼ぶ（Windows は
/// `Unsupported`。実装計画 3.2 節）。空文字列・数値でない出力は `None` を返す。
///
/// 呼び出し元（`competitor_lightpanda.rs` の `sample_rss_kb`）が
/// `#[cfg(unix)]` 限定のため、この関数自体も `#[cfg(unix)]` にする。
/// 無条件公開のままだと Windows ネイティブビルドで到達不能になり
/// `dead_code` 警告が `-D warnings`（ci.md「3 OS CI」）で fail する。
#[cfg(unix)]
pub fn parse_ps_rss_kb(out: &str) -> Option<u64> {
    out.trim().parse::<u64>().ok()
}

/// テンプレート引数中の `{port}` プレースホルダを実ポート番号へ展開する。
///
/// `Target::serve_args`（実装計画 3.1 節）に対して起動直前に呼ぶ。プレースホルダを
/// 含まない要素はそのまま返す（トークン置換のみ・シェル展開はしない。
/// security.md「インジェクション」対策としてシェルを経由しないコマンド起動と組む）。
pub fn expand_args(template: &[String], port: u16) -> Vec<String> {
    template
        .iter()
        .map(|arg| arg.replace("{port}", &port.to_string()))
        .collect()
}

/// 環境変数の値（空白区切り）を引数ベクタへ分割する。
///
/// `FANDHE_BROWSER_SERVE_ARGS` 等の環境変数から起動引数を受け取る経路が使う。
/// クォート解釈はせず、素朴な空白分割に留める（シェル評価をしないことで
/// インジェクションを避ける方針。実装計画セクション 6）。
pub fn split_args(env: &str) -> Vec<String> {
    env.split_whitespace().map(str::to_string).collect()
}

/// 出力 JSON へ埋め込む文字列をエスケープする。
///
/// 手書き JSON シリアライズ（実装計画 3.3 節。新規依存を避けるため `serde_json`
/// は使わない）で使う。制御文字は `\uXXXX` 形式にする。
pub fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    for c in input.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 手書き最小 JSON パーサーが返す値。
///
/// MCP（`mcp` サブコマンド）が stdio 経由で返す NDJSON 応答行から、呼び出し
/// `id` の照合と `result.content[].text` の抽出だけを行う用途に限定した最小限の
/// 値表現（`AISNAP-1`）。汎用 JSON ライブラリ（`serde_json`）は新規依存になる
/// ため使わない（dependency-policy.md）。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(HashMap<String, JsonValue>),
}

impl JsonValue {
    /// `Object` のときだけ指定キーの値を返す。MCP 応答の `id`・`result` 等の
    /// フィールド取り出しに使う（外部入力なので `get` 相当の非 panic API のみ提供）。
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(map) => map.get(key),
            _ => None,
        }
    }

    /// `Array` のときだけ指定 index の値を返す。
    pub fn index(&self, i: usize) -> Option<&JsonValue> {
        match self {
            JsonValue::Array(items) => items.get(i),
            _ => None,
        }
    }

    /// `String` のときだけ中身を借用する。
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// `Number` のときだけ `f64` を返す。MCP 応答の `id`（数値）照合に使う。
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonValue::Number(n) => Some(*n),
            _ => None,
        }
    }
}

/// JSON パース失敗の理由。呼び出し元（MCP 応答の逐次パース）は該当行を
/// `error` 扱いにして次のサイトの計測を継続する（実装計画 4 節）。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonParseError {
    UnexpectedEnd,
    UnexpectedChar(char),
    InvalidNumber,
    InvalidEscape,
    InvalidSurrogate,
    DepthExceeded,
    TrailingData,
}

/// 再帰下降の深さ上限。MCP 応答は数階層のオブジェクト・配列のネストに留まる
/// 想定だが、外部プロセスからの入力を無制限に信用しないため上限を設ける
/// （coding-rust.md「長さ・件数を上限検証してからアロケーションに使う」）。
const MAX_JSON_DEPTH: usize = 64;

/// 最小 JSON パーサーのエントリポイント。末尾に空白以外のデータが残っていたら
/// `TrailingData` にする（NDJSON の 1 行 = 1 JSON 値という前提を守るため）。
pub fn parse_json(input: &str) -> Result<JsonValue, JsonParseError> {
    let chars: Vec<char> = input.chars().collect();
    let mut pos = 0usize;
    let value = parse_value(&chars, &mut pos, 0)?;
    skip_whitespace(&chars, &mut pos);
    if pos != chars.len() {
        return Err(JsonParseError::TrailingData);
    }
    Ok(value)
}

fn skip_whitespace(chars: &[char], pos: &mut usize) {
    while let Some(&c) = chars.get(*pos) {
        if c.is_whitespace() {
            *pos += 1;
        } else {
            break;
        }
    }
}

fn peek(chars: &[char], pos: usize) -> Option<char> {
    chars.get(pos).copied()
}

fn parse_value(chars: &[char], pos: &mut usize, depth: usize) -> Result<JsonValue, JsonParseError> {
    if depth > MAX_JSON_DEPTH {
        return Err(JsonParseError::DepthExceeded);
    }
    skip_whitespace(chars, pos);
    match peek(chars, *pos) {
        None => Err(JsonParseError::UnexpectedEnd),
        Some('"') => parse_string(chars, pos).map(JsonValue::String),
        Some('{') => parse_object(chars, pos, depth),
        Some('[') => parse_array(chars, pos, depth),
        Some('t') => parse_literal(chars, pos, "true", JsonValue::Bool(true)),
        Some('f') => parse_literal(chars, pos, "false", JsonValue::Bool(false)),
        Some('n') => parse_literal(chars, pos, "null", JsonValue::Null),
        Some(c) if c == '-' || c.is_ascii_digit() => parse_number(chars, pos),
        Some(c) => Err(JsonParseError::UnexpectedChar(c)),
    }
}

fn parse_literal(
    chars: &[char],
    pos: &mut usize,
    literal: &str,
    value: JsonValue,
) -> Result<JsonValue, JsonParseError> {
    for expected in literal.chars() {
        match peek(chars, *pos) {
            Some(c) if c == expected => *pos += 1,
            Some(c) => return Err(JsonParseError::UnexpectedChar(c)),
            None => return Err(JsonParseError::UnexpectedEnd),
        }
    }
    Ok(value)
}

fn parse_number(chars: &[char], pos: &mut usize) -> Result<JsonValue, JsonParseError> {
    let start = *pos;
    if peek(chars, *pos) == Some('-') {
        *pos += 1;
    }
    let mut saw_digit = false;
    while let Some(c) = peek(chars, *pos) {
        if c.is_ascii_digit() {
            *pos += 1;
            saw_digit = true;
        } else {
            break;
        }
    }
    if !saw_digit {
        return Err(JsonParseError::InvalidNumber);
    }
    if peek(chars, *pos) == Some('.') {
        *pos += 1;
        let mut saw_frac_digit = false;
        while let Some(c) = peek(chars, *pos) {
            if c.is_ascii_digit() {
                *pos += 1;
                saw_frac_digit = true;
            } else {
                break;
            }
        }
        if !saw_frac_digit {
            return Err(JsonParseError::InvalidNumber);
        }
    }
    if matches!(peek(chars, *pos), Some('e') | Some('E')) {
        *pos += 1;
        if matches!(peek(chars, *pos), Some('+') | Some('-')) {
            *pos += 1;
        }
        let mut saw_exp_digit = false;
        while let Some(c) = peek(chars, *pos) {
            if c.is_ascii_digit() {
                *pos += 1;
                saw_exp_digit = true;
            } else {
                break;
            }
        }
        if !saw_exp_digit {
            return Err(JsonParseError::InvalidNumber);
        }
    }
    let text: String = chars
        .get(start..*pos)
        .ok_or(JsonParseError::InvalidNumber)?
        .iter()
        .collect();
    text.parse::<f64>()
        .map(JsonValue::Number)
        .map_err(|_| JsonParseError::InvalidNumber)
}

fn parse_string(chars: &[char], pos: &mut usize) -> Result<String, JsonParseError> {
    // 呼び出し元で `"` を確認済みの前提（`parse_value` の分岐から呼ばれる）。
    *pos += 1;
    let mut out = String::new();
    loop {
        match peek(chars, *pos) {
            None => return Err(JsonParseError::UnexpectedEnd),
            Some('"') => {
                *pos += 1;
                return Ok(out);
            }
            Some('\\') => {
                *pos += 1;
                let escaped = peek(chars, *pos).ok_or(JsonParseError::UnexpectedEnd)?;
                match escaped {
                    '"' => {
                        out.push('"');
                        *pos += 1;
                    }
                    '\\' => {
                        out.push('\\');
                        *pos += 1;
                    }
                    '/' => {
                        out.push('/');
                        *pos += 1;
                    }
                    'b' => {
                        out.push('\u{0008}');
                        *pos += 1;
                    }
                    'f' => {
                        out.push('\u{000C}');
                        *pos += 1;
                    }
                    'n' => {
                        out.push('\n');
                        *pos += 1;
                    }
                    'r' => {
                        out.push('\r');
                        *pos += 1;
                    }
                    't' => {
                        out.push('\t');
                        *pos += 1;
                    }
                    'u' => {
                        *pos += 1;
                        let high = parse_hex4(chars, pos)?;
                        if (0xD800..=0xDBFF).contains(&high) {
                            // サロゲートペアの上位。直後に下位サロゲート
                            // （`\uDC00`〜`\uDFFF`）が続く前提で結合する。
                            if peek(chars, *pos) != Some('\\') || peek(chars, *pos + 1) != Some('u')
                            {
                                return Err(JsonParseError::InvalidSurrogate);
                            }
                            *pos += 2;
                            let low = parse_hex4(chars, pos)?;
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                return Err(JsonParseError::InvalidSurrogate);
                            }
                            let combined = 0x10000
                                + (u32::from(high) - 0xD800) * 0x400
                                + (u32::from(low) - 0xDC00);
                            let c =
                                char::from_u32(combined).ok_or(JsonParseError::InvalidSurrogate)?;
                            out.push(c);
                        } else if (0xDC00..=0xDFFF).contains(&high) {
                            // 単独の下位サロゲートは不正な値として扱う。
                            return Err(JsonParseError::InvalidSurrogate);
                        } else {
                            let c = char::from_u32(u32::from(high))
                                .ok_or(JsonParseError::InvalidSurrogate)?;
                            out.push(c);
                        }
                    }
                    other => return Err(JsonParseError::UnexpectedChar(other)),
                }
            }
            // レビュー指摘 P2（PR #442）: 未エスケープの U+0000〜U+001F 制御
            // 文字（RFC 8259 の JSON 文法で文字列内に生で現れることを許さない
            // 範囲）を通常文字として受理していた。MCP 応答を untrusted な
            // 外部入力として扱う方針（coding-rust.md）に従い、ここで
            // `JsonParseError` として拒否する。
            Some(c) if (c as u32) < 0x20 => return Err(JsonParseError::UnexpectedChar(c)),
            Some(c) => {
                out.push(c);
                *pos += 1;
            }
        }
    }
}

fn parse_hex4(chars: &[char], pos: &mut usize) -> Result<u16, JsonParseError> {
    let mut value: u16 = 0;
    for _ in 0..4 {
        let c = peek(chars, *pos).ok_or(JsonParseError::UnexpectedEnd)?;
        let digit = c.to_digit(16).ok_or(JsonParseError::InvalidEscape)?;
        value = value
            .checked_mul(16)
            .and_then(|v| v.checked_add(digit as u16))
            .ok_or(JsonParseError::InvalidEscape)?;
        *pos += 1;
    }
    Ok(value)
}

fn parse_array(chars: &[char], pos: &mut usize, depth: usize) -> Result<JsonValue, JsonParseError> {
    // 呼び出し元で `[` を確認済み。
    *pos += 1;
    let mut items = Vec::new();
    skip_whitespace(chars, pos);
    if peek(chars, *pos) == Some(']') {
        *pos += 1;
        return Ok(JsonValue::Array(items));
    }
    loop {
        let value = parse_value(chars, pos, depth + 1)?;
        items.push(value);
        skip_whitespace(chars, pos);
        match peek(chars, *pos) {
            Some(',') => {
                *pos += 1;
            }
            Some(']') => {
                *pos += 1;
                return Ok(JsonValue::Array(items));
            }
            Some(c) => return Err(JsonParseError::UnexpectedChar(c)),
            None => return Err(JsonParseError::UnexpectedEnd),
        }
    }
}

fn parse_object(
    chars: &[char],
    pos: &mut usize,
    depth: usize,
) -> Result<JsonValue, JsonParseError> {
    // 呼び出し元で `{` を確認済み。
    *pos += 1;
    let mut map = HashMap::new();
    skip_whitespace(chars, pos);
    if peek(chars, *pos) == Some('}') {
        *pos += 1;
        return Ok(JsonValue::Object(map));
    }
    loop {
        skip_whitespace(chars, pos);
        if peek(chars, *pos) != Some('"') {
            return Err(JsonParseError::UnexpectedChar(
                peek(chars, *pos).unwrap_or('\0'),
            ));
        }
        let key = parse_string(chars, pos)?;
        skip_whitespace(chars, pos);
        match peek(chars, *pos) {
            Some(':') => *pos += 1,
            Some(c) => return Err(JsonParseError::UnexpectedChar(c)),
            None => return Err(JsonParseError::UnexpectedEnd),
        }
        let value = parse_value(chars, pos, depth + 1)?;
        map.insert(key, value);
        skip_whitespace(chars, pos);
        match peek(chars, *pos) {
            Some(',') => {
                *pos += 1;
            }
            Some('}') => {
                *pos += 1;
                return Ok(JsonValue::Object(map));
            }
            Some(c) => return Err(JsonParseError::UnexpectedChar(c)),
            None => return Err(JsonParseError::UnexpectedEnd),
        }
    }
}

#[cfg(test)]
mod tests {
    // Cargo は `[[bench]]` ターゲット（本ファイルを `#[path]` で取り込む
    // `competitor_lightpanda.rs`）にも常に `--cfg test`（`--test` フラグ自体は
    // 付けない）を渡すため、この `mod tests` はベンチ側のビルドでも解析対象になる。
    // その場合 `--test` 抜きでは組み込みの `#[test]` 属性がアイテムを剥ぎ取り
    // 本体が空になるため、この glob import は「未使用」と判定される
    // （`[[test]]` ターゲット（`cargo test`）としての単独ビルドでは実際に使われる）。
    #[allow(unused_imports)]
    use super::*;

    // PERF-3・PERF-6: median は cold start / RSS の複数試行から代表値を出す。
    #[test]
    fn median_even_count_averages_middle_two() {
        assert_eq!(median(&[1.0, 3.0, 5.0, 7.0]), Some(4.0));
    }

    #[test]
    fn median_odd_count_returns_middle() {
        assert_eq!(median(&[5.0, 1.0, 3.0]), Some(3.0));
    }

    #[test]
    fn median_empty_is_none() {
        assert_eq!(median(&[]), None);
    }

    // PERF-1・PERF-6: reduction_pct は Chromium 比の削減率算出に使う。
    #[test]
    fn reduction_pct_basic() {
        assert_eq!(reduction_pct(100.0, 20.0), Some(80.0));
    }

    #[test]
    fn reduction_pct_zero_baseline_is_none() {
        assert_eq!(reduction_pct(0.0, 20.0), None);
    }

    #[test]
    fn reduction_pct_negative_baseline_is_none() {
        assert_eq!(reduction_pct(-1.0, 20.0), None);
    }

    // AISNAP-1: approx_tokens はトークン削減率計測の近似式。
    #[test]
    fn approx_tokens_rounds_up() {
        assert_eq!(approx_tokens(0), 0);
        assert_eq!(approx_tokens(1), 1);
        assert_eq!(approx_tokens(4), 1);
        assert_eq!(approx_tokens(5), 2);
        assert_eq!(approx_tokens(8), 2);
    }

    // PERF-3: parse_http_status は cold start の readiness probe が使う。
    #[test]
    fn parse_http_status_ok() {
        assert_eq!(parse_http_status("HTTP/1.1 200 OK\r\n"), Some(200));
    }

    #[test]
    fn parse_http_status_malformed_is_none() {
        assert_eq!(parse_http_status("not an http status"), None);
        assert_eq!(parse_http_status(""), None);
        assert_eq!(parse_http_status("HTTP/1.1"), None);
        assert_eq!(parse_http_status("HTTP/1.1 abc"), None);
    }

    // PERF-6: parse_ps_rss_kb は unix の `ps -o rss=` 出力を解釈する
    // （関数定義が `#[cfg(unix)]` のため、テストも合わせて限定する）。
    #[cfg(unix)]
    #[test]
    fn parse_ps_rss_kb_basic() {
        assert_eq!(parse_ps_rss_kb("  12345\n"), Some(12345));
    }

    #[cfg(unix)]
    #[test]
    fn parse_ps_rss_kb_invalid_is_none() {
        assert_eq!(parse_ps_rss_kb(""), None);
        assert_eq!(parse_ps_rss_kb("not a number"), None);
    }

    #[test]
    fn expand_args_replaces_port_placeholder() {
        let template = vec![
            "serve".to_string(),
            "--port".to_string(),
            "{port}".to_string(),
        ];
        assert_eq!(
            expand_args(&template, 9400),
            vec![
                "serve".to_string(),
                "--port".to_string(),
                "9400".to_string()
            ]
        );
    }

    #[test]
    fn expand_args_no_placeholder_is_unchanged() {
        let template = vec!["mcp".to_string()];
        assert_eq!(expand_args(&template, 9400), vec!["mcp".to_string()]);
    }

    #[test]
    fn split_args_splits_on_whitespace() {
        assert_eq!(
            split_args("serve  --port {port}"),
            vec![
                "serve".to_string(),
                "--port".to_string(),
                "{port}".to_string()
            ]
        );
    }

    #[test]
    fn split_args_empty_is_empty() {
        assert_eq!(split_args(""), Vec::<String>::new());
    }

    #[test]
    fn json_escape_basic() {
        assert_eq!(json_escape("plain"), "plain");
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("line1\nline2"), "line1\\nline2");
    }

    #[test]
    fn json_escape_control_char() {
        assert_eq!(json_escape("\u{0001}"), "\\u0001");
    }

    // AISNAP-1: parse_json は MCP 応答（id 照合・content[].text 抽出）に使う。
    #[test]
    fn parse_json_object_and_get() {
        let value = parse_json(r#"{"id": 1, "result": {"ok": true}}"#).expect("valid json");
        assert_eq!(value.get("id").and_then(JsonValue::as_f64), Some(1.0));
        assert_eq!(
            value.get("result").and_then(|r| r.get("ok")),
            Some(&JsonValue::Bool(true))
        );
    }

    #[test]
    fn parse_json_array_and_index() {
        let value = parse_json(r#"[1, "two", null]"#).expect("valid json");
        assert_eq!(value.index(0).and_then(JsonValue::as_f64), Some(1.0));
        assert_eq!(value.index(1).and_then(JsonValue::as_str), Some("two"));
        assert_eq!(value.index(2), Some(&JsonValue::Null));
        assert_eq!(value.index(3), None);
    }

    #[test]
    fn parse_json_string_with_escapes_and_surrogate_pair() {
        // U+1F600 (😀) は UTF-16 サロゲートペア 😀 で表現される。
        let value = parse_json(r#""a\n\t\"😀""#).expect("valid json");
        assert_eq!(value.as_str(), Some("a\n\t\"\u{1F600}"));
    }

    #[test]
    fn parse_json_lone_surrogate_is_error() {
        let err = parse_json(r#""\uD83D""#).unwrap_err();
        assert_eq!(err, JsonParseError::InvalidSurrogate);
    }

    // レビュー指摘 P2（PR #442）: 未エスケープの制御文字（U+0000〜U+001F）を
    // 含む文字列は不正な JSON として拒否する（RFC 8259）。
    #[test]
    fn parse_json_unescaped_control_char_is_error() {
        let raw = "\"a\u{0001}b\"";
        let err = parse_json(raw).unwrap_err();
        assert_eq!(err, JsonParseError::UnexpectedChar('\u{0001}'));
    }

    #[test]
    fn parse_json_unescaped_newline_in_string_is_error() {
        let raw = "\"a\nb\"";
        let err = parse_json(raw).unwrap_err();
        assert_eq!(err, JsonParseError::UnexpectedChar('\n'));
    }

    #[test]
    fn parse_json_content_text_extraction_shape() {
        // MCP `tools/call` 応答の実際の形（実装計画 3.1 節・4 節）を模す。
        let raw = r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"<html></html>"}]}}"#;
        let value = parse_json(raw).expect("valid json");
        let text = value
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.index(0))
            .and_then(|item| item.get("text"))
            .and_then(JsonValue::as_str);
        assert_eq!(text, Some("<html></html>"));
    }

    #[test]
    fn parse_json_empty_input_is_error() {
        assert_eq!(parse_json(""), Err(JsonParseError::UnexpectedEnd));
    }

    #[test]
    fn parse_json_malformed_is_error() {
        assert!(parse_json("{not json}").is_err());
        assert!(parse_json("[1, 2,]").is_err());
        assert!(parse_json("tru").is_err());
    }

    #[test]
    fn parse_json_trailing_data_is_error() {
        assert_eq!(parse_json("1 2"), Err(JsonParseError::TrailingData));
    }

    #[test]
    fn parse_json_depth_exceeded_is_error() {
        let deep = "[".repeat(MAX_JSON_DEPTH + 2) + &"]".repeat(MAX_JSON_DEPTH + 2);
        assert_eq!(parse_json(&deep), Err(JsonParseError::DepthExceeded));
    }

    #[test]
    fn parse_json_numbers() {
        assert_eq!(parse_json("0"), Ok(JsonValue::Number(0.0)));
        assert_eq!(parse_json("-1.5"), Ok(JsonValue::Number(-1.5)));
        assert_eq!(parse_json("1e3"), Ok(JsonValue::Number(1000.0)));
        assert!(parse_json("-").is_err());
        assert!(parse_json("1.").is_err());
    }
}
