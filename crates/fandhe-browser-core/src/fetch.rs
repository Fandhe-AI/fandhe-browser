//! fetch: URL からの HTTP/HTTPS 取得を担うモジュール。
//!
//! `cdp`（`Page.navigate` 等）や将来の CLI から呼ばれ、取得した生のバイト列を
//! `parse` モジュール（TASK-24.4・#38）へ渡す下位レイヤー（TASK-24.2・
//! ビヘイビア `CORE-1`）。ネットワーク応答は信頼できない外部入力として扱い、
//! 次の方針を徹底する（coding-rust.md「エラーハンドリング」・security.md
//! 「不安全な設計」「SSRF」）:
//!
//! - 全体タイムアウト・接続タイムアウト・リダイレクト回数・レスポンス本文の
//!   各上限を [`FetchOptions`] で設定し、超過時は `panic` せず [`crate::Error`]
//!   を返す
//! - `http`/`https` 以外の scheme（`file:` 等）は要求前・リダイレクト先の
//!   両方で拒否する
//! - 外部入力の経路（レスポンスヘッダ・本文）で `unwrap`/`expect`/添字アクセス
//!   を使わない
//!
//! 本実装は async API（[`Fetcher::get`]）のみを提供し、`reqwest::blocking` は
//! 使わない（tokio ランタイム内から呼ぶと panic するため。CDP サーバー
//! （TASK-41・#171）は tokio 前提の想定で、この制約と衝突する）。
//!
//! ## 本 Issue（#36）の範囲外（将来仕様。REPAIR-3: 実装済みを装わない）
//!
//! - ローカルファイル（`file:`）の取得: 受け入れ基準外のため対応しない
//! - 内部・プライベートアドレスへのアクセス制御（SSRF の深掘り）: ブラウザでは
//!   `localhost` への到達も正当な用途があるため、方針は人間の判断事項とする
//! - Cookie セッション維持（ビヘイビア CORE-5 (8)）
//! - 文字コード判定（ビヘイビア CORE-5 (7)）: 本文は常にバイト列で返す
//! - gzip 等の展開・HTTP/2・プロキシ設定: 挙動の決定性を優先し、`no_proxy()`
//!   で環境変数のプロキシ設定を無視する

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::redirect::Policy;
use reqwest::{Client, ClientBuilder, Url};

use crate::error::{Error, Result};

/// デフォルトの全体タイムアウト（30 秒）。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// デフォルトの接続タイムアウト（10 秒）。
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// デフォルトの最大リダイレクト回数。
const DEFAULT_MAX_REDIRECTS: usize = 10;
/// デフォルトの最大レスポンス本文サイズ（16 MiB）。
const DEFAULT_MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// `Fetcher::new` の呼び出し元が指定する取得条件。
///
/// `#[non_exhaustive]` により、後続タスクでのフィールド追加（gzip 展開の
/// 有効化等）が呼び出し側の構造体リテラル初期化を破壊的変更にしない
/// （REPAIR-4: 戻り値・入力は将来拡張できる構造にする）。`Default` と
/// `with_*` メソッドで組み立てる。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FetchOptions {
    /// リクエスト全体のタイムアウト（接続確立からレスポンス完了まで）。
    pub timeout: Duration,
    /// TCP/TLS 接続確立のタイムアウト。
    pub connect_timeout: Duration,
    /// 許可する最大リダイレクト回数。リダイレクト先への遷移がこの回数を
    /// 超えた時点（`attempt.previous().len() > max_redirects` となった
    /// 時点）で [`Error::TooManyRedirects`] を返す。
    pub max_redirects: usize,
    /// 許可する最大レスポンス本文サイズ（バイト）。`Content-Length` による
    /// 事前検査と、ストリーミング読み込み中の逐次検査の両方で使う。
    pub max_body_bytes: u64,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

impl FetchOptions {
    /// 既定値で [`FetchOptions`] を作る（[`Default`] と同義）。
    pub fn new() -> Self {
        Self::default()
    }

    /// リクエスト全体のタイムアウトを設定する。
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 接続確立のタイムアウトを設定する。
    #[must_use]
    pub fn with_connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    /// 許可する最大リダイレクト回数を設定する。
    #[must_use]
    pub fn with_max_redirects(mut self, max_redirects: usize) -> Self {
        self.max_redirects = max_redirects;
        self
    }

    /// 許可する最大レスポンス本文サイズ（バイト）を設定する。
    #[must_use]
    pub fn with_max_body_bytes(mut self, max_body_bytes: u64) -> Self {
        self.max_body_bytes = max_body_bytes;
        self
    }

    /// 各フィールドが `Fetcher::new` で受理できる値かを検証する
    /// （ゼロ値は「即座に失敗する設定」であり呼び出し側の誤りである
    /// 可能性が高いため、`InvalidInput` として早期に拒否する）。
    fn validate(&self) -> Result<()> {
        if self.timeout.is_zero() {
            return Err(Error::InvalidInput {
                message: "FetchOptions::timeout must not be zero".to_string(),
            });
        }
        if self.connect_timeout.is_zero() {
            return Err(Error::InvalidInput {
                message: "FetchOptions::connect_timeout must not be zero".to_string(),
            });
        }
        if self.max_body_bytes == 0 {
            return Err(Error::InvalidInput {
                message: "FetchOptions::max_body_bytes must not be zero".to_string(),
            });
        }
        Ok(())
    }
}

/// HTTP/HTTPS でページを取得するクライアント。内部に `reqwest::Client` を
/// 保持し、接続プールを再利用する（呼び出しのたびに構築しない）。
///
/// `cdp` の `Page.navigate` ハンドラ・将来の CLI から生成・保持され、
/// [`Fetcher::get`] を介して呼び出される想定（TASK-24.2・CORE-1）。
pub struct Fetcher {
    client: Client,
    options: FetchOptions,
}

/// rustls の crypto provider を 1 回だけ install するためのガード。
///
/// `Fetcher::new` は複数回呼ばれ得るが、`rustls::crypto::CryptoProvider` の
/// install はプロセス全体で 1 回のみ許可される（2 回目以降は `Err` を返す）。
/// 既に別の provider（例: 呼び出し元アプリケーションが独自に install した
/// もの）が入っている場合は、そのエラーを無視して既存の provider を使う
/// （`unwrap` しない。coding-rust.md「エラーハンドリング」）。
static CRYPTO_PROVIDER_INIT: OnceLock<()> = OnceLock::new();

fn ensure_crypto_provider_installed() {
    CRYPTO_PROVIDER_INIT.get_or_init(|| {
        // 戻り値の `Result<(), Arc<CryptoProvider>>` は「既に別の provider が
        // install 済み」を示すだけで、この crate にとってはどちらの provider
        // が実際に使われるかに関わらず後続の TLS 接続は成立する。
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// リダイレクトポリシーが打ち切りを検出したときに埋め込む非公開マーカー。
/// `reqwest::Error::source()` を辿って `downcast_ref` で判定するためだけに
/// 存在し、Display 文字列を外部へ公開しない（送信エラーの写像内で判定した
/// 後、`Error::TooManyRedirects`/`Error::DisallowedScheme` へ書き換える）。
#[derive(Debug)]
enum RedirectMarker {
    TooManyRedirects { limit: usize },
    DisallowedScheme { scheme: String },
}

impl std::fmt::Display for RedirectMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RedirectMarker::TooManyRedirects { limit } => {
                write!(f, "too many redirects (limit: {limit})")
            }
            RedirectMarker::DisallowedScheme { scheme } => {
                write!(f, "disallowed redirect scheme: {scheme}")
            }
        }
    }
}

impl std::error::Error for RedirectMarker {}

impl Fetcher {
    /// `options` を検証したうえで `Fetcher` を構築する。
    ///
    /// - crypto provider（ring）をプロセス内で 1 回だけ install する
    /// - `options` の不正な値（ゼロのタイムアウト・ゼロの本文上限）を
    ///   [`Error::InvalidInput`] として拒否する
    /// - タイムアウト・接続タイムアウト・リダイレクトポリシー・User-Agent を
    ///   設定し、環境変数のプロキシ設定は `no_proxy()` で無視する（挙動の
    ///   決定性のため。プロキシ対応自体は将来仕様）
    pub fn new(options: FetchOptions) -> Result<Self> {
        ensure_crypto_provider_installed();
        options.validate()?;

        let max_redirects = options.max_redirects;
        let redirect_policy = Policy::custom(move |attempt| {
            if attempt.previous().len() > max_redirects {
                return attempt.error(RedirectMarker::TooManyRedirects {
                    limit: max_redirects,
                });
            }
            let scheme = attempt.url().scheme().to_string();
            match scheme.as_str() {
                "http" | "https" => attempt.follow(),
                other => attempt.error(RedirectMarker::DisallowedScheme {
                    scheme: other.to_string(),
                }),
            }
        });

        let client = ClientBuilder::new()
            .timeout(options.timeout)
            .connect_timeout(options.connect_timeout)
            .redirect(redirect_policy)
            .no_proxy()
            .user_agent(concat!("fandhe-browser/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|source| map_reqwest_error(source, &options))?;

        Ok(Self { client, options })
    }

    /// `url` を GET で取得する。
    ///
    /// 呼び出し元との契約: 本メソッドはランタイムを内蔵しない async fn だが、
    /// 内部の `reqwest::Client`（hyper 経由）は tokio 前提であり、呼び出し元
    /// （`cdp` の `Page.navigate` ハンドラ・将来の CLI 等）が tokio ランタイム
    /// 上で await する必要がある（tokio ランタイムの外で呼ぶと失敗する）。
    ///
    /// - `url` を [`reqwest::Url`] として解析し、`http`/`https` 以外の
    ///   scheme（`file:`・`data:` 等）は送信前に [`Error::DisallowedScheme`]
    ///   として拒否する（解析失敗は [`Error::InvalidInput`]）
    /// - `Content-Length` ヘッダが `max_body_bytes` を超える場合は本文を
    ///   読まずに [`Error::ResponseTooLarge`] を返す（(a) 事前検査）
    /// - ヘッダが無い・嘘の値でも、`chunk()` による逐次読み込み中に蓄積
    ///   サイズを `checked_add` で検査し、上限超過時点で同じエラーを返す
    ///   （(b) ストリーミング検査）
    /// - 4xx/5xx は `Err` にせず `Ok` で返す（404 ページ等もパース対象に
    ///   なり得るため、判断は呼び出し側に委ねる）
    pub async fn get(&self, url: &str) -> Result<FetchResponse> {
        let parsed = Url::parse(url).map_err(|source| Error::InvalidInput {
            message: format!("invalid URL: {source}"),
        })?;
        reject_disallowed_scheme(&parsed)?;

        let response = self
            .client
            .get(parsed)
            .send()
            .await
            .map_err(|source| map_reqwest_error(source, &self.options))?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        if let Some(content_length) = response.content_length()
            && content_length > self.options.max_body_bytes
        {
            return Err(Error::ResponseTooLarge {
                limit: self.options.max_body_bytes,
            });
        }

        let mut final_url = response.url().clone();
        // userinfo（`user:pass@host`）はリダイレクト後の URL にも残り得る
        // ため、公開する `final_url` からは常に取り除く（security.md
        // 秘密情報混入防止）。`set_username`/`set_password` は既に空の
        // 場合も含め常に `Ok` を返す入力（空文字列・`None`）のみを渡す。
        let _ = final_url.set_username("");
        let _ = final_url.set_password(None);

        let capacity = response
            .content_length()
            .unwrap_or(0)
            .min(self.options.max_body_bytes);
        let mut body = Vec::with_capacity(usize::try_from(capacity).unwrap_or(0));

        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|source| map_reqwest_error(source, &self.options))?
        {
            let new_len = body
                .len()
                .checked_add(chunk.len())
                .ok_or(Error::ResponseTooLarge {
                    limit: self.options.max_body_bytes,
                })?;
            if new_len as u64 > self.options.max_body_bytes {
                return Err(Error::ResponseTooLarge {
                    limit: self.options.max_body_bytes,
                });
            }
            body.extend_from_slice(&chunk);
        }

        Ok(FetchResponse {
            status,
            final_url: final_url.to_string(),
            content_type,
            body,
        })
    }
}

/// URL の scheme を検証し、`http`/`https` 以外を拒否する
/// （security.md「SSRF」: `file:`・`data:` 等の無検証アクセスを避ける）。
fn reject_disallowed_scheme(url: &Url) -> Result<()> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(Error::DisallowedScheme {
            scheme: other.to_string(),
        }),
    }
}

/// `reqwest::Error` を [`Error`] へ写像する。
///
/// - タイムアウト（`is_timeout()`）は [`Error::Timeout`] にする。全体
///   タイムアウトと接続タイムアウトを reqwest 側は区別しないため、
///   `options.timeout` を代表値として使う
/// - リダイレクト起因のエラーは、`source()` を辿って [`RedirectMarker`] へ
///   `downcast_ref` できればそれを対応する `Error` に写像する。マーカーが
///   見つからなくても `is_redirect()` が真なら `TooManyRedirects` として
///   扱う（フォールバック）
/// - それ以外は `without_url()` で URL（userinfo・クエリを含み得る）を
///   取り除いたメッセージを [`Error::Network`] に詰める（外部型
///   `reqwest::Error` 自体は保持しない）
fn map_reqwest_error(source: reqwest::Error, options: &FetchOptions) -> Error {
    if source.is_timeout() {
        return Error::Timeout {
            limit: options.timeout,
        };
    }

    let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&source);
    while let Some(err) = cause {
        if let Some(marker) = err.downcast_ref::<RedirectMarker>() {
            return match marker {
                RedirectMarker::TooManyRedirects { limit } => {
                    Error::TooManyRedirects { limit: *limit }
                }
                RedirectMarker::DisallowedScheme { scheme } => Error::DisallowedScheme {
                    scheme: scheme.clone(),
                },
            };
        }
        cause = err.source();
    }

    if source.is_redirect() {
        return Error::TooManyRedirects {
            limit: options.max_redirects,
        };
    }

    Error::Network {
        message: source.without_url().to_string(),
    }
}

/// [`Fetcher::get`] の結果。
///
/// `#[non_exhaustive]` により、後続タスクでのフィールド追加（レスポンス
/// ヘッダ全体の保持等）が呼び出し側の構造体リテラル・非網羅 `match` を
/// 破壊的変更にしない（REPAIR-4）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FetchResponse {
    status: u16,
    final_url: String,
    content_type: Option<String>,
    body: Vec<u8>,
}

impl FetchResponse {
    /// HTTP ステータスコード。4xx/5xx でも `Err` にはならず、ここに格納
    /// される（呼び出し側がハンドリングを判断する）。
    pub fn status(&self) -> u16 {
        self.status
    }

    /// リダイレクト後の最終 URL（userinfo は除去済み）。
    pub fn final_url(&self) -> &str {
        &self.final_url
    }

    /// `Content-Type` レスポンスヘッダの値（存在する場合）。
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// レスポンス本文の生バイト列。文字コード判定（ビヘイビア CORE-5 (7)）
    /// は本 crate の範囲外のため、呼び出し側または `parse` モジュール
    /// （TASK-24.4・#38）が行う想定。
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// レスポンス本文を UTF-8 として非可逆変換した文字列（不正なバイト列は
    /// 置換文字に置き換える）。厳密な文字コード判定（`charset` ヘッダ・
    /// meta タグ由来）は範囲外の将来仕様であり、これは参考用の簡易変換に
    /// 留まる。
    pub fn body_text_lossy(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}
