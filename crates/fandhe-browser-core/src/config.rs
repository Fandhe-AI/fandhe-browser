//! config: `fandhe-browser.toml`（TOML）を読み込み、プロファイル保存先・
//! 分離強度などブラウザ全体の設定を解釈するモジュール（TASK-91（91.1）・
//! Issue #214。対象ビヘイビアなし・基盤タスク。MS-3）。
//!
//! # 呼び出し文脈
//!
//! `fandhe-browser-cli`（TASK-41.5・未作成）が起動時に [`Config::load`] で
//! 設定ファイルを読み込み、得られた [`ProfileConfig`] を
//! `fandhe-browser-profile::Profile::open`（TASK-50・#177）へ渡す想定
//! （設計時点の申し送り。cli crate 側の配線は別 Issue）。
//!
//! # スコープ（91.1 の範囲）
//!
//! 本 Issue（#214）で読めるのは `[profile]` セクション（保存先 `root`・分離
//! 強度 `isolation`）のみ。兄弟 Issue が以下を追加する契約とする
//! （REPAIR-3: 実装済みを装わない）。
//!
//! - `[js] engine`（JS エンジン選択・fail-closed エラー。TASK-91（91.2）・
//!   Issue #215）
//! - `[rendering]`（レンダリング層有効化・`fandhe-browser.toml` サンプル
//!   ファイル。TASK-91（91.3）・Issue #216）
//!
//! トップレベル・`[profile]` とも未知キーを `deny_unknown_fields` で拒否する
//! （タイプミスを黙って無視しない fail-closed 方針。security.md「不安全な
//! 設計」）。そのため #215・#216 がマージされるまで `[js]`・`[rendering]` を
//! 含む TOML は構文エラー（[`ConfigError::Syntax`]）になる。各兄弟 Issue は
//! 自セクションのフィールドを非公開の `RawConfig` へ追加する責務を持つ。
//!
//! # serde/toml を公開 API に漏らさない方針
//!
//! `error.rs` が `reqwest`/`html5ever` の具象型を公開 API に漏らさない方針と
//! 同様に、本モジュールの公開型（[`Config`]・[`ProfileConfig`]・
//! [`IsolationStrength`]・[`ConfigError`]）は serde/toml の derive・型を持たない。
//! `#[derive(serde::Deserialize)]` を持つのは非公開の `RawConfig`/`RawProfile`
//! のみで、[`Config::from_toml_str`] 内で検証しながら公開型へ変換する
//! （coding-rust.md「JS エンジンはトレイト抽象越しに」と同じ考え方を
//! 外部クレートの derive 型にも適用する）。
//!
//! # パス解決
//!
//! [`ProfileConfig::root`] は `PathBuf` で保持し、文字列連結ではなく
//! `Path::join` のみで組み立てる（coding-rust.md クロスプラットフォーム）。
//! [`Config::load`] は `root` が相対パスの場合、設定ファイルの**親ディレクトリ**
//! を基準に解決する（CWD 依存を避けるため）。`~` 展開・環境変数展開は行わない。
//! パストラバーサル・symlink 検証はここでは行わず、
//! `fandhe-browser-profile::Profile::open`（TASK-50・#177 のハンドル基準検証）
//! に委ねる（二重実装しない）。
//!
//! # リソース上限
//!
//! 外部入力（設定ファイル）を無制限にアロケーションへ使わないよう、
//! [`MAX_CONFIG_BYTES`] で読み込みサイズを事前検証する（security.md
//! 「不安全な設計」: 巨大ファイルによる DoS を防ぐ）。

use crate::error::{Error, Result};
use serde::Deserialize;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// 設定ファイルとして読み込む最大バイト数（1 MiB）。
///
/// 設定ファイルは通常数十〜数百行程度であり、この上限を超える入力は
/// 誤配置・悪意ある入力のいずれかとみなし、アロケーション前に拒否する
/// （security.md「無制限リソース確保による DoS を防ぐ」）。
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

/// `fandhe-browser.toml` から読み込んだブラウザ全体の設定。
///
/// `#[non_exhaustive]` により、`[js]`（Issue #215）・`[rendering]`
/// （Issue #216）のフィールド追加を非破壊にする（REPAIR-4）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    profile: ProfileConfig,
}

impl Config {
    /// TOML 文字列から [`Config`] を構築する。
    ///
    /// 入力長を [`MAX_CONFIG_BYTES`] で事前検証してからパースする。空文字列・
    /// `[profile]` セクション省略は既定値として扱いエラーにしない。
    pub fn from_toml_str(input: &str) -> Result<Config> {
        if input.len() > MAX_CONFIG_BYTES {
            return Err(Error::from(ConfigError::TooLarge {
                limit: MAX_CONFIG_BYTES,
            }));
        }

        let raw: RawConfig = toml::from_str(input)
            .map_err(|source| Error::from(ConfigError::from_toml_de_error(input, &source)))?;

        let profile = ProfileConfig::from_raw(raw.profile.unwrap_or_default())?;

        Ok(Config { profile })
    }

    /// `path` の設定ファイルを読み込み、[`Config`] を構築する。
    ///
    /// [`MAX_CONFIG_BYTES`] を超えるファイルは中身を読み切る前に
    /// [`ConfigError::TooLarge`] を返す（`Read::take` で上限+1 バイトだけ読む）。
    /// `[profile] root` が相対パスの場合、`path` の親ディレクトリを基準に
    /// [`Path::join`] で解決する（呼び出し元の CWD に依存させないため）。
    pub fn load(path: &Path) -> Result<Config> {
        let file = File::open(path).map_err(|source| {
            Error::from(ConfigError::Io {
                message: format!("failed to open {}: {source}", path.display()),
            })
        })?;

        let mut limited = file.take(MAX_CONFIG_BYTES as u64 + 1);
        let mut bytes = Vec::new();
        limited.read_to_end(&mut bytes).map_err(|source| {
            Error::from(ConfigError::Io {
                message: format!("failed to read {}: {source}", path.display()),
            })
        })?;

        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(Error::from(ConfigError::TooLarge {
                limit: MAX_CONFIG_BYTES,
            }));
        }

        let text =
            String::from_utf8(bytes).map_err(|_source| Error::from(ConfigError::InvalidUtf8))?;

        let mut config = Config::from_toml_str(&text)?;

        if let Some(root) = config.profile.root.take() {
            let resolved = if root.is_absolute() {
                root
            } else {
                // 設定ファイルの親ディレクトリ基準で解決する（CWD 非依存）。
                // `path` がファイル名のみ（親コンポーネントなし）の場合、
                // `parent()` は `Some("")` を返しうるため `join` にそのまま
                // 委ね、空パス相対（= CWD 相対）として扱う。
                path.parent().unwrap_or_else(|| Path::new("")).join(&root)
            };
            config.profile.root = Some(resolved);
        }

        Ok(config)
    }

    /// プロファイル関連の設定（保存先・分離強度）を返す。
    pub fn profile(&self) -> &ProfileConfig {
        &self.profile
    }
}

/// `[profile]` セクションの設定（プロファイル保存先・分離強度。ビヘイビア
/// `PROF-6` が定める既定分離強度と対応する）。
///
/// `#[non_exhaustive]` により将来のフィールド追加（暗号化オプション等）を
/// 非破壊にする（REPAIR-4）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProfileConfig {
    root: Option<PathBuf>,
    isolation: IsolationStrength,
}

impl ProfileConfig {
    /// プロファイル保存先。`None` の場合、呼び出し元（`fandhe-browser-cli`
    /// 想定）がプラットフォーム既定のディレクトリを決める
    /// （将来仕様。プラットフォーム既定ディレクトリ解決は追加の依存が
    /// 必要になるため本 Issue（#214）では行わない。REPAIR-3）。
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// プロファイルの分離強度。
    pub fn isolation(&self) -> IsolationStrength {
        self.isolation
    }

    fn from_raw(raw: RawProfile) -> Result<ProfileConfig> {
        let root = match raw.root {
            Some(root) => {
                if root.is_empty() {
                    return Err(Error::from(ConfigError::InvalidValue {
                        key: "profile.root",
                        message: "profile.root must not be empty".to_string(),
                    }));
                }
                let candidate = PathBuf::from(&root);
                // `Config::load` は相対パスを設定ファイルの親ディレクトリへ
                // `Path::join` する（正規化しない）。ここで字句上
                // （ファイルシステムに触れず）正規化した結果が「移動なし」
                // （深さ 0）になる値（`.`・`profiles/..` 等）は、結合後の実体が
                // 設定ファイルの親ディレクトリ自体を指す。`Profile::open`
                // （TASK-50・#177）はそのディレクトリの権限変更・子ディレクトリ
                // 作成・ロック取得を行う契約のため、設定ファイルを置いた
                // ディレクトリへの意図しない副作用を防ぐためここで拒否する
                // （security.md「不安全な設計」・プロファイル境界。上位への
                // 脱出（`..` 等）自体の検証は二重実装せず `Profile::open` の
                // ハンドル基準検証に委ねる。モジュール doc「パス解決」参照）。
                if resolves_to_zero_depth(&candidate) {
                    return Err(Error::from(ConfigError::InvalidValue {
                        key: "profile.root",
                        message: format!(
                            "profile.root {:?} must not resolve to the config file's own \
                             directory (e.g. \".\" or \"profiles/..\")",
                            truncate_for_message(&root)
                        ),
                    }));
                }
                Some(candidate)
            }
            None => None,
        };

        let isolation = match raw.isolation {
            Some(value) => IsolationStrength::parse(&value)?,
            None => IsolationStrength::default(),
        };

        Ok(ProfileConfig { root, isolation })
    }
}

/// プロファイルの分離強度（ビヘイビア `PROF-6`）。
///
/// `#[non_exhaustive]` により、`profile` 分離（将来のオプトイン拡張）を
/// 追加する際に非破壊にする（REPAIR-4）。現時点では
/// [`IsolationStrength::DataDirectory`]（PROF-6 の既定: データディレクトリ
/// 分離 + advisory lock + パーミッション 700。`fandhe-browser-profile::
/// Profile::open`（TASK-50）が担う）の 1 variant のみを提供する。プロセス
/// 分離（`"process"`）は spec 上「将来のオプトイン拡張」と定義されており
/// 未実装のため、指定されると [`ConfigError::InvalidValue`] を返す
/// （実装済みを装わない。REPAIR-3）。
///
/// `fandhe-browser-profile` crate がこの型を消費するようになったら、
/// crate 間で共有する型は下位 crate へ置く規約（coding-rust.md「crate 構成と
/// 境界」）に従い、本型を `fandhe-browser-profile` へ移設すること。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum IsolationStrength {
    /// データディレクトリ分離 + advisory lock + パーミッション 700
    /// （PROF-6 の既定。`fandhe-browser-profile::Profile::open` が担う）。
    #[default]
    DataDirectory,
}

impl IsolationStrength {
    /// TOML の文字列表現に対応する `&'static str` を返す。
    pub fn as_str(self) -> &'static str {
        match self {
            IsolationStrength::DataDirectory => "data-directory",
        }
    }

    fn parse(value: &str) -> Result<IsolationStrength> {
        match value {
            "data-directory" => Ok(IsolationStrength::DataDirectory),
            "process" => Err(Error::from(ConfigError::InvalidValue {
                key: "profile.isolation",
                message: "profile.isolation \"process\" is not supported yet (process \
                          isolation is a planned opt-in; supported: data-directory)"
                    .to_string(),
            })),
            other => Err(Error::from(ConfigError::InvalidValue {
                key: "profile.isolation",
                message: format!(
                    "unknown profile.isolation {:?} (supported: data-directory)",
                    truncate_for_message(other)
                ),
            })),
        }
    }
}

/// `path` が字句上（ファイルシステムへ触れずコンポーネント解析のみで）
/// 「移動なし」（深さ 0）に正規化されるかを判定する。
///
/// `.`（`CurDir` のみ）や `profiles/..`（`Normal` 1 個と `ParentDir` 1 個が
/// 相殺）のように、結合先の基準ディレクトリそのものを指す値を検出するために
/// `ProfileConfig::from_raw` から呼ばれる。絶対パス（`RootDir`/`Prefix` を含む）
/// は対象外として `false` を返す。基準より上位へ脱出するパス（例: `".."`　や
/// `"a/../.."`）も `false` を返す（脱出自体の妥当性検証はここで二重実装せず
/// `Profile::open` のハンドル基準検証に委ねる。モジュール doc「パス解決」参照）。
fn resolves_to_zero_depth(path: &Path) -> bool {
    use std::path::Component;

    let mut depth: i64 = 0;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir => depth -= 1,
            Component::RootDir | Component::Prefix(_) => return false,
        }
        if depth < 0 {
            return false;
        }
    }
    depth == 0
}

/// エラーメッセージへ反響する外部入力由来の文字列を切り詰める
/// （security.md: 外部入力の反響・ログ肥大化を避ける）。
fn truncate_for_message(value: &str) -> String {
    const MAX_ECHO_CHARS: usize = 64;
    if value.chars().count() <= MAX_ECHO_CHARS {
        value.to_string()
    } else {
        let truncated: String = value.chars().take(MAX_ECHO_CHARS).collect();
        format!("{truncated}...")
    }
}

/// トップレベルの生 TOML 構造（非公開。serde の derive をここに閉じ込める）。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    profile: Option<RawProfile>,
}

/// `[profile]` セクションの生 TOML 構造（非公開）。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    root: Option<String>,
    isolation: Option<String>,
}

/// [`config`](self) モジュールが返すエラー（[`Error::Config`] の payload）。
///
/// `#[non_exhaustive]` により後続タスクでのバリアント追加を非破壊にする
/// （REPAIR-4）。
#[derive(Debug)]
#[non_exhaustive]
pub enum ConfigError {
    /// 設定ファイルの読み書き（実体はオープン・読み込みのみ）で発生した
    /// I/O エラー。`std::io::Error` は具象型のまま公開せず、メッセージへ
    /// 写像する（error.rs の方針を踏襲。パスは含めてよいが内容は含めない）。
    Io {
        /// 人間・AI 双方が読める英語メッセージ。
        message: String,
    },
    /// 読み込み対象が [`MAX_CONFIG_BYTES`] を超えている
    /// （アロケーション前に検査する。security.md）。
    TooLarge {
        /// 適用された上限（バイト数）。
        limit: usize,
    },
    /// 設定ファイルが妥当な UTF-8 ではなかった。
    InvalidUtf8,
    /// TOML の構文エラー（未知キーの検出を含む。`deny_unknown_fields`）。
    Syntax {
        /// `toml::de::Error::message()` 由来の英語メッセージ。
        message: String,
        /// エラー位置の行番号（1 始まり）。位置が特定できない場合は `None`。
        line: Option<usize>,
        /// エラー位置の列番号（1 始まり）。位置が特定できない場合は `None`。
        column: Option<usize>,
    },
    /// 構文的には正しいが、値が受理可能な範囲外だった
    /// （空の `root`・未知の `isolation` 値・未対応の `"process"` 等）。
    InvalidValue {
        /// 問題のあったキー（例: `"profile.root"`）。
        key: &'static str,
        /// 人間・AI 双方が読める英語メッセージ。
        message: String,
    },
}

impl ConfigError {
    /// `toml::de::Error` を `input` と突き合わせて 1 始まりの行・列に変換し、
    /// [`ConfigError::Syntax`] を構築する。
    ///
    /// `toml::de::Error` 自体は公開 API に出さない（外部クレートの具象型を
    /// 上位 crate へ漏らさない方針）ため、必要な情報（メッセージ・位置）を
    /// ここで取り出して汎用的な variant へ写像する。
    fn from_toml_de_error(input: &str, source: &toml::de::Error) -> ConfigError {
        let (line, column) = match source.span() {
            Some(span) => {
                let (line, column) = line_column(input, span.start);
                (Some(line), Some(column))
            }
            None => (None, None),
        };

        ConfigError::Syntax {
            message: source.message().to_string(),
            line,
            column,
        }
    }
}

/// バイトオフセット `start` を 1 始まりの (行, 列) に変換する。
///
/// `start` が `input` の文字境界でない場合は、直前の境界まで切り詰めてから
/// 計算する（外部由来のオフセットを添字アクセスへそのまま使わない。
/// coding-rust.md「外部入力の経路では添字アクセスを使わない」）。
fn line_column(input: &str, start: usize) -> (usize, usize) {
    let mut boundary = start.min(input.len());
    while boundary > 0 && !input.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let prefix: &str = input.get(..boundary).unwrap_or_default();

    let line = prefix.matches('\n').count() + 1;
    let column = match prefix.rfind('\n') {
        Some(newline_index) => match prefix.get(newline_index + 1..) {
            Some(rest) => rest.chars().count() + 1,
            None => 1,
        },
        None => prefix.chars().count() + 1,
    };

    (line, column)
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io { message } => write!(f, "I/O error: {message}"),
            ConfigError::TooLarge { limit } => {
                write!(f, "configuration file exceeds limit of {limit} bytes")
            }
            ConfigError::InvalidUtf8 => write!(f, "configuration file is not valid UTF-8"),
            ConfigError::Syntax {
                message,
                line,
                column,
            } => match (line, column) {
                (Some(line), Some(column)) => {
                    write!(f, "syntax error at line {line}, column {column}: {message}")
                }
                _ => write!(f, "syntax error: {message}"),
            },
            ConfigError::InvalidValue { key, message } => {
                write!(f, "invalid value for {key:?}: {message}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// TASK-91（91.1）: 空文字列は `Config::default()`（root `None`・
    /// isolation `DataDirectory`）と一致する。
    #[test]
    fn task_91_1_empty_input_yields_default_config() {
        let config = Config::from_toml_str("").expect("空文字列はパースできる");
        assert_eq!(config, Config::default());
        assert_eq!(config.profile().root(), None);
        assert_eq!(
            config.profile().isolation(),
            IsolationStrength::DataDirectory
        );
    }

    /// TASK-91（91.1）: `[profile]` のみ（キーなし）でも既定値になる。
    #[test]
    fn task_91_1_empty_profile_section_yields_default() {
        let config = Config::from_toml_str("[profile]\n").expect("空 [profile] はパースできる");
        assert_eq!(config, Config::default());
    }

    /// TASK-91（91.1）: `root` を指定すると `Some(PathBuf)` になる。
    #[test]
    fn task_91_1_root_is_parsed_as_path() {
        let config = Config::from_toml_str("[profile]\nroot = 'profiles/default'\n")
            .expect("root 指定はパースできる");
        assert_eq!(config.profile().root(), Some(Path::new("profiles/default")));
    }

    /// TASK-91（91.1）・PROF-6: `isolation = "data-directory"` は
    /// `IsolationStrength::DataDirectory` になる。
    #[test]
    fn task_91_1_isolation_data_directory_is_parsed() {
        let config = Config::from_toml_str("[profile]\nisolation = 'data-directory'\n")
            .expect("data-directory はパースできる");
        assert_eq!(
            config.profile().isolation(),
            IsolationStrength::DataDirectory
        );
    }

    /// TASK-91（91.1）・PROF-6: `isolation = "process"` は未対応として
    /// 明示エラーになる（実装済みを装わない。REPAIR-3）。
    #[test]
    fn task_91_1_isolation_process_is_rejected_as_unsupported() {
        let err = Config::from_toml_str("[profile]\nisolation = 'process'\n")
            .expect_err("process は未対応エラーになる");
        let message = err.to_string();
        assert!(message.contains("process"), "message was: {message}");
        assert!(message.contains("not supported"), "message was: {message}");
    }

    /// TASK-91（91.1）: 未知の `isolation` 値は指定値と受理可能な値を含む
    /// エラーになる。
    #[test]
    fn task_91_1_isolation_unknown_value_is_rejected() {
        let err = Config::from_toml_str("[profile]\nisolation = 'quickjs'\n")
            .expect_err("未知値はエラーになる");
        let message = err.to_string();
        assert!(message.contains("quickjs"), "message was: {message}");
        assert!(message.contains("data-directory"), "message was: {message}");
    }

    /// TASK-91（91.1）: 空文字列の `root` はエラーになる。
    #[test]
    fn task_91_1_empty_root_is_rejected() {
        let err = Config::from_toml_str("[profile]\nroot = ''\n")
            .expect_err("空文字列の root はエラーになる");
        assert!(matches!(
            err,
            Error::Config(ConfigError::InvalidValue {
                key: "profile.root",
                ..
            })
        ));
    }

    /// TASK-91（91.1）・PROF-6: `root = "."` は設定ファイルの親ディレクトリ
    /// 自体を指すため拒否される（Issue #538 レビュー指摘の回帰防止）。
    #[test]
    fn task_91_1_root_current_dir_is_rejected() {
        let err = Config::from_toml_str("[profile]\nroot = '.'\n")
            .expect_err("\".\" は設定ファイルのディレクトリ自体を指すためエラーになる");
        assert!(matches!(
            err,
            Error::Config(ConfigError::InvalidValue {
                key: "profile.root",
                ..
            })
        ));
    }

    /// TASK-91（91.1）・PROF-6: `root = "profiles/.."` は正規化すると
    /// 設定ファイルの親ディレクトリ自体を指すため拒否される
    /// （Issue #538 レビュー指摘の回帰防止）。
    #[test]
    fn task_91_1_root_normalizing_to_current_dir_is_rejected() {
        let err = Config::from_toml_str("[profile]\nroot = 'profiles/..'\n")
            .expect_err("\"profiles/..\" は正規化後に設定ファイルの親ディレクトリを指す");
        assert!(matches!(
            err,
            Error::Config(ConfigError::InvalidValue {
                key: "profile.root",
                ..
            })
        ));
    }

    /// TASK-91（91.1）: 基準より上位へ脱出するだけの値（`".."`）は、この層
    /// では「設定ファイルのディレクトリ自体」には該当しないため拒否しない
    /// （脱出自体の検証は `Profile::open` に委ねる方針。モジュール doc
    /// 「パス解決」参照）。
    #[test]
    fn task_91_1_root_parent_traversal_is_not_rejected_here() {
        let config = Config::from_toml_str("[profile]\nroot = '..'\n")
            .expect("\"..\" 自体はこの層では拒否しない");
        assert_eq!(config.profile().root(), Some(Path::new("..")));
    }

    /// TASK-91（91.1）: 通常の相対パス（`profiles/default`）は引き続き
    /// 受理される（回帰防止）。
    #[test]
    fn task_91_1_root_normal_relative_path_is_accepted() {
        let config = Config::from_toml_str("[profile]\nroot = 'profiles/default'\n")
            .expect("通常の相対パスは受理される");
        assert_eq!(config.profile().root(), Some(Path::new("profiles/default")));
    }

    /// TASK-91（91.1）: 型違い（`root` が文字列でない）は構文エラーになる。
    #[test]
    fn task_91_1_wrong_type_for_root_is_syntax_error() {
        let err =
            Config::from_toml_str("[profile]\nroot = 1\n").expect_err("型違いは構文エラーになる");
        assert!(matches!(err, Error::Config(ConfigError::Syntax { .. })));
    }

    /// TASK-91（91.1）: 型違い（`isolation` が真偽値）は構文エラーになる。
    #[test]
    fn task_91_1_wrong_type_for_isolation_is_syntax_error() {
        let err = Config::from_toml_str("[profile]\nisolation = true\n")
            .expect_err("型違いは構文エラーになる");
        assert!(matches!(err, Error::Config(ConfigError::Syntax { .. })));
    }

    /// TASK-91（91.1）: トップレベルの未知キーは `deny_unknown_fields` により
    /// 構文エラーになる（#215・#216 未マージの間、`[js]`・`[rendering]` を
    /// 含む TOML もこの経路でエラーになる）。
    #[test]
    fn task_91_1_unknown_top_level_key_is_rejected() {
        let err = Config::from_toml_str("[unknown]\nfoo = 1\n")
            .expect_err("未知セクションはエラーになる");
        assert!(matches!(err, Error::Config(ConfigError::Syntax { .. })));
    }

    /// TASK-91（91.1）: `[profile]` 内の未知キーは `deny_unknown_fields` により
    /// 構文エラーになる（タイプミスの黙殺を防ぐ）。
    #[test]
    fn task_91_1_unknown_profile_key_is_rejected() {
        let err =
            Config::from_toml_str("[profile]\nrooot = 'x'\n").expect_err("未知キーはエラーになる");
        assert!(matches!(err, Error::Config(ConfigError::Syntax { .. })));
    }

    /// TASK-91（91.1）: 構文エラーの行番号が正しく計算される（1 行目）。
    #[test]
    fn task_91_1_syntax_error_reports_line_one() {
        let err = Config::from_toml_str("[profile\n").expect_err("不正な構文はエラーになる");
        match err {
            Error::Config(ConfigError::Syntax { line, .. }) => {
                assert_eq!(line, Some(1));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// TASK-91（91.1）: 2 行目のエラーで行番号が 2 になる（行・列計算の検証）。
    #[test]
    fn task_91_1_syntax_error_on_second_line_reports_line_two() {
        let err = Config::from_toml_str("[profile]\nroot = 1\n").expect_err("型違いはエラーになる");
        match err {
            Error::Config(ConfigError::Syntax { line, .. }) => {
                assert_eq!(line, Some(2));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// TASK-91（91.1）: `MAX_CONFIG_BYTES` を超える入力は `TooLarge` になる。
    #[test]
    fn task_91_1_input_too_large_is_rejected() {
        let input = "a".repeat(MAX_CONFIG_BYTES + 1);
        let err = Config::from_toml_str(&input).expect_err("上限超過はエラーになる");
        assert!(matches!(
            err,
            Error::Config(ConfigError::TooLarge {
                limit: MAX_CONFIG_BYTES
            })
        ));
    }

    /// TASK-91（91.1）: `Error::from(ConfigError)` の `Display` 接頭辞と
    /// `source()` が `Some` になる。
    #[test]
    fn task_91_1_error_display_prefix_and_source() {
        let err = Error::from(ConfigError::InvalidUtf8);
        assert_eq!(
            err.to_string(),
            "configuration error: configuration file is not valid UTF-8"
        );
        assert!(std::error::Error::source(&err).is_some());
    }

    /// TASK-91（91.1）: `line_column` はマルチバイト文字を含む行でも
    /// 文字数ベースの列を返す（バイトオフセットをそのまま列にしない）。
    #[test]
    fn task_91_1_line_column_counts_chars_not_bytes() {
        let input = "あいう\nx";
        // 2 行目の 'x'（バイトオフセットは日本語 3 文字分で 9、'\n' で 10）。
        let (line, column) = line_column(input, 10);
        assert_eq!((line, column), (2, 1));
    }
}
