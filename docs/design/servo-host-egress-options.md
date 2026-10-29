# Servo ホストの egress 方針の比較（判断材料の下書き）

- **状態**: 下書き（判断材料）。方式の決定は GA 前にオーナーが行う。2026-10-21 のオーナー判断で時機（GA 前に判断する）のみ確定している。2026-10-29 のオーナー承認により、比較表だけを先に作る
- **対応**: TASK-38（Servo ホスト）・`RENDER-3`（Servo ホストのオプトイン配布）・Issue #1003。関連は `SEC-2`・`PLUG-1`・`RENDER-1`
- **出典**: 設計 `servo-host-design.md` §2.5・§13 項目 6。本書は §2.5 の「残存リスク」に挙げた対策候補を OS ごとに比べ、実測すべき項目を洗い出すためのもの

## 本書の位置づけと注意

- 本書は**方式を決めない**。比較と、決めるために要る承認事項・実測項目を並べるだけである
- 外部ページ（Servo・hyper-util・各 OS の公式ドキュメント）の記述は、小モデルによる抽出を経た要約で、引用が原文と一語一句一致しない可能性がある。判断の前にリンク先の原文で再確認する
- 「推測」と書いた箇所は、読んだ範囲からの推論で、実測も原文確認もしていない。「未確認」は調べていない事項を指す
- 参照日は 2026-10-29、Servo は `v0.7.0` タグ
- ビヘイビア ID の注意（spec 側の課題として要報告。spec は本リポから編集しない）: `RENDER-3` の spec 本文はホストのバイナリサイズ・ビルド時間の数値化とオプトイン配布の設計判断材料で、egress は扱っていない。egress 方針に専用のビヘイビア ID は見当たらず、設計書 §2.5・§13 も #1003 だけを参照している。`PLUG-1` はプラグイン境界（`dylib` 動的ロードと WASM 同梱を採らず、実行時拡張を別プロセス境界に限る方針）で、本書では「OS 隔離のために外部バイナリや動的ロードを増やさない」観点で関係づけている。対応づけが正しいかは spec 側の確認が要る（`spec-reference.md`）

## 1. 現状

### 1.1 防げているもの

| 層 | 実装 | 防げるもの |
| -- | ---- | ---------- |
| クライアント（`fandhe-browser-render`） | `protocol.rs` の `validate_url`（送信前） | 長さ上限（`MAX_URL_BYTES`）、制御文字・空白・バックスラッシュ、`http` / `https` 以外の scheme、userinfo、空 host、不正な port |
| クライアント | `host_renderer.rs` の `check_network_target_url`（core のポリシー）の呼び出し。`allow_private_network_access` が既定 false | `load` の URL の到達先がループバック・プライベート IP・`localhost` のもの |
| クライアント | `host_renderer.rs` の `validate_final_url` | ホストが返した `final_url` が上記ポリシー違反、または userinfo つき |
| ホスト（`servo-host`） | `servo-host/src/server/request.rs` の URL 再検証（設計 §2.5 の二重化。コメントによれば本体側 `validate_url` と同じ規則: http / https のみ・userinfo 拒否・host 非空・port は数字）。ループバック・プライベート IP の到達先ポリシーをホスト側でも検査するかは、本書では未確認（クライアント側の `check_network_target_url` は確認済み） | クライアントを経由しない入力の形式不正 |
| ホスト | `engine.rs` の `fixed_preferences`（プロキシ 3 pref を空文字にする。テスト `fixed_config_is_closed`） | `http_proxy` 等の環境変数由来のプロキシを持ち込まない（識別面・SSRF 経路。`SEC-2`） |
| 起動 | `env_clear` と許可リスト（設計 §2.6）。`*_PROXY` は渡さない | 環境変数経由のプロキシ設定 |

### 1.2 防げていないもの（設計 §2.5 の残存リスク）

URL の検証は**最初の `load` の URL だけ**に効く。Servo がページ内で自分で始める通信には効かない。

| 経路 | 状況 |
| ---- | ---- |
| HTTP リダイレクト | Servo 内部で辿る。最終 URL は `final_url` で事後に検査されるが、途中の中継先への接続は止まらない |
| サブリソース（画像・CSS・スクリプト・iframe・`fetch` / XHR） | Servo が直接取得する。URL 検証の対象外 |
| DNS リバインディング | 検証時と接続時で名前解決の結果が変わりうる。Servo が名前解決を自分で行うため、本体は接続先 IP を見ない |
| WebSocket | `websocket_loader.rs` が `TcpStream::connect` で直接接続し、プロキシ pref を参照しない（確認済み。§2） |
| その他の直接接続 | Servo の net 層の全ファイルを読んでいないため、WebSocket 以外にあるかは**未確認** |

本番の実行経路に egress ガードは**無い**。`harness/render-screenshot/egress_guard.py` は撮影ハーネス（`RENDER-5`・TASK-37）専用の検出器で、次の限界がある（同ファイルの docstring による）。

- `/proc` のソケット表を 25 ms 間隔でポーリングし、検証プロキシ以外への接続を検出したらプロセスツリーを kill する。止めるのではなく検出する方式で、より短命な接続は取りこぼす
- Linux 専用。別のネットワーク名前空間のプロセスは検査できない（`GuardUnavailable` として fail-closed にする）
- 完全な保証ではなく、ネットワーク名前空間による隔離の代わりにはならない

## 2. Servo 0.7 の net 層の事実

| 事項 | 結果 | 確度 |
| ---- | ---- | ---- |
| HTTP(S) のプロキシ | `connector.rs` の `ProxyConnector::new` が `hyper_util::client::proxy::matcher::Matcher::builder().http(pref!(network_http_proxy_uri)).https(pref!(network_https_proxy_uri)).no(pref!(network_http_no_proxy))` で構築する | 取得内容に基づく |
| 環境変数 | このファイルは環境変数を読まず、`Matcher::builder()` も読まない（`from_env` / `from_system` だけが読む）。`engine.rs` が 3 つの pref を空にしているのは、`Preferences::default()` 側の環境由来の値を消すため | 確認済み |
| HTTPS | `intercept()` が当たった要求は `Tunnel`（CONNECT）経由。TLS はそのトンネルの上で張る | `Tunnel` の内部は未読 |
| loopback の自動除外 | なし。`no` に書かない限り、プロキシ自身の宛先もプロキシ経由になる。`no` が空文字なら実害はない | 確認済み（hyper-util master） |
| プロキシの種類・認証 | `http` / `socks4` / `socks4a` / `socks5` / `socks5h`。userinfo は HTTP プロキシなら Basic 認証として扱われる | hyper-util master の内容。Servo 0.7 が固定する版は未確認 |
| WebSocket | `websocket_loader.rs` は `TcpStream::connect((domain, port))` で直接接続し、プロキシ設定を参照しない。`egress_guard.py` の記述とも一致 | 確認済み |
| 直接 DNS | プロキシ未使用の経路は `ServoHttpConnector` → `HyperHttpConnector` がローカルで名前解決する | 確認済み |
| ホスト名の解決 | プロキシ経由なら宛先ホスト名は CONNECT で渡り、ローカルでは引かない。本体側プロキシが接続時に IP を再検査でき、これが DNS リバインディングの実効的な対策になる | 推測（CONNECT の中身は未読） |
| リダイレクト・サブリソース | 1 本ごとに CONNECT になるので許可リストが毎回効く。ただしプロキシが見えるのは `host:port` までで、URL パスは見えない（TLS を中継するため） | 推測 |
| 他の直接接続 | `http_loader.rs` は単一の `ServoClient` を使うが、末尾 21KB と他ファイルは未読。全箇所の網羅調査は未実施 | 未確認 |

読み取れること（推測を含む）は次の 2 点。

- プロキシ pref を本体側の許可リスト付きプロキシに向ければ、HTTP(S) のリダイレクト・サブリソース・DNS 解決を本体側で扱える見込みがある
- WebSocket など pref を通らない経路は、プロキシだけでは塞げない。OS 側の隔離（接続先の制限）が別に要る

## 3. OS × 手段の比較

「非特権」は一般ユーザー権限だけで使えるかどうか。「宛先の粒度」は、その手段で絞れる接続先の単位。

### 3.1 Linux

| 手段 | 宛先の粒度 | 非特権 | 本体からの適用方法 | 既知の制約 | 一次情報 |
| ---- | ---------- | ------ | ------------------ | ---------- | -------- |
| user ns + net ns（`CLONE_NEWUSER\|CLONE_NEWNET`） | 全遮断。新しい netns には loopback だけが入る（bubblewrap の記述） | カーネル上は可（Linux 3.8 以降、user ns と他の `CLONE_NEW*` を同時指定できる）。**Ubuntu 24.04 では既定の AppArmor 制限で ns 内の capability が拒否される** | `pre_exec` + `unshare` は `unsafe` かつ libc 依存。外部の `bwrap` を起動する手もあるが実行時依存になる | 親 ns への veth は親側の権限が要るはずで（未検証）、プロキシへの到達は ns 内に中継プロセスを置いて継承 fd で本体につなぐ構成が必要。loopback を up にするには ns 内の `CAP_NET_ADMIN` が要る（未検証）。**Docker 既定の seccomp は `CAP_SYS_ADMIN` なしでは `unshare` と `CLONE_NEW*` 付き `clone` を拒否**。`clone3` も `ENOSYS`。ほかに、userns の悪用による DoS 懸念の注意書きがある | [user_namespaces(7)](https://man7.org/linux/man-pages/man7/user_namespaces.7.html)、[bubblewrap README](https://raw.githubusercontent.com/containers/bubblewrap/main/README.md)、[moby seccomp](https://raw.githubusercontent.com/moby/profiles/main/seccomp/default.json)、[Ubuntu 制限の概説](https://discourse.ubuntu.com/t/understanding-apparmor-user-namespace-restriction/58007) |
| Landlock net（ABI 4、Linux 6.7 以降） | **TCP の bind / connect をポート単位**で許可。宛先 IP・ホスト名は絞れない。ABI 4 は UDP と DNS を制限しない | 可（`no_new_privs` が必須）。userns を使わないので Ubuntu の AppArmor 制限とは無関係（推測）。Docker 既定 seccomp も `landlock_*`・`prctl`・`seccomp` を無条件許可 | ホスト起動直後に自分自身へ適用するのが最も素直（`servo-host` 側の依存追加＋`unsafe` / syscall。親の `pre_exec` より安全）。「connect はプロキシのポートだけ」にできる | 新しいドキュメントは UDP を ABI 10 で追加するが、実機の対応カーネルは未確認。6.7 未満のディストリでは使えない。ポートが同じなら任意 IP に届く。Landlock が拒否した接続は WebSocket の直接接続を含めて止まる | [kernel Landlock](https://docs.kernel.org/userspace-api/landlock.html)、[landlock(7)](https://man7.org/linux/man-pages/man7/landlock.7.html) |
| seccomp-bpf | `socket()` の引数（domain・type）でソケット種別を禁止できる。`connect()` の宛先は見えない | 可（`no_new_privs`） | ホスト自身か `pre_exec`。BPF の組み立てに依存クレートか自前実装が要る | フィルタが見るのはレジスタ値で、`sockaddr` の中身は見えない（man ページの記述からの推論）。宛先の判別には `SECCOMP_RET_USER_NOTIF` と監督プロセスが要る。Landlock と組んで「UDP・IPv6・AF_UNIX 以外のソケットを禁止」に使うのが現実的 | [seccomp(2)](https://man7.org/linux/man-pages/man2/seccomp.2.html) |
| cgroup BPF / nftables / iptables | IP・ポート・ホスト（IP 集合） | 不可（root / `CAP_NET_ADMIN`、`CAP_BPF`）。netns 内なら権限を得られるが経路が無い | 要件に合わない | man ページ未取得のため、この行は一般知識（推測） | 未取得 |
| 現行の `egress_guard.py` | 接続済みソケットの宛先（検出のみ） | 可 | ポーリング監視で、止めるのではなく検出して kill する | 25 ms 未満の短命な接続は取りこぼす。Linux 専用。別 netns のプロセスは検査不能 | リポ内 `harness/render-screenshot/egress_guard.py` |

### 3.2 macOS

| 手段 | 宛先の粒度 | 非特権 | 本体からの適用方法 | 既知の制約 | 一次情報 |
| ---- | ---------- | ------ | ------------------ | ---------- | -------- |
| `sandbox-exec -p <profile> host`（Seatbelt） | 全遮断、`localhost:<port>`、`*:<port>`、プロトコル別。**ホスト名・任意 IP は不可**（プロファイルの host は `*` か `localhost` のみ） | 可 | `Command::new("/usr/bin/sandbox-exec")` で起動するだけ。`unsafe` と依存は不要 | man ページに **DEPRECATED** とある（調査時の手元の man、2017 年版）。プロファイル言語は非公式で OS 版ごとに変わる。実行テストは未実施（本調査では実行手段がなかった） | 手元 `/usr/share/man/man1/sandbox-exec.1`、[Bazel #5206](https://github.com/bazelbuild/bazel/issues/5206)、[northpole.security](https://northpole.security/docs/workshop/rules/sandbox-rules.md) |
| `sandbox_init()` で自分自身に適用 | 同上 | 可 | C FFI で `unsafe` が要る | 手元に `sandbox_init(3)` の man が無く、非推奨の有無は未確認 | `/usr/share/man/man3/sandbox_init.3` は存在せず |
| App Sandbox（`com.apple.security.network.client`） | 真偽値のみ。宛先の粒度なし（一般知識） | 署名済みバンドルが前提 | CLI の素の実行ファイルに効くかは**未検証** | Apple のページは JS 描画のため本文を取得できなかった | [entitlement ページ](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.network.client)（本文未取得） |
| Network Extension（NEFilterDataProvider） | 任意（フローごと） | 不可（システム拡張、Apple 付与の entitlement、ユーザー承認が要る。一般知識） | 要件に合わない | 本文未取得のため、この行は推測 | [Filtering Network Traffic](https://developer.apple.com/documentation/networkextension/filtering-network-traffic)（本文未取得） |
| PF（`pfctl`） | IP・ポート | 不可（root）。一般知識 | 要件に合わない | 未取得 | — |

### 3.3 Windows

| 手段 | 宛先の粒度 | 非特権 | 本体からの適用方法 | 既知の制約 | 一次情報 |
| ---- | ---------- | ------ | ------------------ | ---------- | -------- |
| AppContainer（capability なし） | 全遮断。capability の有無で通信を切る | 可（`CreateAppContainerProfile` は現在ユーザーのプロファイルを作る） | `CreateProcess` + `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` が要り、`std::process::Command` では不可。`windows-sys` と `unsafe` が要る（承認事項） | **UWP は既定で loopback の受信が許可されない**。例外化の `CheckNetIsolation LoopbackExempt` は「開発・デバッグ用途」とされ、`NetworkIsolationSetAppContainerConfig` も debugging 用。管理者要否は取得ページに明記なし | [AppContainer isolation](https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-isolation)、[hh780593](https://learn.microsoft.com/en-us/previous-versions/windows/apps/hh780593(v=win.10))、[troubleshooting-uwp-firewall](https://learn.microsoft.com/en-us/windows/security/operating-system-security/network-security/windows-firewall/troubleshooting-uwp-firewall)、[SetAppContainerConfig](https://learn.microsoft.com/en-us/windows/win32/api/netfw/nf-netfw-networkisolationsetappcontainerconfig) |
| AppContainer + `internetClient`（S-1-15-3-1） | インターネット宛を通す。**LAN と loopback は通さない設計**（Unity ドキュメントの記述。Microsoft の同旨の文面は未確認） | 可 | 同上 | 公開ホスト宛のリダイレクトは止まらず、許可リストにはならない。「プライベート IP / loopback 宛だけを OS で落とす」用途に限り有効かもしれない。Servo ホスト（ANGLE の DLL、フォント、一時ディレクトリ）を AppContainer で動かせるかは**未検証** | [networking-basics](https://learn.microsoft.com/bs-latn-ba/windows/apps/develop/networking/networking-basics)、troubleshooting-uwp-firewall の Case 1 |
| Windows Filtering Platform | IP・ポート・アプリ・パッケージ単位 | 実質不可。フィルタエンジンの既定 SD は Administrators に GENERIC_ALL、一般ユーザーは OPEN / CLASSIFY のみ（フィルタ追加の可否は未検証） | 要件に合わない | — | [WFP Access control](https://learn.microsoft.com/en-us/windows/win32/fwp/access-control)、[FwpmEngineOpen0](https://learn.microsoft.com/en-us/windows/win32/api/fwpmu/nf-fwpmu-fwpmengineopen0) |
| Job Object | ネットワーク制限クラスは無い（記憶によるもので未確認） | 可 | — | 帯域スロットルはあるが遮断ではない（記憶によるもので未確認） | 未取得 |
| Windows ファイアウォール規則 | IP・ポート・アプリ | 管理者（一般知識。未取得） | 要件に合わない | 推測 | — |

## 4. 3 案の 3 OS 成立性

案は次の 3 つ。

- **選択肢 1**: egress ガード（接続先を絞る）
- **選択肢 2**: OS 隔離（ネットワークを丸ごと切る）
- **プロキシ＋OS 隔離**: Servo の HTTP(S) を本体側の許可リスト付きプロキシに通し、OS 隔離でプロキシ以外への接続を塞ぐ

Issue #1003 の選択肢 3（core が取得済みの HTML を渡す方式）は、v1 の「ホストが自分で取得する」決定を変えるため最後の手段とされている（#1003 の推奨・2026-10-21 のオーナー判断）。本書では比較の対象にしていない。

| 案 | Linux | macOS | Windows | 総合 |
| -- | ----- | ----- | ------- | ---- |
| **選択肢 1: egress ガード** | 条件付き。検出のみなら現行の `/proc` ポーリング（保証にならない）。**予防**はポート単位の Landlock しかなく、IP は絞れない | 不成立。非特権で宛先ホストを絞れる手段が見つからない（Seatbelt の host は `*` か `localhost` のみ、NE は特権・承認が要る） | 不成立。WFP は管理者前提 | **不成立**。OS 間で揃わず、WebSocket 直接接続と DNS リバインディングも残る |
| **選択肢 2: OS 隔離** | 条件付き。netns は非特権で動くが、Ubuntu 24.04 の AppArmor 制限と Docker 既定 seccomp で塞がれうる | 成立。`sandbox-exec` で全遮断できる（非推奨 API） | 成立。capability なしの AppContainer（`unsafe` と `windows-sys` が要る） | 技術的には 3 OS で全遮断できる。ただし**外部 http/https に出るという要件と矛盾**し、単独では使えない |
| **プロキシ＋OS 隔離** | 条件付き。ポート単位の **Landlock**（6.7 以降）＋ seccomp で socket 種別を制限が現実的で、user ns を使わず Docker でも動く見込み。6.7 未満のディストリでは不可。netns 方式は中継プロセスが要り、Ubuntu / Docker で塞がれうる | 条件付き。`sandbox-exec` の `(deny network*)` ＋ `(allow network-outbound (remote ip "localhost:<port>"))`。プロファイルの動作は未実測、非推奨 API | **不成立の公算が高い**。AppContainer は loopback を既定で通さず、例外化は開発用途のみで管理者要否も未確認。`internetClient` だけのコンテナで private / loopback を落とす緩い代替案にとどまる | 本体側プロキシが許可リストと再検査を担い、OS 隔離が WebSocket 直接接続などの迂回を塞ぐ。**唯一 DNS リバインディングとリダイレクトの両方を扱える案**だが、Windows が最大の障害 |

プロキシ構成の前提は次のとおり（推測を含む）。

- Servo の `Preferences` の `network_http_proxy_uri` と `network_https_proxy_uri` を、本体が loopback で待ち受けるプロキシの `http://<loopback アドレス>:<port>` に向ける
- `network_http_no_proxy` は空のままにする（loopback の自動除外は無いため、空なら実害はない）
- 本体側プロキシは新規コンポーネントで、置き場所（`fandhe-browser-render` か別 crate か）と待ち受けの扱いは未設計。loopback のみに bind することは、`security.md` の「アクセス制御の不備」観点で確認が要る

## 5. 案ごとの承認事項と変更が要る箇所

以下はすべて**承認が要る事項の洗い出し**で、決定ではない。`dependency-policy.md`（依存の追加はユーザー承認・`=x.y.z` 固定）と `coding-rust.md`（`unsafe` の新規追加はユーザー承認）に従い、builder は着手しない。ライセンスは `licensing.md` の許可範囲か確認が要る。

### 5.1 案・OS ごとの承認事項

| 案・OS | 依存クレート | `unsafe` / FFI | ほか |
| ------ | ------------ | -------------- | ---- |
| プロキシ＋Landlock（Linux） | `landlock`、seccomp 用クレート（自前の BPF 組み立てにするなら不要）。いずれも未選定 | 自プロセスへの適用でも syscall 呼び出しが要る見込み（クレートが包むなら本リポの `unsafe` は不要な可能性。推測） | 6.7 未満のカーネルでの扱い（無効化か起動拒否か）。`servo-host` は `deny.toml` を別に持つため、依存は `servo-host/` 側に置く案が筋（要判断） |
| プロキシ＋Seatbelt（macOS） | 不要 | `Command` で `/usr/bin/sandbox-exec` を起動するだけなら不要。`sandbox_init()` の FFI なら `unsafe` | 非推奨 API を採ること自体がリスク（将来の OS で消える可能性）。外部コマンド起動は `PLUG-1` の「別プロセス境界のみ」と矛盾しないか確認 |
| AppContainer（Windows） | `windows-sys` | `CreateProcess` と属性リストの組み立てで `unsafe` が必須 | ルート `.cargo/config.toml` の `+crt-static`（#917）と `servo-host/` の `-crt-static` の違いを踏まえた配置の確認。loopback 例外の権限が未確認のため、プロキシ構成では成立しない公算が高い |
| 本体側プロキシ（案共通） | HTTP プロキシ（CONNECT）の実装に依存クレートを使うか自前か。未選定 | 不要の見込み | 許可リストの設計、接続時の IP 再検査（DNS リバインディング対策）、loopback 限定 bind、ログに URL のクエリを出さないこと（`security.md`） |
| 選択肢 1（ガード単独） | Linux の予防は Landlock と同じ | 同上 | 3 OS で成立しないため、採るなら OS ごとに異なる保証水準を公開することになる |
| 選択肢 2（全遮断単独） | 各 OS の手段と同じ | 同上 | 外部 http/https に出る要件と矛盾。「core が取得済みの HTML を渡す方式」（設計 §2.5 の別の対策候補）と組む場合に限り意味がある。本書では未評価 |

### 5.2 プロキシを使う場合に変更が要る `servo-host/src/engine.rs`

| 箇所 | 現状 | 変更の内容（案） |
| ---- | ---- | ---------------- |
| `fixed_preferences` の `network_http_proxy_uri`・`network_https_proxy_uri` | 空文字 | 本体側プロキシの loopback アドレスとポートを指す。値の渡し方（起動引数か、ハンドシェイクか）は未設計。クライアントから任意引数は渡さない方針（設計 §2.6）との整合が要る |
| `fixed_preferences` の `network_http_no_proxy` | 空文字 | 空のまま。loopback の自動除外が無いため、空でプロキシ宛もプロキシ経由になる（実害なしの見込み） |
| テスト `fixed_config_is_closed` | 3 つの pref が空であることを assert | 「環境変数由来のプロキシを持ち込まない」の意図を保ったまま、新しい期待値（注入されたプロキシ宛であること）に直す |
| `fixed_preferences` のドキュメントコメント | 「proxy は識別面・SSRF 経路」 | `*_PROXY` 環境変数を渡さない方針（`SEC-2`）と、pref で固定のプロキシを指すことが矛盾しない旨に改める。UA 等の識別面を変えないことは維持する |
| `Opts.sandbox` | `false` に固定 | Servo 内部のコンテンツプロセスのサンドボックスで、`multiprocess` 前提と思われるが未確認。OS 隔離とは別物 |

## 6. オーナー判断が要る点

### (a) GA で必須とする成立範囲

3 OS すべてで同じ保証を求めるか、Linux を先行するか。

- 現時点で 3 OS すべてで成立する案は見つかっていない（§4）
- Linux 先行にする場合、macOS・Windows は「この対策なし」の状態で GA することになる。公開する制約の書き方（README・設計 §2.5）も合わせて決める必要がある

### (b) 対策導入を GA 条件とするかの判断

- 導入を GA の条件にする: 実装規模は依存追加・`unsafe`・新規プロキシ・3 OS の CI を含み大きい。#1003 は GA 前の判断としているため、判断後の起票が要る
- 既知の制約として公開する: 現状の二重検証だけで GA する。その場合、リダイレクト・サブリソース・DNS リバインディング・WebSocket から内部ネットワークに到達しうることを明記する。到達先の内部ネットワークが無い運用（クラウドの隔離環境等）では、運用側の隔離に頼る前提になる

### (c) Windows でプロキシ＋OS 隔離が不成立の公算が高い場合の扱い（対応方針の選択）

- Windows だけ弱い保証（`internetClient` のみの AppContainer で private・loopback を落とす）にとどめるか
- Windows は GA の対象外（もしくは対策なしを明記）にするか
- 外部に出ない構成（core が取得済みの HTML を渡す方式）を Windows だけに使うか。設計 §2.5 の別案で、本書では評価していない
- 前提として、Windows の loopback 例外の権限（下記 (d)）が実測で分かるまで、この項目は確定できない

### (d) 次に実測で確かめる項目

実測で判断材料を埋める候補。いずれも本書では未実施。

| 項目 | 確かめること |
| ---- | ------------ |
| macOS の `sandbox-exec` プロファイル | `(deny network*)` ＋ `(allow network-outbound (remote ip "localhost:<port>"))` が実際に効くか。WebSocket の直接接続が止まるか。Servo ホスト（GPU・フォント・一時ディレクトリ）が動くか。OS 版ごとの差 |
| Linux の Landlock | 対象カーネル（6.7 以降）で、Servo ホストが自プロセスへ適用した後も動くか。Docker 既定 seccomp 下で適用できるか。UDP・DNS の漏れ。6.7 未満での挙動 |
| Windows の AppContainer の loopback 例外の権限 | `CheckNetIsolation LoopbackExempt` や `NetworkIsolationSetAppContainerConfig` に管理者が要るか。要らないなら、プロキシ宛の loopback だけを通せるか。Servo ホスト（ANGLE の DLL、フォント、一時ディレクトリ）が AppContainer 内で動くか |
| Servo の net 層の網羅 | `http_loader.rs` の残りと他ファイルに、WebSocket 以外の直接接続が無いか。`Tunnel` が CONNECT でホスト名をそのまま渡すか |

## 7. 未確認事項（推測を含む）

1. Servo 0.7.0 の `http_loader.rs` 末尾 21KB と他の net 層ファイル。直接接続の全箇所は網羅できていない（WebSocket 以外にあるか不明）
2. hyper-util `Tunnel` が CONNECT でホスト名をそのまま渡すこと、Servo 0.7 が固定する hyper-util の版、プロキシ認証 userinfo を Servo が使えるか
3. Windows の `CheckNetIsolation LoopbackExempt` が管理者を要するか。取得した Microsoft のページに明記はなかった。資料は「開発・デバッグ用途」としている
4. Windows で `internetClient` のみの AppContainer が private 宛・loopback 宛を落とす点（Unity 資料のみ）。Servo ホスト自体が AppContainer 内で動くか
5. macOS の `sandbox-exec` での `(remote ip "localhost:<port>")` の実動作。調査時に実行手段がなく、プロファイルは未実測。App Sandbox の entitlement を CLI の素の実行ファイルに適用できるか。Network Extension の要件は Apple のページ本文が取れず一般知識
6. Landlock 以外の Linux 手段の詳細。netns 内で loopback を up にする権限、`pasta` / `slirp4netns` などの rootless ネットワークは未調査（外部バイナリでライセンス未確認）
7. Landlock の UDP 制限（ABI 10）が実際に入るカーネル、および RHEL / Debian 安定版が 6.7 未満である点（一般知識）
8. 依存追加が必要になりうる箇所はすべて承認事項で、本書では判断していない。`landlock` クレート、seccomp 用クレート、Windows の `windows-sys` と `unsafe` が該当する（`.claude/rules/dependency-policy.md`）
9. Servo 内部の `Opts.sandbox`（コンテンツプロセスのサンドボックス）は `multiprocess` 前提と思われるが未確認。`engine.rs` は両方とも `false` に固定している

## 出典

- Servo（`v0.7.0` タグ）: [connector.rs](https://raw.githubusercontent.com/servo/servo/v0.7.0/components/net/connector.rs)、[websocket_loader.rs](https://raw.githubusercontent.com/servo/servo/v0.7.0/components/net/websocket_loader.rs)、[http_loader.rs](https://raw.githubusercontent.com/servo/servo/v0.7.0/components/net/http_loader.rs)（途中まで）
- hyper-util: [matcher.rs（master）](https://raw.githubusercontent.com/hyperium/hyper-util/master/src/client/proxy/matcher.rs)
- Linux・macOS・Windows の各ページは §3 の表の URL。macOS の Seatbelt の host 制約は [Bazel #5206](https://github.com/bazelbuild/bazel/issues/5206)（2018 年の議論で、Apple 公式の記述ではない）
- リポ内: `docs/design/servo-host-design.md` の §2.5・§2.6・§13、`servo-host/src/engine.rs`、`harness/render-screenshot/egress_guard.py`、`crates/fandhe-browser-render/src/protocol.rs`（`validate_url`）、`crates/fandhe-browser-render/src/host_renderer.rs`（`validate_final_url`・`check_network_target_url` の呼び出し）
- spec（SSOT。本リポからは編集しない）: `04-behavior/rendering-layer.md` の `RENDER-1`・`RENDER-3`、`04-behavior/plugin-extension.md` の `PLUG-1`、`04-behavior/security-policy.md` の `SEC-2`
