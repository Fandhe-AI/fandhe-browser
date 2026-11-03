# egress-isolation（Servo ホストの egress 隔離の実測。Linux・macOS・Windows）

Issue #1377（Linux）・#1387（macOS・Windows）・TASK-38・`RENDER-3`・`SEC-2`・`PLUG-1`・`XOS-1`。親判断 #1003 の比較設計
`docs/design/servo-host-egress-isolation.md` §6（実測計画）のうち、**依存・`unsafe`・pip を使わずに測れる
Linux の範囲**を測るハーネス。**方式の採否は決めない**。結果は測定条件つきの事実として記録する。

## 何を測るか

許可外の宛先を loopback のダミー listener（127.0.0.2 / 127.0.0.3）に置き、Servo ホストに次の経路を試させる。

| 経路 | 内容 | 観測手段 |
| ---- | ---- | -------- |
| `redirect` | 許可外ホストへの 302 | 外側 listener の受信・内側 listener の受信 |
| `img` / `fetch` | 許可外ホストのサブリソース | 同上 |
| `ws` | `new WebSocket()` | 同上 |
| `internal` | 内部アドレスの代替宛先（127.0.0.3） | 同上 |
| `udp_direct` | 内側ランナー自身の UDP `sendto`（Servo ではなくハーネスのプローブ） | 外側 UDP listener の受信 |
| `internal_real` | 実在の内部アドレス（隔離下の内側だけで試す） | 経路表＋connect だけのプローブ（外部からは観測しない） |
| `dns` | hosts に無い名前の解決（ハーネス自身の `getaddrinfo`） | 経路表＋解決失敗（Servo 自身の DNS・UDP 送信は外から観測しない） |

モード: `baseline`（bwrap。`--unshare-net` だけ無し。陽性対照）・`bwrap-netns`（`--unshare-net`）・
`docker-none`（`docker run --network none`。イメージがあるときだけ）。

判定は `pass` / `fail` / `unobservable`（`judge.py`）。外側の受信が 1 件でもあれば `fail`。`pass` は「外側 0 件・
ベースラインで届いた・内側で試行を観測・guard が監視できた・`--selftest` と loopback の `load` が成功」の全部を要し、
欠ければ `unobservable`（合格とは扱わない。fail-closed）。

## 使い方

```bash
# 自己テスト（偽ホストのみ。bwrap・docker 不要）
make check-egress-isolation

# 実測（要 bwrap。docker-none は --docker-image を足す）
python3 -I -B harness/egress-isolation/measure.py \
  --host /abs/path/to/fandhe-browser-servo-host \
  --modes baseline,bwrap-netns [,docker-none --docker-image <image>] \
  --out "$(mktemp -d)"

# docker-none の準備（glibc をそろえたホストを Debian trixie のイメージ内でビルドして取り出す。#1386）
bash harness/egress-isolation/container/prepare.sh build fandhe-egress-isolation:local
bash harness/egress-isolation/container/prepare.sh extract fandhe-egress-isolation:local "$(mktemp -d)"
# 続けて、表示された取り出し先のバイナリを --host に渡す:
#   measure.py --host <取り出したバイナリ> --modes baseline,bwrap-netns,docker-none \
#     --docker-image fandhe-egress-isolation:local --out "$(mktemp -d)"

# Landlock ABI・netns の作り方ごとの可否・sysctl（読み取りのみ）
python3 -I -B harness/egress-isolation/platform_probe.py [--docker-image <image>]
```

`--out` に `result.json`・`summary.md`・モードごとの内側結果が出る。`make ci` と CI には含めない
（実測は人間の判断材料。後続の候補: `harness-ubuntu` への自己テストのステップ追加）。

### docker-none で glibc をそろえる理由

measure.py は全モードで同一バイナリ（sha256 一致。`binary_match`）を要求する。ホストの新しい glibc でビルドした
バイナリをイメージ（Debian trixie・glibc 2.41）へ COPY すると `GLIBC_2.43` 等を要求して起動できない
（#1377 の再実測 run 2）。そこで `container/Dockerfile` が trixie 内でホストをビルドし、`prepare.sh extract` が
同じファイルを取り出して全モードの `--host` に使う（glibc 2.41 でリンクしたバイナリは新しいホストでも動く）。
イメージは push・配布しない（MPL-2.0 部分を含む）。ベースイメージのダイジェストは
`harness/render-screenshot/container/Dockerfile` と同値で、`test_container.py` がドリフトを検査する。

## 安全上の規則

- 宛先は 127.0.0.0/8 のダミーだけ。実在の内部アドレスへの connect / fetch は、隔離下の内側で `lo` 以外の
  インターフェース・経路が無いことを確かめた後にだけ行う（満たさなければ exit 2 で中止し何も読み込まない）。
- argv は定数リスト・`shell=False`。`--host` は実行可能ファイルの絶対パス、`--docker-image` は正規表現で検証。
- ログ・結果に URL のクエリ・本文・プロキシ系環境変数の値を載せない（有無だけ）。
- ホストへ渡す環境変数は `servo_bbox_driver.build_host_env` の許可リストのみ（`SEC-2`）。UA・ヘッダには触らない。

## 限界

- `dns` / `udp_direct` はハーネス自身のプローブで、Servo 自身の DNS・UDP 送信の外部観測は xos-measure の
  `egress-host-ubuntu` の host-capture（#1461。次節）が担う（root が要るため使い捨てランナーに限る）。
- 送信先の監視は `monitor.py`（`egress_guard` の走査関数を流用し、ハーネスが意図的に接続させるダミー宛先を除外して継続する）。接続済みソケットを 25 ms 間隔で走査するため短命な接続は拾えず、経路ごとに走査が 1 回以上完了していなければ合格にしない。補助の観測手段。
- `dns` は `/etc/resolv.conf` の実体位置に固定の非ループバック nameserver（192.0.2.53）を重ね、その宛先への UDP connect が経路なしで失敗したときだけ試行を観測した扱いにする（`/run` の隠蔽による resolv.conf 欠落と netns 由来の失敗を区別する。区別できなければ unobservable）。docker-none は resolv.conf を差し替えないため、nameserver が得られなければ unobservable になる。
- docker-none の不採用案: render-screenshot のイメージの流用（Chromium の導入で遅く無関係な依存が入る）、
  測定機の OS にそろえたベースイメージへのローカルビルドの COPY（配布コンテナと同じ glibc という目的から外れる）。
- AppArmor の `apparmor_restrict_unprivileged_userns=1` の環境では `unshare -rn` が使えず、bwrap（setuid 相当の
  プロファイル）経由でのみ netns を作れる。ns 内に CAP_NET_ADMIN は無く、veth は作れない。
- 内側の `/etc/hosts` は bind マウント（docker は `--add-host`）。Servo が参照することはベースラインの受信で確認する。

## proxy-relay（netns 内の中継 → UDS → CONNECT プロキシ。#1431）

設計 §8.1 は Linux の方式を B（bwrap `--unshare-net`）とし、外への通信は本体の許可リスト付き CONNECT プロキシだけを通す想定にした。
`--unshare-net` の中からは親の loopback に届かないため、「netns 内のクライアント → netns 内 `127.0.0.1:R`（中継）→ bind マウントした
UDS → 親の CONNECT プロキシ → 宛先」の構成が成り立つかを、**Servo を使わず**に `relay_probe.py`（単独のプローブ。`measure.py` とは独立。
結果 JSON の `mode` は `proxy-relay`）で測る。

```bash
python3 -I -B harness/egress-isolation/relay_probe.py --out "$(mktemp -d)"   # 要 bwrap・Linux のみ
```

5 項目（判定は `judge_relay`。pass / fail / unobservable の fail-closed）:

| 項目 | 内容 |
| ---- | ---- |
| `lo` | `--unshare-net` の中で `lo` に bind・listen し、同じ netns から往復できるか |
| `uds` | 親で listen する UDS を `--bind`（rw）と `--ro-bind`（ro）で `--tmpfs /run` の上に見せ、中から両方 connect できるか |
| `relay` | 中継を通して `CONNECT` → `200` → nonce の往復まで届くか（陽性対照。外側 listener の受信とプロキシの `allowed` も要る） |
| `direct` | 中継を通さない直接 connect が netns で失敗するか（陰性対照。ベースライン〔`--unshare-net` なし〕で届くことが前提。プロキシ経由でない外側の受信が 0 件） |
| `ip_recheck` | 許可リストに載った 127.0.0.2 への CONNECT を、IP の再検査（`ip_denied`）で 403 にできるか（拒否理由が `ip_denied` でなければ unobservable） |

- テスト用プロキシのポリシーは**測定用のフィクスチャ**で、本番のプロキシではない。本番のポリシーは loopback を含む内部アドレスをすべて拒否するはずで、
  ここでは 127.0.0.4 を許可先の代役にしているにすぎない。評価順は 127/8 外の拒否（安全柵）→ IP の再検査 → 許可リスト。authority は IPv4 リテラルのみ（名前解決しない）。
- 本番のプロキシの実装・`servo-host` への注入・HostProcess への bwrap の組み込みは設計 §7 の承認事項のまま。ここで確かめたのは構成の成立性だけ。
- 範囲外: 名前解決後の再検査（DNS rebinding）、CONNECT 上の TLS、Servo 0.7.0 の `Tunnel` との実接続。
- 安全上の規則: 宛先は 127.0.0.0/8 のダミーのみ（プロキシは 127/8 外へ connect しない）。UDS は 0700 のディレクトリ内。ヘッダ長・同時接続・転送量・アイドル時間に上限。結果に nonce・要求本文は残さない。自己テストは bwrap を使わない（`make check-egress-isolation`）。
- bwrap はホストの FS を `--ro-bind / /`＋`--dev /dev` で読み取り専用に見せる（読み取りは制限せず、制限するのは書き込みだけ）。このプローブが測るのはネットワーク（netns・中継・プロキシ）で、FS の隔離は測定の対象外。sandbox の中で書き込めるのは tmpfs の `/run`・UDS ディレクトリ・`out/inner/` だけ（#1433・#1442。以前は `--dev-bind / /` でホストの FS 全体に、その後は `out/` 全体に書けた）。出力は `result.json`・`summary.md` が `out/` 直下、inner の `baseline.json`・`netns.json` が `out/inner/`。接続ごとに総時間 10 秒の期限を持ち、少しずつ送り続ける接続が同時接続の枠を占有し続けないようにしている。
- `direct`（陰性対照）は「netns 内の `127.x` は netns 自身の `lo` に向かう」性質に頼る測定で、外へ出る経路の遮断そのものは設計 §6.4 の実測が担う。

### 実測結果（2026-11-02。コミット 06d4ba30）

| 項目 | 値 |
| ---- | -- |
| カーネル | 7.0.0-34-generic（x86_64） |
| bwrap | bubblewrap 0.11.1 |
| AppArmor | `kernel.apparmor_restrict_unprivileged_userns=1`（制限あり）。bwrap 経由で netns を作れた |

| 項目 | 結果 | 観測 |
| ---- | ---- | ---- |
| `lo` | pass | netns 内の 127.0.0.1 で bind・listen・往復に成功 |
| `uds` | pass | rw・ro の両マウントに connect でき、プロキシが 2 接続を数えた |
| `relay` | pass | 200・nonce 16 バイトが一致・外側 listener の受信 1 件 |
| `direct` | pass | netns の直接 connect は `ECONNREFUSED`（ベースラインでは届いた）。プロキシ経由でない受信 0 件 |
| `ip_recheck` | pass | 127.0.0.2 は 403（`ip_denied` 1 件）。内部 listener の受信 0 件（ベースラインでは 1 件） |

結論: この環境では構成が成り立つ。他の環境（AppArmor の制限が無い環境・6.7 未満のカーネル）は未測定。

## macOS・Windows（#1387）

`xos-measure.yml` の suite `egress-isolation`（`gh workflow run xos-measure.yml -f suite=egress-isolation`。
macOS・Windows の GitHub ホステッドランナー。結果は #1003 に記録する）か、手元の実機で `measure.py` を実行する。
判定語彙（`pass` / `fail` / `unobservable`）と fail-closed は Linux と同じ。OS ごとのモードは `judge.PLATFORM_MODES`。

| OS | モード | 内容 |
| -- | ------ | ---- |
| macOS | `baseline` / `seatbelt` | baseline は `sandbox-exec -p "(version 1)(allow default)"`（隔離と無関係な失敗の切り分け）。`seatbelt` は `(deny network-outbound (remote ip))` ＋ページ配信用 loopback 1 ポートの許可（`seatbelt.py`） |
| Windows | `baseline` / `appcontainer` | baseline は直接起動。`appcontainer` は capability なしの AppContainer で内側ランナーごと起動する（`appcontainer.py`。#1396。下記） |

観測手段が Linux と異なる点（`method` 列にも出る）:

- プロセス単位の監視（`egress_guard`）が無い。pass の条件から外す代わりに、外側 listener の受信 0 件・baseline の到達・
  試行の観測・`--selftest` と loopback の `load` の成功をすべて要する。
- Windows では Servo ホストが結果の出力後に Servo の後始末を通さず `process::exit` で終わる（AppContainer 内の ROUTER の panic 回避。#1413）。
  この修正後は `selftest_rc`・`host_exit_rc` が 0 になる想定で、マージ後の xos-measure で確認する。
- 異常終了の診断（#1413）。内側の結果 JSON に次を残す。`selftest_rc` が 0 以外のときだけ `selftest_stderr_tail`
  （`--selftest` の stderr の末尾。段階マーカー `selftest: <stage>` と panic メッセージ）、`shutdown` 後にホストを待って
  `host_exit_rc`（待ちが時間切れなら `host_exit_timed_out: true`）、0 以外のときだけ `host_stderr_tail`。どれも上限つきで、
  URL・環境変数の値は載せない。Windows の `0xC0000409`（3221226505）は fail-fast の終了コードで、スタック溢れだけでなく
  Rust の `panic = "abort"` の panic も同じコードになる。判定（`positive_ok`）には使わない。
- 試行の観測（#1395）。macOS・Windows の TCP 経路の試行は次の順で根拠にする。いずれも ip:port と件数だけを記録する。
  - 失敗経路のビーコン（補助情報のみ。img・fetch・ws・internal）: 同じページが、許可外宛先への要求が失敗したとき
    （`onerror` / `catch`）だけ許可オリジンへ `/beacon/<route>` を送り、結果の `beacons` に件数を記録する。
    ブラウザ内部が要求を拒否した場合も発火するため、接続 syscall の試行や Seatbelt による遮断の証拠にならず、
    `attempts` の根拠にも判定にも使わない（ビーコンだけでは `unobservable`）。Linux のページは変えない。
  - サンドボックス拒否ログ（TCP 経路の唯一の根拠。`log show`）: 取りこぼしの原因を切り分けるため、診断カウンタ（読んだ行・JSON 行・拒否行・
    宛先を取れた行・pid の帰属で除外した行・帰属した行。`deny_log.diagnostics`）を記録する。あわせて形式の確認用に、対象 pid に帰属する `deny(` 行で正規表現に一致しなかったものの「形」だけを最大 16 件（プロセス名・pid・パス・外部 IP・クエリは `<proc>`・`<pid>`・`<path>`・`<ipv4>`・`<redacted>` に置換。127.0.0.0/8 の宛先は実値）と、pid 帰属で除外した行の `{pid, dest}` を最大 8 件（本文は保存しない。127.0.0.0/8 以外の dest は `<ipv4>:port`）記録する。無関係なプロセスの不一致行は件数（`unmatched_deny_unattributed`）だけ。内側ランナー自身の UDP
    プローブが拒否されたのに拒否ログに無いとき、または隔離モードで予熱・診断経路の外側受信が 1 以上のとき（#1422。理由に経路名と件数。複数の理由は連結する）は、自己検査が不成立（`deny_log.available=false`）となり、拒否ログを根拠にしない。
    macOS 26 の拒否ログは宛先アドレスを `*` で出す形式（`deny(1) network-outbound <語>:*:<port>`。実測 xos-measure run 41305443526）もある。
    既存の IPv4 形式は変えず、この形式は「pid が対象 pid に含まれ、かつポートがこの run の既知の宛先ポート集合で一意」のときだけその宛先へ帰属させる
    （IPv4 形式より厳しい結合条件。判定ロジックは変えない）。既知集合に無いポートは `wildcard_unknown_port`、複数の宛先が共有するポートは帰属させず
    （unobservable 側）`wildcard_ambiguous_port`・`wildcard_ambiguous_ports` に残す。宛先ごとの一致形式は `deny_log.diagnostics.match_kind`
    （`ip:port` / `*:port` の件数）、この形式で観測した経路は `deny_log.wildcard_routes` と verdict の `method` に出る。
    形のサンプルは `network-outbound` 直後の宛先トークン末尾のポート数字（1〜5 桁）と `*` を保持する（ポートは記録範囲の ip:port の内）。英字語は `a` 形のまま。
  - 拒否ログが使えない（自己検査の不成立・取りこぼし）TCP 経路は、ビーコンがあっても `unobservable`（理由は結果の `reason`）。
    したがって「Servo の事前ブロックではなく遮断された」ことは拒否ログの試行でのみ示す。
  - `udp_direct` は `sendto` が拒否された errno。
  - Servo ホスト側の接続試行ログ（`egress_guard` 相当）は採らない。`egress_guard` は `/proc/net` を走査する Linux 専用で、
    macOS では connect が syscall で拒否されソケットが現れない。`servo-host/src` に接続試行のログも無く、追加には Servo の
    net スタックの改変か servo-host の改修が要る。
  - macOS は sudo なしの `log show --style ndjson`（非特権）で拒否行を取る（#1402。xos-measure の実測 run 41309744793・41310673642・41313641135 で、
    sudo・`log config` なしでも上記 macOS 26 のワイルドカード形式の拒否行が取れ、TCP 4 経路（img・fetch・ws・internal）が pass・自己検査成立。redirect は予熱つきの実測（#1415。run 41401095620・41402439545）で pass になった（下の予熱の節の結果）。オーナー判断 2026-11-01 で
    sudo つき拒否ログの opt-in と `log config` の操作を削除した）。UDP の自己照合が成り立てば TCP 5 経路の deny 行が試行の根拠になり、
    成り立たなければ理由（`deny log unavailable: ...`）つきの `unobservable`。`log stream`・`log erase` など管理者操作は実装しない。
  - 後続の候補（今回は入れない）: SBPL の `(with message ...)` 等の修飾子、`log stream` の並行収集。
  - redirect の診断（#1415。macOS だけ・判定には使わない）。macOS 26 の実測（run 41326087766）で redirect だけ拒否行が 0 件の
    `unobservable` になる。redirect は唯一のトップレベルナビゲーションで、servo-host の `load_failed` は複数の原因（エラーページ・
    final_url 欠落・許可規則外・Crashed）を 1 種別にまとめるため、「connect を試みていない」のか「拒否されたが行が出ていない」のかを
    今の結果では区別できない。切り分けのため、判定経路の後に診断を足す。verdict・`attempts`・`positive_ok`・
    `judge.routes_for_mode` は変えず、redirect が `unobservable` のときだけ理由の末尾に事実を足す。
    - D1 `diag_redirect_late`: 末尾でもう一度 302（専用ポート・`/redirect-late`）を試す。先頭であることによる間引きの確認。
    - D2 `diag_nav_direct`: 302 を経ず `127.0.0.2:<専用ポート>/direct` をトップレベルで直接 load する。302 の追従の確認。
    - D3: 全 `load` の所要時間（ms）と、失敗直後のホスト生存（`proc.poll()`）。すぐ失敗なら connect 拒否かエラーページ、約 8 秒なら時間切れ。
    - D4: 帰属できなかった `deny(` 行の内訳（`diagnostics.unattributed_*`）。記録するのは pid の有無・既知の操作名（それ以外は `<op>`・
      種類は 16 まで）・既知で一意なポートと一致した宛先の件数だけ。未知ポートは数字を残さず件数のみ。本文・pid・プロセス名・パスは残さない。
    - D5: 本来の収集で redirect の宛先が 0 件のときだけ、`log show --info --debug`（述語は同じ固定文字列）で読み直し、既知の宛先の件数だけを
      `deny_log.diagnostic_info_debug_dest_counts` に残す。根拠にはしない。
    - 結果の読み方: D2 に拒否行があり redirect に無ければ 302 の追従が connect に達していない。どちらも無ければトップレベルナビゲーションの
      接続がこの形式では報告されない。D1 だけにあれば順序の影響（後続で経路順の変更を検討する）。隔離モードで診断経路の外側に受信があれば
      WARNING を出し、拒否ログの自己検査を不成立にする（fail-closed。#1422。理由に経路名と件数。baseline は対象外。
      `judge.py` の判定基準は変えない。`udp_direct` は errno と外側の UDP listener で判定するため影響しない）。診断経路は Linux・Windows では有効にしない。
  - redirect の予熱（#1415。macOS だけ・判定には使わない）。診断（run 41338564798）で、拒否行が取れないのは「最初に読み込む判定経路」
    だけで、同じ形の 302 を後ろで試す D1 には 1 件出ることが分かった（順序への依存）。オーナー判断 2026-11-02 の案 A で予熱を 1 回入れたが、
    それでも redirect と img が `unobservable` のまま残り（run 41382057223・41386861681。予熱なしの比較は run 41382060305）、
    取りこぼしは先頭の 1 件ではなく先頭の 1〜3 件で、何件かはランごとに違った。`log show` の取得開始は測定開始の 2 秒前で、取得範囲は原因ではない。
    そこでオーナー判断 2026-11-02（案 A'）で、判定基準を変えずに「件数によるか時間によるか」を測れるようにした。
    - 順序: `/ok`（許可宛先。陽性対照）→ 予熱 K 回（既定 5。`--warmup-count`、0〜10）→ 待ち（`--warmup-wait-ms`、0〜30000。既定 0）→
      判定経路 → D1・D2。各回は「遮断ダミー宛先 `127.0.0.2:<回ごとの専用ポート>` へのトップレベル 302（`/redirect-warmup-<i>`）」で、
      拒否行をどの回のものか帰属できる（経路名 `warmup_redirect_<i>`）。待ちは最後の予熱の後に 1 回だけ入れる（K=0 でも W>0 なら入る）。
    - 集計から外す: 予熱は verdict・`attempts`・`loads`・`guard_polls`・`outer_counts`・`deny_log.dest_counts` に入れない。
      結果の `inner.warmup_loads`（経路名 -> 結果）・`warmup`（`count`・合計の外側受信 `outer`・拒否行 `deny`・回ごとの `per_route`。
      拒否行は観測値で根拠にしない）に別に残す。baseline と seatbelt で同じ順序にする（両方に付ける）。
      隔離モードで予熱のどれかの回で外側受信が 1 以上なら WARNING を出し、診断経路と同じく拒否ログの自己検査を不成立にする（fail-closed。#1422。理由に受信のあった経路名と件数。受信 0・`per_route` が空は不成立にしない。baseline は対象外）。
    - 時系列: 各読み込み（`/ok`・予熱・判定経路・D1/D2）の `route`・`role`（loopback / warmup / judged / diag）・ホスト起動からの開始 ms
      （`start_ms`）・所要 ms・読み込み種別・外側受信 `outer`・拒否行 `deny` を `load_timeline`（結果 JSON と Step Summary の表）に残す。
      `deny_log.first_deny`（`index`: 遮断読み込みの何件目か、`route`、`start_ms`、`missed_before`: それ以前に拒否行が 0 件だった遮断読み込みの数）で、
      拒否行が出始める位置を 1 か所で比べる。int と固定語彙だけで、URL・拒否行の本文は載せない。
    - 読み方: `first_deny.index`（件数）がランをまたいでそろえば件数によるため、K を増やして確定させる。`start_ms`（時間）がそろえば時間によるため、
      `--warmup-wait-ms` で待ちを足す。切り分けの例: `K=5・W=0`（既定）、`K=1・W=5000`、`K=0・W=10000`。
    - 結果（#1415・xos-measure の suite `egress-isolation`・macOS 26。予熱は判定・集計に入れない）: 既定の予熱 5 回で 2 回実測し、既定は 5 回で確定する。
      run 41401095620（main 653c5ebb）と run 41402439545（main 02dba6fc）の両方で、予熱 1〜3 回目の拒否行は 0 件、4・5 回目が各 1 件だった。
      拒否行が出始めたのは 4 回目で、開始からそれぞれ 8601 ms と 8548 ms。両方とも redirect・img・fetch・ws・internal・udp_direct が pass で、
      自己検査も成立した（予熱・診断経路の外側受信は 0 件）。
    - 未確定: 各読み込みは約 2.8 秒おきに始まるため、取りこぼしが件数（先頭 3 件）によるのか時間（約 8.5 秒まで）によるのかは、この 2 回では区別できていない。
      どちらにしても、既定の予熱 5 回で判定経路はその取りこぼしの範囲の外に出る。切り分けの例（`K=1・W=5000` 等）は未実施。
    - 起動: xos-measure の入力 `egress_warmup_count`・`egress_warmup_wait_ms`（macOS の suite のみ。文字列で受け、シェルで正規表現と範囲を検証してから渡す）。
      `measure.py` は macOS 以外で指定すると exit 2。上限は資源の枯渇とジョブの超過を防ぐためのもの。
    - macOS だけ有効（`--diag-routes` と同じゲート）。Linux・Windows には導入しない。判定基準（`judge.py`）・管理者操作の不使用は変えない。
    - 次の手（servo-host から `load_failed` の詳細を返す案 B・既知の制約として記録する案 C）はオーナー判断で、ここでは実装しない。
- 共有 loopback で内側の写し listener を立てられない。hosts も変えないため、ページは IP リテラル（127.0.0.x）で配る。
- `internal_real` / `dns` は netns 前提の構造検査で、Seatbelt・AppContainer では成り立たないため `unobservable`
  （実在の内部アドレスにも向けない）。
- macOS は lo0 に 127.0.0.2 / 127.0.0.3 の alias が要る（`sudo ifconfig lo0 alias 127.0.0.2 up`）。ハーネスは sudo を使わず、
  bind できなければ exit 2 で案内する。workflow は使い捨て runner で固定 argv の alias 追加を行う。
- Windows の `appcontainer`（#1396。オーナー承認 2026-11-01: `harness/egress-isolation/` の Python 計測ハーネスに限る ctypes の Win32 FFI）:
  - `appcontainer.py` が `CreateAppContainerProfile`・`PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`・`CreateProcessW`・Job
    （`KILL_ON_JOB_CLOSE`）で内側ランナー（`in_sandbox.py`。Servo ホストは子プロセス）を起動する。標準ライブラリの ctypes のみで、
    本体 crate（Rust）への FFI・`unsafe`・依存の追加は行わない。管理者権限が要る操作は、次項の #1401 の承認範囲（`xos-measure.yml` の Windows ジョブでの opt-in）を除き行わない。
  - 容器から読めるのは AppContainer 専用フォルダだけなので、Python ランタイム・ハーネス・ホスト（exe と同階層の DLL）をそこへ複製する
    （toolcache・チェックアウト・`D:\sr` の ACL は変えない）。専用フォルダが取れないときだけ一時ディレクトリに `icacls` で SID を付与し、
    終了時に除去する。複製したホストの SHA256 を記録し、baseline と同一性を比べる。
  - プロファイル・ハンドル・属性リスト・SID・複製先は成功・失敗・タイムアウトのいずれでも `try/finally` で片付け、各段の成否を
    結果の `appcontainer.cleanup` に記録する。片付けが不完全なら pass にしない。環境変数は許可リスト（`SystemRoot`・`TMP`・`TEMP`・
    `PYTHONIOENCODING`）のみ。
  - loopback の切り分け: AppContainer は既定で loopback に出られない。外側 listener の受信 0 件が egress 遮断か loopback 遮断かを区別するため、
    Servo を使わない preflight（`appcontainer_probe.py`）を capability なし（`nocap`）とネットワーク capability あり（`netcap`:
    `internetClient`＋`privateNetworkClientServer`）で実行し、容器内の loopback 自己接続と、preflight 専用の外側 listener（127.0.0.2）への
    到達を記録する。`netcap` でも外側に届かなければ loopback 遮断と egress 遮断を区別できず、listener 経路は理由つきの `unobservable`
    （`judge.appcontainer_confound`）。preflight が無い・不正な場合も同じ（fail-closed）。
  - 宛先は 127.0.0.0/8 のダミーだけ。TCP 5 経路は試行の根拠（写し listener・拒否ログ）が無いため `unobservable` のまま。`udp_direct` の
    errno は生の値を `probes.udp_direct` に記録するだけで、Windows の拒否 errno は実測前に判定へ入れない
    （`in_sandbox.APPCONTAINER_DENY_ERRNOS` は空。観測した値を根拠に追加する）。
  - ループバック免除（#1401。オーナー承認 2026-11-01: `xos-measure.yml` の Windows ジョブ〔使い捨て runner〕に限り、ハーネスが作った
    AppContainer の SID だけに `CheckNetIsolation LoopbackExempt` を付ける。opt-in で、計測後に必ず解除）: `measure.py --appcontainer-loopback-exempt`
    （Windows の `appcontainer` モードのときだけ受け付ける。既定・ローカル・`ci.yml` では呼ばない）。プロファイル作成の直後・最初の起動の前に
    付与し、片付けではプロファイル削除より前に解除する（付与に失敗しても解除を試みる）。解除の成否と、解除後に一覧へ SID が無いことを
    `appcontainer.cleanup` の `loopback_exempt_removed`・`loopback_exempt_absent_after_remove` に記録し、解除に失敗すれば pass にしない。
    交絡の判定は 3 通り: (1) `nocap` が外側に届いた場合は loopback 宛ての listener では egress を測れないとして listener 経路を理由つきの
    `unobservable`（外側の受信由来の fail も `unobservable` に変え、件数は `outer_count` に残す）、(2) `nocap` 0 件・`netcap` 1 件以上なら交絡なし、
    (3) 両方 0 件なら免除ありでも区別できず `unobservable`。
  - 免除で測れない場合の代替案（未検証。採否はオーナー判断）:

    | 案 | 内容 | 懸念 |
    | -- | ---- | ---- |
    | (a) runner 自身の NIC アドレス | loopback 以外の自機アドレスの listener | 宛先を 127.0.0.0/8 以外に向けない安全規則に反する。隔離が自機の全アドレスに及ぶ可能性も未確認 |
    | (b) 別ホスト・別 VM の listener | ジョブ間のネットワーク設計が要る | 実在アドレスへのプローブになる |
    | (c) WFP の監査（`auditpol` の Filtering Platform Connection 等） | 試行そのものを観測する | 本承認の範囲外の管理者操作 |

- Windows の runner は管理者で動くため、「非管理者で loopback 例外を作れるか」には答えられない（設計 §6.2）。

## host-capture（#1461。隔離下の実ホストの DNS・UDP・直接 TCP 送信の外部観測）

`xos-measure.yml` の `egress-host-ubuntu`（`render3_egress_*` の前後に `host-capture start / stop / judge`）で、テスト中の
host netns を記録し、bwrap 配下の Servo ホストに帰属する外向き送信が 0 件かを判定する（`host_capture.py`・`RENDER-3`・`SEC-2`）。
合否は Step Summary とアーティファクト（`host-capture/verdict.json`・`summary.md`・`samples.jsonl`）に出し、最終判断は人間が行う。

- 権限: sudo は tcpdump の導入・起動・停止だけ（`-Z` で実行ユーザーへ即座に権限を落とし、`-w -` で runner のシェルが書く）。
  サンプラーと判定は非特権。sysctl・ルーティング・nftables・ruleset は変えない。承認は #1457 のコメント（オーナー承認 2026-11-03）で、使い捨てランナー限定。
- 記録: `tcpdump -i any -nn -U -s 256 -c 300000 "udp or tcp"`。ペイロードの大半を落とす。pcap と変換テキストはランナー自身の通信ヘッダを含むためアーティファクトに載せず削除する。
- 帰属: サンプラー（`host_capture.py sample`）が 200 ms ごとに Servo ホスト pid（`/proc/<pid>/exe` の実体が `$FANDHE_BROWSER_RENDER_HOST` と一致）、
  その netns、host netns のソケット所有 pid（同一ユーザーの `/proc/<pid>/fd`）を記録する。パケットの送信元（proto・port）を時刻が近い所有者へ突き合わせる。
  隔離は「bwrap の祖先がいる」または「netns が host と異なる」。bwrap の祖先がいて netns が host と同じなら隔離破れで fail。
- 判定: 隔離下の Servo に帰属する DNS・UDP・直接 TCP SYN が 1 件でもあれば fail。隔離外 Servo（WebSocket の対照）・自身・他プロセス・帰属不能は件数を記録するだけ。
  対象は非 `lo` の Out（UDP 全ポート・TCP SYN）と `lo` の Out・In の宛先 53 番（lo は libpcap が送信側重複を除外し In で記録される）（systemd-resolved スタブ経由を拾う）。
- 観測不能は合格にしない（unobservable）: カナリア（サンプラーが 127.0.0.1 の UDP で自分宛てに送る陽性対照）が `lo` の記録に現れて自分に帰属しない、
  隔離下の Servo pid を一度も観測しない、サンプル 0、外向きパケット 0、tcpdump の異常終了・pcap 変換の失敗・カーネルドロップの検出または統計の欠落、上限による打ち切り、解析不能行の超過。
- 限界: サンプル間隔より短命なソケット・root のプロセス（systemd-resolved 等）は帰属できない。隔離下のホストの ns 内の `lo` で閉じる問い合わせは host に届かないため観測対象外。
  テストのページは IP リテラルで、宛先の DNS を誘発しない（Servo が自発的に出す送信だけを見る）。fd 受け渡しによる netns 越えのソケットは想定外。ubuntu のみ。
- 自己テスト: `test_host_capture.py`（合成 tcpdump テキスト・合成サンプル。`make check-egress-isolation`）。行形式は tcpdump 4.99.6（SLL2）の実出力に合わせてある。

## 承認事項（この issue では実装しない）

Landlock ルールの適用（`landlock` クレートまたは `unsafe`）・seccomp-bpf・許可リスト付きプロキシ（本番の実装。netns 内の中継の成立性だけは #1431 の proxy-relay で確認済み）・
Windows の TCP 経路の試行の観測手段（WFP の監査等）・古いカーネルでの再測定。
（`CheckNetIsolation LoopbackExempt` は #1401 で承認済みの opt-in〔xos-measure の Windows のみ〕として実装済み。Windows の AppContainer ランチャー〔ctypes〕は #1396 でオーナー承認のうえ実装済み。範囲はこのディレクトリの Python ハーネスに限る。）
