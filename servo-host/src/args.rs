//! Servo ホストの起動引数の検査（Servo 非依存の純関数）。
//!
//! 役割: `main` が受け取る argv を固定形式の許可リストで検査し、検証済みの [`Command`] へ写す
//! （SEC-2・RENDER-3・TASK-38・#1435。設計 §2.6）。URL や任意文字列は受け取らず、受理するのは
//! 1〜65535 の 10 進ポートと、正規化済みの絶対 UDS パスだけ。値はエラーメッセージに含めない
//! （ログ注入・機密露出の回避）。
//!
//! 呼び出し元は `main`。`--egress-proxy-port`（macOS 向け）・`--egress-relay-uds`（Linux 向け）は
//! 本体の egress 隔離（#1436 が付与）が渡す。proxy の場所は `engine::LoopbackProxy` として
//! Servo の proxy pref にだけ流れる。

use std::fmt;
use std::num::NonZeroU16;
#[cfg(unix)]
use std::path::{Component, Path, PathBuf};

/// UDS パスの最大バイト長。`sun_path`（Linux 108・macOS 104、いずれも NUL 込み）のどちらにも
/// 収まる値で、`libc` に依存せず固定する（`harness/egress-isolation` の `SUN_PATH_MAX` と揃える）。
#[cfg(unix)]
pub(crate) const UDS_PATH_MAX_BYTES: usize = 100;

/// 検証済みの起動コマンド。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    /// `--version`
    Version,
    /// `--selftest`
    Selftest,
    /// `--stdio`（egress の指定つき）
    Stdio(Egress),
}

/// Servo の通信の出口の指定。排他で、どれも検証済みの値だけを持つ。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Egress {
    /// proxy なし（従来どおり）。
    None,
    /// `127.0.0.1:<port>` の CONNECT プロキシへ向ける（macOS 向け）。
    ProxyPort(NonZeroU16),
    /// netns 内の中継を起動し、各接続を `<path>` の UDS へ転送する（Linux 向け）。
    #[cfg(unix)]
    RelayUds(PathBuf),
}

/// 引数検査の失敗。`Display` は英語で、入力値を含めない。
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum ArgError {
    /// 未知・位置違い・余分な引数。
    Usage,
    /// オプションの値がない。
    MissingValue(&'static str),
    /// ポートが 1〜65535 の 10 進整数ではない。
    InvalidPort,
    /// UDS パスが絶対・正規化済み・長さ上限内ではない。
    InvalidRelayPath(&'static str),
    /// `--egress-proxy-port` と `--egress-relay-uds` の同時指定。
    ConflictingEgress,
    /// 同じオプションの重複。
    DuplicateOption,
    /// このプラットフォームでは中継を使えない。
    #[cfg(not(unix))]
    RelayUnsupported,
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => write!(
                f,
                "fandhe-browser-servo-host supports only --version, --selftest and --stdio \
                 (optionally with --egress-proxy-port <port> or --egress-relay-uds <path>)"
            ),
            Self::MissingValue(opt) => write!(f, "option {opt} requires a value"),
            Self::InvalidPort => write!(f, "egress proxy port must be an integer in 1..=65535"),
            Self::InvalidRelayPath(reason) => write!(f, "invalid relay socket path: {reason}"),
            Self::ConflictingEgress => write!(
                f,
                "--egress-proxy-port and --egress-relay-uds cannot be combined"
            ),
            Self::DuplicateOption => write!(f, "duplicate egress option"),
            #[cfg(not(unix))]
            Self::RelayUnsupported => {
                write!(f, "--egress-relay-uds is not supported on this platform")
            }
        }
    }
}

impl std::error::Error for ArgError {}

const OPT_PORT: &str = "--egress-proxy-port";
const OPT_UDS: &str = "--egress-relay-uds";

/// argv（プログラム名を除く）を検査して [`Command`] にする。
///
/// `--version`・`--selftest` は単独のみ。`--stdio` は先頭必須で、後続は egress オプション
/// （名前と値の別引数）を 0 または 1 組だけ受け付ける。`--opt=value` 形式は受け付けない。
pub(crate) fn parse(args: &[String]) -> Result<Command, ArgError> {
    match args {
        [a] if a == "--version" => Ok(Command::Version),
        [a] if a == "--selftest" => Ok(Command::Selftest),
        [a, rest @ ..] if a == "--stdio" => parse_egress(rest).map(Command::Stdio),
        _ => Err(ArgError::Usage),
    }
}

fn parse_egress(rest: &[String]) -> Result<Egress, ArgError> {
    let Some((name, tail)) = rest.split_first() else {
        return Ok(Egress::None);
    };
    let opt: &'static str = match name.as_str() {
        OPT_PORT => OPT_PORT,
        OPT_UDS => OPT_UDS,
        _ => return Err(ArgError::Usage),
    };
    let Some((value, extra)) = tail.split_first() else {
        return Err(ArgError::MissingValue(opt));
    };
    if let Some(next) = extra.first() {
        return Err(if next == OPT_PORT || next == OPT_UDS {
            if next == name {
                ArgError::DuplicateOption
            } else {
                ArgError::ConflictingEgress
            }
        } else {
            ArgError::Usage
        });
    }
    if opt == OPT_PORT {
        parse_port(value).map(Egress::ProxyPort)
    } else {
        parse_relay(value)
    }
}

/// 正規形の 10 進ポート（先頭 0・符号・空白なし）だけを受理する。
fn parse_port(value: &str) -> Result<NonZeroU16, ArgError> {
    let canonical = !value.is_empty()
        && value.len() <= 5
        && value.bytes().all(|b| b.is_ascii_digit())
        && !value.starts_with('0');
    if !canonical {
        return Err(ArgError::InvalidPort);
    }
    value
        .parse::<u16>()
        .ok()
        .and_then(NonZeroU16::new)
        .ok_or(ArgError::InvalidPort)
}

#[cfg(unix)]
fn parse_relay(value: &str) -> Result<Egress, ArgError> {
    if value.contains('\0') {
        return Err(ArgError::InvalidRelayPath("contains NUL"));
    }
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err(ArgError::InvalidRelayPath("not absolute"));
    }
    // `components()` は途中の `.` と連続する `/` を黙って畳むため、文字列の区切りでも検査する。
    let clean_segments = value.strip_prefix('/').is_some_and(|rest| {
        rest.split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
    });
    if !clean_segments
        || !path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Err(ArgError::InvalidRelayPath(
            "contains empty, . or .. segment",
        ));
    }
    if value.len() > UDS_PATH_MAX_BYTES {
        return Err(ArgError::InvalidRelayPath("too long"));
    }
    Ok(Egress::RelayUds(path.to_path_buf()))
}

#[cfg(not(unix))]
fn parse_relay(_value: &str) -> Result<Egress, ArgError> {
    Err(ArgError::RelayUnsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Command, ArgError> {
        let v: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        parse(&v)
    }

    fn port(n: u16) -> Egress {
        Egress::ProxyPort(NonZeroU16::new(n).unwrap())
    }

    /// SEC-2・RENDER-3（#1435）: 正常系の受理。
    #[test]
    fn accepts_valid_forms() {
        assert_eq!(p(&["--version"]), Ok(Command::Version));
        assert_eq!(p(&["--selftest"]), Ok(Command::Selftest));
        assert_eq!(p(&["--stdio"]), Ok(Command::Stdio(Egress::None)));
        assert_eq!(
            p(&["--stdio", "--egress-proxy-port", "8080"]),
            Ok(Command::Stdio(port(8080)))
        );
        assert_eq!(
            p(&["--stdio", "--egress-proxy-port", "65535"]),
            Ok(Command::Stdio(port(65535)))
        );
    }

    #[cfg(unix)]
    #[test]
    fn accepts_relay_uds() {
        assert_eq!(
            p(&["--stdio", "--egress-relay-uds", "/run/x/proxy.sock"]),
            Ok(Command::Stdio(Egress::RelayUds(PathBuf::from(
                "/run/x/proxy.sock"
            ))))
        );
        let exact = format!("/{}", "a".repeat(UDS_PATH_MAX_BYTES - 1));
        assert!(p(&["--stdio", "--egress-relay-uds", &exact]).is_ok());
    }

    #[test]
    fn rejects_conflict_duplicate_unknown_missing() {
        assert_eq!(
            p(&[
                "--stdio",
                "--egress-proxy-port",
                "80",
                "--egress-relay-uds",
                "/a"
            ]),
            Err(ArgError::ConflictingEgress)
        );
        assert_eq!(
            p(&[
                "--stdio",
                "--egress-relay-uds",
                "/a",
                "--egress-proxy-port",
                "80"
            ]),
            Err(ArgError::ConflictingEgress)
        );
        assert_eq!(
            p(&[
                "--stdio",
                "--egress-proxy-port",
                "80",
                "--egress-proxy-port",
                "81"
            ]),
            Err(ArgError::DuplicateOption)
        );
        for args in [
            &["--stdio", "--foo"][..],
            &["--stdio", "extra"],
            &["--stdio", "--egress-proxy-port=80"],
            &["--egress-proxy-port", "80", "--stdio"],
            &["--selftest", "--egress-proxy-port", "80"],
            &["--version", "--stdio"],
            &["--stdio", "--egress-proxy-port", "80", "extra"],
            &[],
        ] {
            assert_eq!(p(args), Err(ArgError::Usage), "args: {args:?}");
        }
        assert_eq!(
            p(&["--stdio", "--egress-proxy-port"]),
            Err(ArgError::MissingValue("--egress-proxy-port"))
        );
        assert_eq!(
            p(&["--stdio", "--egress-relay-uds"]),
            Err(ArgError::MissingValue("--egress-relay-uds"))
        );
    }

    #[test]
    fn rejects_bad_ports() {
        for v in [
            "",
            "abc",
            "+80",
            "-1",
            "080",
            " 80",
            "8_0",
            "0",
            "65536",
            "99999999999",
            "٣",
        ] {
            assert_eq!(
                p(&["--stdio", "--egress-proxy-port", v]),
                Err(ArgError::InvalidPort),
                "value: {v:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_bad_relay_paths() {
        let long = format!("/{}", "a".repeat(UDS_PATH_MAX_BYTES));
        for v in [
            "proxy.sock",
            "./p.sock",
            "/a/../b.sock",
            "/a/./b.sock",
            "/a/b\0c",
            "/a//b.sock",
            "/a/b/",
            "",
            long.as_str(),
        ] {
            assert!(
                matches!(
                    p(&["--stdio", "--egress-relay-uds", v]),
                    Err(ArgError::InvalidRelayPath(_))
                ),
                "value: {v:?}"
            );
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn relay_is_unsupported_off_unix() {
        assert_eq!(
            p(&["--stdio", "--egress-relay-uds", "C:\\x"]),
            Err(ArgError::RelayUnsupported)
        );
    }

    #[test]
    fn usage_message_keeps_legacy_phrase_and_omits_values() {
        assert!(
            ArgError::Usage
                .to_string()
                .contains("supports only --version, --selftest and --stdio")
        );
        assert!(!ArgError::InvalidPort.to_string().contains("99999"));
    }
}
