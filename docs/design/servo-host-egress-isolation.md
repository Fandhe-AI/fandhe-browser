# Servo ホストの egress 隔離方式: 3 OS の比較設計と実測計画

- **状態**: 設計（比較と実測計画）。**方式の採否は決めない**。決定は実測の後にオーナーが行う（#1003）
- **対応**: TASK-38・MS-4・`RENDER-3`・`SEC-2`・`PLUG-1`・`XOS-1`・Issue #1368（親判断 #1003）。関連は `RENDER-1`
- **入力資料**: [servo-host-egress-options.md](./servo-host-egress-options.md)（#1317。手段ごとの詳細表・Servo 0.7 の net 層の事実・承認事項の洗い出し。以下「options §n」と書く）。設計の親は [servo-host-design.md](./servo-host-design.md) §2.5・§13 項目 6
- **確度の表記**: 「確認済み」は原文またはコードを確認した事項、「推測」は読んだ範囲からの推論、「未確認」は調べていない事項。一次情報は options の出典（参照日 2026-10-29、Servo は `v0.7.0` タグ）に従い、判断の前に原文で再確認する。本書では新たな外部ページの再取得はしていない
- **spec 側の課題**（本リポからは編集しない。要報告）: egress 方針に専用のビヘイビア ID が無い。`RENDER-3` の本文はホストのサイズ・ビルド時間とオプトイン配布の設計材料で egress を扱わない。対応づけの確認は spec 側で行う

## 1. 2026-10-31 のオーナー判断の枠組み（#1003）

- **B（OS のネットワーク隔離）を本命**にする
- **A（Linux の `egress_guard.py` 型の事後検知）は補助**にする。A 単独は採らない（事後検出で SSRF の副作用を止められない。Linux 専用で `XOS-1` に合わない）
- B が成立しない OS は、既知の制約として利用者向け文書に明記する（D との併用）
- 方式の最終決定は実測の後。本書は最初の作業である比較設計と実測計画
- core が取得済みの HTML を渡す方式（#1003 の選択肢 3）は、2026-10-21 の判断で最後の手段とされており、比較の対象外

B の具体形は次の 2 つ。外部 http/https に出る要件があるため、現実的なのは後者（根拠は options §4）。

| 形 | 内容 |
| -- | ---- |
| 全遮断 | ホストのネットワークを丸ごと切る。保存済みページの撮影（`harness/render-screenshot/isolation_check.py` の `docker run --network none`。#1281）には使える前例がある。外部に出る用途とは両立しない |
| プロキシ＋接続先制限 | 本体側の許可リスト付きプロキシに Servo の HTTP(S) を通し、OS 隔離で「プロキシ以外へ出られない」ようにする |

## 2. 覆うべき経路と役割分担

| 経路 | プロキシ pref を通るか | 備考 |
| ---- | ---------------------- | ---- |
| HTTP リダイレクト・サブリソース（img・CSS・script・iframe・fetch / XHR） | 通る見込み（推測。options §2） | 1 本ごとに CONNECT になるため許可リストが毎回効く。プロキシが見るのは `host:port` まで |
| WebSocket | 通らない（確認済み。`websocket_loader.rs` が `TcpStream::connect` で直接接続） | OS 隔離でしか塞げない |
| DNS（ローカルでの名前解決・リバインディング） | プロキシ経由ならホスト名は CONNECT で渡る（推測） | プロキシ未使用の経路は `HyperHttpConnector` がローカルで引く（確認済み） |
| UDP（QUIC・直接 DNS 等） | 通らない | OS 隔離の対象 |
| その他の直接接続 | 不明 | net 層の網羅は未確認（options §7 の 1） |

役割分担の方針（設計案であり決定ではない）:

- 宛先 IP の判定（内部アドレスの遮断）は本体側プロキシが接続時に再検査して担う。DNS リバインディングへの実効的な対策もここ
- OS 隔離は「プロキシ以外へ出られない」ことを担う。OS 手段の多くは宛先 IP を絞れないため
- `allow_private_network_access`（既定 false）を弱める案は書かない。`true` 指定の意味は隔離を入れても変えない
- 隔離・プロキシ構成によって UA 等の識別面を変えない（`SEC-2`）
- 本体側プロキシは loopback のみに bind し、認証なしで外部へ公開しない（`security.md`）

## 3. 3 OS の比較表

観点は次の 6 つ。各セルの末尾に確度を付ける。手段ごとの長い記述と一次情報の URL は options §3 を参照する。

- ① 公開アドレスだけを許し内部アドレスを遮断できるか（宛先の粒度）
- ② リダイレクト・サブリソース・WebSocket・DNS の被覆
- ③ 必要な権限
- ④ 配布形態との整合。Servo ホストは musl 静的・scratch では配布せず、glibc（Debian slim）系の別イメージ（設計 §13 項目 8）。musl 静的・scratch は本体の既定・軽量コンテナだけに当てはまる
- ⑤ `allow_private_network_access` との整合
- ⑥ `PLUG-1` との関係（外部バイナリ・FFI・`unsafe`・動的ロードを増やさないか）

### 3.1 Linux

| 手段 | ① 宛先の粒度 | ② 被覆 | ③ 権限 | ④ 配布 | ⑤ PNA | ⑥ PLUG-1 |
| ---- | ------------ | ------ | ------ | ------ | ----- | -------- |
| netns（user ns + net ns。`unshare` / `bwrap`） | 全遮断。プロキシへは中継プロセスが要る（options §3.1。推測） | 全経路（WebSocket・UDP・DNS 含む） | カーネル上は非特権。Ubuntu 24.04 の AppArmor 制限で ns 内 capability が拒否される（options の記述） | Docker 既定 seccomp で `unshare` が拒否されうる（未確認） | 変えない | `pre_exec` + `unshare` は `unsafe`。`bwrap` は外部バイナリ |
| コンテナ外部の netns（`docker run --network none` 等） | 全遮断。プロキシへは別途経路が要る（未設計） | 全経路 | docker 権限（ホスト側の運用） | 前例あり（`isolation_check.py`。#55 の判断。保存済みページ撮影のみ） | 変えない | 本体の外側の隔離で、ホスト本体のコードは増えない |
| Landlock net（ABI 4、6.7 以降） | TCP のポート単位。宛先 IP は不可（options §3.1。確認済みとして記載） | TCP は被覆。ABI 4 は UDP・DNS を制限しない | 非特権（`no_new_privs`）。userns 不要 | Docker 既定 seccomp は `landlock_*` を許可（options の記述） | プロキシと同ポートの別 IP への直接接続は塞げない | 依存（`landlock` 等）と syscall の `unsafe` が候補（承認事項） |
| seccomp-bpf（socket 種別） | `socket()` の domain・type のみ。`connect()` の宛先は見えない | UDP・RAW を禁止して補完する役 | 非特権 | 同上 | 宛先 IP は絞れない | BPF 組み立ての依存か自前（承認事項） |
| cgroup BPF / nftables | IP・ポート | 全経路 | root / `CAP_NET_ADMIN` / `CAP_BPF` | 非特権起動の前提と衝突 | 絞れる | 要件に合わない（推測） |
| 補助 A: `egress_guard.py` | 検出のみ（接続済みソケットの宛先） | 25 ms 未満の短命な接続は取りこぼす。別 netns は検査不能 | 非特権 | Linux 専用 | 検出のみ | リポ内スクリプトで追加なし |

### 3.2 macOS

| 手段 | ① 宛先の粒度 | ② 被覆 | ③ 権限 | ④ 配布 | ⑤ PNA | ⑥ PLUG-1 |
| ---- | ------------ | ------ | ------ | ------ | ----- | -------- |
| `sandbox-exec -p`（Seatbelt） | 全遮断、`localhost:<port>`、`*:<port>`、プロトコル別。ホスト名・任意 IP は不可（Bazel #5206 の議論。Apple 公式ではない） | プロファイルでソケット全般を拒否できる見込み（未実測） | 非特権 | OS 同梱。**man に DEPRECATED とある**（options の記述） | 宛先 IP は絞れない | `Command` で外部の `/usr/bin/sandbox-exec` を起動するだけ。`unsafe`・依存は不要。別プロセス境界のみの方針と矛盾しないか要確認 |
| `sandbox_init()` | 同上 | 同上 | 非特権 | 非推奨の有無は未確認 | 同上 | C FFI の `unsafe` |
| App Sandbox（`network.client`） | 真偽値のみ | — | 署名済みバンドルが前提 | CLI の素の実行ファイルに効くか未検証 | — | — |
| Network Extension / PF | フローごと / IP・ポート | — | システム拡張・root | 要件に合わない（一般知識。推測） | — | — |

### 3.3 Windows

| 手段 | ① 宛先の粒度 | ② 被覆 | ③ 権限 | ④ 配布 | ⑤ PNA | ⑥ PLUG-1 |
| ---- | ------------ | ------ | ------ | ------ | ----- | -------- |
| AppContainer（capability なし） | 全遮断。loopback は既定で通らない | 全経路 | 作成は非特権。loopback 例外（`CheckNetIsolation` / `NetworkIsolationSetAppContainerConfig`）は開発・デバッグ用途とされ、管理者要否は未確認 | Servo ホスト（ANGLE の DLL・フォント・一時ディレクトリ）が中で動くか未検証 | プロキシ構成にするには loopback 例外が鍵 | `CreateProcess` の属性リストで `windows-sys` と `unsafe`（承認事項） |
| AppContainer + `internetClient` | インターネット宛を通し、LAN・loopback は通さない設計（Unity 資料のみ。推測） | 公開ホスト宛は止まらず、許可リストにならない | 非特権 | 同上 | private / loopback 宛だけを OS で落とす用途に限り有効かもしれない | 同上 |
| WFP / ファイアウォール規則 | IP・ポート・アプリ | 全経路 | 管理者（実質）。WFP の非特権フィルタ追加の可否は未検証 | 要件に合わない | 絞れる | — |
| Job Object | ネットワーク制限クラスなし（記憶。未確認） | — | — | — | — | — |

## 4. OS ごとの暫定見立て（採否ではない）

| OS | 見立て | 決める未確認事項 |
| -- | ------ | ---------------- |
| Linux | 見込みあり（条件付き）。Landlock + seccomp + プロキシ、またはコンテナ外部の netns。後者は運用側で隔離する前提 | 6.7 未満のカーネルでの扱い。宛先 IP を絞れない限界（options §4.1）。Docker 既定 seccomp と Ubuntu の AppArmor 制限下の挙動 |
| macOS | 条件付き。`(deny network*)` + loopback のプロキシのポートだけ許可 | `remote ip "localhost:<port>"` の実動作。非推奨 API が将来の OS で消える可能性。ヘッドレスでの GPU・フォントの動作（`RENDER-4` の前歴） |
| Windows | 不成立の公算が高い | AppContainer の loopback 例外に管理者権限が要るか。ホストが AppContainer 内で動くか |

B が成立しない OS の既知の制約として明記する文言（D）の置き場所の候補は、設計 §2.5 と README。文言は実測の後に決める。

## 5. 補助 A の位置づけ

- Linux の実測での観測手段として使う。B を入れた後の多層防御としても使える
- 限界を再掲する: 25 ms ポーリング・別 netns のプロセスは検査不能（`GuardUnavailable`）・Linux 専用。A の結果だけを保証として扱わない

## 6. 実測計画

### 6.1 共通の観測方法

loopback で配信するフィクスチャページ（`harness/render-screenshot/serve_pages.py` の流用を前提とする）で、実在の外部サイトや内部ネットワークには向けない。ページは次を試みる。

- (i) 許可外ホストへのリダイレクト
- (ii) 許可外ホストのサブリソース（img・fetch）
- (iii) 許可外の宛先への `new WebSocket()`
- (iv) 内部アドレス（プライベート IP・link-local）への接続
- (v) DNS・UDP の直接送信

合否の観測:

- 合格は、許可リスト外への接続が 0 件と観測されること。観測できない場合は fail-closed（不合格）
- 陽性の確認も行う: 隔離の下でも、ホストの `--selftest` と loopback ページの `load` が成功すること
- 観測手段は、プロキシのアクセスログ、許可外の宛先に置く受信側 listener（loopback または別 netns のダミー）、`isolation_check.py` 型の connect だけのプローブ（何も送らない）。Linux ではさらに `egress_guard.py` を併用する
- 実測ログには URL のクエリ・本文・プロキシ系の環境変数の値を載せず、有無だけを記録する。`sandbox-exec -p` のプロファイル等は固定値で、外部入力を連結しない

### 6.2 OS ごとの計画

| OS | 測ること | 環境 | 既知の限界 |
| -- | -------- | ---- | ---------- |
| Linux | Landlock ABI（`landlock_create_ruleset` のバージョン問い合わせ）。Landlock + seccomp 適用後のホストの動作。UDP・DNS の漏れ。`docker run`（Debian slim）入れ子での既定 seccomp 下の挙動。`--network none` の前例との比較 | ローカル + `xos-measure.yml` の ubuntu runner（sudo あり・Docker の中ではない）。6.7 未満はローカル VM または別ディストリ | runner のカーネルが固定される |
| macOS | `sandbox-exec` の `(deny network*)` + プロキシのポートのみ許可で、WebSocket の直接接続が止まるか。ホストが隔離下で動くか。OS 版による差（runner のイメージ版を記録） | `xos-measure.yml` の macos runner（ローカルでも可） | `--selftest` が隔離と無関係に失敗する場合の切り分け手順を先に用意する（ヘッドレスの GPU・WindowServer 制限。`RENDER-4`） |
| Windows | AppContainer の loopback 例外に管理者が要るか。プロキシ宛だけを通せるか。ANGLE の DLL・フォント・一時ディレクトリを含めホストが AppContainer 内で起動するか | **非管理者の実機またはローカル VM が必要**。`xos-measure.yml` の windows runner は管理者で動くため、鍵となる問いには答えられない | runner だけでは権限の問いに結論が出ない |

### 6.3 実施の扱い

- `xos-measure.yml`（手動専用・required ではない）への suite 追加と測定ハーネスの実装は後続 issue（infra-builder・render-builder）とする。本 issue では計画を書くだけにする
- 依存・`unsafe` を要する測定用コードは承認の後にする

## 7. 承認事項（実装しない）

方針は options §5 を参照。builder は着手しない（`dependency-policy.md`・`coding-rust.md`）。

| 項目 | 内容 |
| ---- | ---- |
| 依存 | `landlock`、seccomp 用クレート、`windows-sys`（いずれも未選定。ホスト側は `servo-host/` の別 workspace に置く案） |
| `unsafe` | `pre_exec`、`sandbox_init` の FFI、`CreateProcess` の属性リスト |
| 新設 | 本体側の許可リスト付きプロキシ（CONNECT。loopback 限定 bind・接続時の IP 再検査） |
| 変更 | `servo-host/src/engine.rs` の `fixed_preferences`（proxy pref の注入。クライアントから任意引数を渡さない方針〔設計 §2.6〕と整合させる） |
| CI | `xos-measure.yml` への suite 追加（required ではないため ruleset への影響なし） |

## 8. オーナー判断が要る点

- GA で必須とする成立範囲（3 OS 揃えるか、Linux を先行するか）
- B が成立しない OS の既知の制約の文言（D）
- Windows の扱い（弱い保証にとどめるか、対象外とするか）

## 9. 未確認事項

options §7 を引き継ぐ。本書で新たに加わる未確認事項はない。ただし Docker 既定 seccomp 下での `unshare` の挙動と、Landlock の実機の対応カーネルは、実測計画（§6.2）で確かめる。

### 9.1 2026-10-31 の Linux 実測で確認できた事項（#1377）

`harness/egress-isolation/`（#1377）で、依存・`unsafe` を使わない範囲を測った。結果の詳細は #1003 のコメントに記録している。方式の採否はここでも決めない。

- 環境: Ubuntu 26.04・カーネル 7.0.0-34・x86_64・`kernel.apparmor_restrict_unprivileged_userns=1`。ホストバイナリは 2026-10-27 ビルドの Docker イメージ由来（現在のコミットのビルドではない）。
- Landlock: LSM に有効で、ABI は 8（`landlock_create_ruleset` のバージョン問い合わせのみ。ルールセットの適用は未測定）。
- netns の作り方: AppArmor の制限下では `unshare -n`・`unshare -rn` は EPERM、`bwrap --unshare-net` は成功する。bwrap の ns 内に CAP_NET_ADMIN は無く（`ip addr add` は EPERM）、veth や 10.x のエイリアスは作れない。
- Docker 既定 seccomp 下（`--network none`）: `unshare -n` は EPERM。Landlock ABI の問い合わせは成功する。イメージに bwrap が無く、bwrap の可否は未確認。
- 隔離下の経路: bwrap の netns と `docker run --network none` のどちらでも、リダイレクト・img・fetch・WebSocket・内部アドレスの代替宛先は外側 listener に 0 件で、ベースライン（`--unshare-net` だけ無し）では各 1 件届いた。ホストの `--selftest` と loopback ページの `load` は両モードで成功した。実在の内部アドレス（IPv4・IPv6）への connect は ENETUNREACH。
- 残った未確認: Servo 自身の DNS・UDP 送信の外部観測、Landlock ルール適用下の動作、seccomp、許可リスト付きプロキシとの併用、macOS・Windows、AppArmor の制限が無い環境・古いカーネル、現在のコミットのホストでの再測定。
