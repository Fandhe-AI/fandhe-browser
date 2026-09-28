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
//!
//! `profile.root` は設定ファイルの親ディレクトリ配下に限定する（Issue #538 P0
//! レビュー指摘: `fandhe-browser-profile::Profile::open` は渡された `root` の
//! 許容範囲を検証せず、そのまま権限変更・ロックファイル作成・子ディレクトリ
//! 作成を行う契約であり〔`Profile::open` の doc 参照〕、字句上の脱出防止を
//! 同関数へ委ねる従来の想定は誤りだった）。検証は 2 段構成で、いずれも
//! 副作用が起きる前（[`Config::load`] が `Config` を返すより前）に行う。
//!
//! 1. 相対パスの字句上の脱出（`..`・`profiles/..`・`.` 等、途中で深さが
//!    負になる場合も含む）を [`ProfileConfig::from_raw`]（文字列のみを扱う
//!    [`Config::from_toml_str`] からも呼ばれるため、ファイルパスなしで判定
//!    できる範囲に限る）で拒否する
//! 2. 絶対パスを設定ファイルの親ディレクトリ配下に限定する検証を
//!    [`Config::load`] で行う（ファイルパスを知っているのは `load` のみの
//!    ため。字句正規化〔`..`・`.` の畳み込み〕はするが `canonicalize`・
//!    `exists` は呼ばずファイルシステムには触れない）
//!
//! symlink に対する防御はこの層では行わず、
//! `fandhe-browser-profile::Profile::open`（TASK-50・#177）が担うハンドル
//! 基準（`openat` + `NOFOLLOW`）の走査に委ねる（二重実装しない）。つまり
//! 「許容範囲内かどうか」は config クレートが、「範囲内の各要素が symlink
//! でないか」は profile クレートが受け持つ、という役割分担になる。
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
                // 絶対パスは設定ファイルの親ディレクトリ配下に限定する
                // （Issue #538 P0 レビュー指摘）。`ProfileConfig::from_raw` の
                // 字句判定（`resolves_outside_or_at_base_dir`）は絶対パスを
                // 対象外にしているため、ファイルパスを知っている本関数でのみ
                // 検証できる。`Profile::open` へ渡す前（副作用の前）に拒否する。
                let base_dir = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let base_abs = std::path::absolute(base_dir).map_err(|source| {
                    Error::from(ConfigError::Io {
                        message: format!(
                            "failed to resolve config directory {}: {source}",
                            base_dir.display()
                        ),
                    })
                })?;
                let base_abs = normalize_lexically(&base_abs);
                let root_abs = normalize_lexically(&root);

                if !is_lexically_within(&base_abs, &root_abs) {
                    return Err(Error::from(ConfigError::InvalidValue {
                        key: "profile.root",
                        message: format!(
                            "profile.root {:?} must resolve to a location under the \
                             config file's directory ({})",
                            truncate_for_message(&root.display().to_string()),
                            base_abs.display()
                        ),
                    }));
                }

                // 検証に使った字句正規化済みパス（`root_abs`）をそのまま
                // 採用する（Issue #538 P0 レビュー再指摘）。未正規化の
                // `root` を返すと、検証済みの最終到達点と実際に
                // `fandhe-browser-profile::Profile::open`（TASK-50・#177）が
                // 要素ごとに辿るパスが食い違う。例えば設定ディレクトリが
                // `<dir>` のとき `root = "<dir>/../outside/../<dir 名>/profiles"`
                // は字句正規化後の最終到達点こそ `<dir>/profiles`（境界内）だが、
                // 未正規化のまま渡すと `Profile::open` は `outside` という
                // 実在しない兄弟ディレクトリを経由して辿ろうとし、設定
                // ディレクトリ外への作成につながりかねない（security.md
                // 「不安全な設計」・プロファイル境界）。字句正規化済みの
                // `root_abs` を渡すことで、境界検証と実際に使われるパスを
                // 一致させる。
                root_abs
            } else {
                // 設定ファイルの親ディレクトリ基準で解決する（CWD 非依存）。
                // `path` 自体が相対パス（例: `config/fandhe-browser.toml`）の
                // 場合、`parent()` も相対のままとなり、そのまま `join` すると
                // `resolved` も相対パスになってしまう。`Config::load` 完了後に
                // 呼び出し元が CWD を変更すると、`fandhe-browser-profile::
                // Profile::open`（TASK-50・#177）へ渡した時点で別の場所を
                // 開いてしまい、本モジュールが掲げる「CWD 非依存」の契約
                // （モジュール doc「パス解決」参照）を満たせない（Issue #538
                // P1 レビュー指摘）。そのため、絶対パス分岐と同様に
                // `std::path::absolute` で親ディレクトリを先に絶対パスへ
                // 固定してから `root` を結合する（`canonicalize` は使わず
                // ファイルシステムには触れない）。相対パスの字句上の脱出は
                // `ProfileConfig::from_raw` が既に拒否済みのため、ここで
                // 組み立てた結果は常に設定ファイルの親ディレクトリ配下に
                // 収まる。
                let base_dir = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let base_abs = std::path::absolute(base_dir).map_err(|source| {
                    Error::from(ConfigError::Io {
                        message: format!(
                            "failed to resolve config directory {}: {source}",
                            base_dir.display()
                        ),
                    })
                })?;
                base_abs.join(&root)
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
                // （ファイルシステムに触れず）正規化した結果、設定ファイルの
                // 親ディレクトリ自体を指す（深さ 0。`.`・`profiles/..` 等）か、
                // それより上位へ脱出する（深さが負になる。`..`・`a/../..` 等）
                // 値は拒否する（Issue #538: 当初は上位への脱出自体の検証を
                // 二重実装せず `Profile::open` のハンドル基準検証に委ねる
                // 想定だったが、`Profile::open` は `root` の許容範囲を検証
                // しない契約〔同関数の doc 参照〕であるため、ここで拒否しないと
                // 検証されないまま `Profile::open` の副作用（権限変更・子
                // ディレクトリ作成・ロック取得）に渡ってしまう。security.md
                // 「不安全な設計」・プロファイル境界。絶対パスの限定は
                // ファイルパスを知っている `Config::load` 側で行う。モジュール
                // doc「パス解決」参照）。
                if resolves_outside_or_at_base_dir(&candidate) {
                    return Err(Error::from(ConfigError::InvalidValue {
                        key: "profile.root",
                        message: format!(
                            "profile.root {:?} must not resolve to the config file's own \
                             directory or an ancestor of it (e.g. \".\", \"profiles/..\", \
                             or \"..\")",
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
/// 基準ディレクトリ自体（深さ 0）、またはそれより上位（深さが途中で負に
/// なる）に正規化されるかを判定する（Issue #538 P0 レビュー指摘対応）。
///
/// `.`（`CurDir` のみ）や `profiles/..`（`Normal` 1 個と `ParentDir` 1 個が
/// 相殺）のように基準ディレクトリそのものを指す値、および `".."`・
/// `"a/../.."` のように基準ディレクトリより上位へ脱出する値の両方を検出する
/// ために `ProfileConfig::from_raw` から呼ばれる。絶対パス（`RootDir`/
/// `Prefix` を含む）は対象外として `false` を返す（このパスパターンには
/// 「基準ディレクトリ相対の深さ」という概念が適用できないため。絶対パスの
/// 限定は `Config::load` が別途行う。モジュール doc「パス解決」参照）。
fn resolves_outside_or_at_base_dir(path: &Path) -> bool {
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
            return true;
        }
    }
    depth <= 0
}

/// パスをファイルシステムへ一切触れずに字句正規化する（`.` を除去し、
/// `..` を直前の `Normal` 要素と相殺する）。
///
/// [`Config::load`] が絶対 `root` を設定ファイルの親ディレクトリ配下に
/// 限定する検証（Issue #538 P0 レビュー指摘）の前処理として使う。
/// `std::path::absolute`（呼び出し元が事前に適用する）は絶対パスをそのまま
/// 返すだけで `..`/`.` を畳み込まないため、比較の前に本関数で正規化する。
/// ルート（`RootDir`/`Prefix`）より上位へ脱出する `..` は、実際の OS の
/// パス解決と同様にルート自身へ留める（捨てる）。`canonicalize`・`exists`
/// は呼ばない（symlink 解決は `fandhe-browser-profile::Profile::open` に
/// 委ねる。モジュール doc「パス解決」参照）。
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match result.components().next_back() {
                Some(Component::Normal(_)) => {
                    result.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {
                    // ルートより上位へは脱出しない（OS のパス解決と同様に
                    // ルート自身に留める）。
                }
                _ => {
                    result.push("..");
                }
            },
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// `candidate`（字句正規化済みの絶対パス）が `base`（同じく字句正規化済みの
/// 絶対パス）の配下（`base` 自身は含まない）にあるかを字句上で判定する。
///
/// [`Config::load`] が絶対 `root` を設定ファイルの親ディレクトリ配下に
/// 限定する検証（Issue #538 P0 レビュー指摘）で使う。
/// `fandhe-browser-profile::assert_within_root` と同じ判定方針（`base` 自身
/// は許可しない・残りのコンポーネントがすべて `Normal` であること）だが、
/// crate 間の依存方向（core は profile に依存してよいが、本チェックは
/// 単純な字句比較のみで新規依存を要しないため導入しない。coding-rust.md
/// 「crate 構成と境界」）に配慮し、config crate 内で完結させる。大文字小文字を
/// 区別しない OS では、大文字小文字だけが異なるパスを「範囲外」と判定し得る
/// （拒否側に倒れるため安全側の制約。coding-rust.md「クロスプラットフォーム」）。
fn is_lexically_within(base: &Path, candidate: &Path) -> bool {
    use std::path::Component;

    let Ok(rest) = candidate.strip_prefix(base) else {
        return false;
    };

    let mut components = rest.components().peekable();
    if components.peek().is_none() {
        return false;
    }
    components.all(|component| matches!(component, Component::Normal(_)))
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

    /// TASK-91（91.1）・PROF-6: 基準より上位へ脱出する値（`".."`）は拒否
    /// される（Issue #538 P0 レビュー指摘の回帰防止。当初は `Profile::open`
    /// への委譲を想定していたが、同関数は `root` の許容範囲を検証しない
    /// 契約のためここで拒否する）。
    #[test]
    fn task_91_1_root_parent_traversal_is_rejected() {
        let err = Config::from_toml_str("[profile]\nroot = '..'\n")
            .expect_err("\"..\" は基準ディレクトリより上位へ脱出するため拒否される");
        assert!(matches!(
            err,
            Error::Config(ConfigError::InvalidValue {
                key: "profile.root",
                ..
            })
        ));
    }

    /// TASK-91（91.1）・PROF-6: 途中で深さが負になる値（`"a/../.."`）も
    /// 拒否される（Issue #538 P0 レビュー指摘の回帰防止）。
    #[test]
    fn task_91_1_root_negative_depth_midway_is_rejected() {
        let err = Config::from_toml_str("[profile]\nroot = 'a/../..'\n")
            .expect_err("\"a/../..\" は途中で基準より上位へ脱出するため拒否される");
        assert!(matches!(
            err,
            Error::Config(ConfigError::InvalidValue {
                key: "profile.root",
                ..
            })
        ));
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
