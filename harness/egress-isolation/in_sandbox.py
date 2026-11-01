#!/usr/bin/env python3
"""サンドボックスの内側で Servo ホストを駆動し、5 経路の試行を行う内側ランナー。

役割・責務境界:
    Issue #1377（TASK-38・`RENDER-3`・`SEC-2`・`XOS-1`）。`measure.py` が bwrap（`--unshare-net` の有無）または
    `docker run --network none`（Windows の `appcontainer` は `appcontainer.py` の AppContainer、macOS の `seatbelt` は
    Seatbelt）の中で起動する。許可オリジン（pages.test -> 127.0.0.1）を自前で配信し、
    ホスト（`fandhe-browser-servo-host --stdio`）へ経路ごとの `load` を送り、(iv)(v) のプローブを行い、
    結果を `--out` の JSON に書く。外側の listener の受信件数は `measure.py` が数える（ここでは数えない）。

呼び出し文脈:
    - 隔離モードでは実行前に `/proc/net/*` で lo 以外の経路が無いことを確かめ、満たさなければ何も読み込まず
      exit 2 で中止する（実在の内部アドレスを含むページを読み込む前の安全側の検査）。
    - 隔離モードでは外側と同じ宛先（127.0.0.2:<port> 等）の写し listener を内側にも立て、ページが実際に
      接続を試みたこと（試行の観測）を確かめる。ベースラインは外側の listener が直接受ける。
    - 同ディレクトリの `render-screenshot` の `servo_bbox_driver`（HostChannel 等）を流用し、送信先の監視は
      `monitor.DestinationMonitor`（既知のダミー宛先を除外して監視を継続する。`EgressGuard` は最初の試行で止まるため使わない）。

信頼境界: 受け取る引数は検証済みの固定集合・int ポート・実行可能ファイルの絶対パスのみ。ホストに渡す環境変数は
`build_host_env` の許可リスト（`*_PROXY`・`LD_*` は渡さない。`SEC-2`）。UA・ヘッダ・ビューポートは触らない。

終了コード: 0（結果 JSON を書いた。経路判定は含まない）／2（引数・前提検査の不成立）。
"""

from __future__ import annotations

import argparse
import errno
import getpass
import http.server
import json
import os
import re
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any

_HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(_HERE))
sys.path.insert(0, str(_HERE.parent / "render-screenshot"))
import monitor  # noqa: E402
import page_templates as fixtures  # noqa: E402
import judge  # noqa: E402
import listeners  # noqa: E402
import quiet_io  # noqa: E402
import seatbelt  # noqa: E402
import servo_bbox_driver as sbd  # noqa: E402

LOAD_TIMEOUT_MS = 8000
SETTLE_S = 2.5
PROBE_TIMEOUT_S = 2.0
DNS_TIMEOUT_S = 5.0
TCP_ROUTES = ("redirect", "img", "fetch", "ws", "internal")
MAX_BEACON_COUNT = 1000
PAGE_ROUTES = ("redirect", "img", "fetch", "ws", "internal", "internal_real", "dns")


class _PageHandler(http.server.BaseHTTPRequestHandler):
    """固定の辞書から HTML を返す（ディスクを読まない）。クエリは無視し、GET/HEAD 以外は 405。"""

    pages: dict[str, str] = {}
    redirect_to = ""
    timeout = 10.0
    # 失敗経路のビーコンの件数（固定キーのみ・上限つき。Issue #1395）。
    beacons: dict[str, int] = {}
    beacon_lock = threading.Lock()

    def _path(self) -> str:
        return self.path.split("?", 1)[0].split("#", 1)[0]

    def do_GET(self) -> None:  # noqa: N802
        path = self._path()
        if path == fixtures.REDIRECT_PATH:
            self.send_response(302)
            self.send_header("Location", self.redirect_to)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if path.startswith(fixtures.BEACON_PREFIX):
            name = path[len(fixtures.BEACON_PREFIX):]
            if name not in fixtures.BEACON_ROUTES:  # 固定集合との完全一致のみ（未知・パス走査風は 404）
                self.send_error(404)
                return
            with self.beacon_lock:
                self.beacons[name] = min(self.beacons.get(name, 0) + 1, MAX_BEACON_COUNT)
            self.send_response(204)
            self.end_headers()
            return
        body = self.pages.get(path)
        if body is None:
            self.send_error(404)
            return
        data = body.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _no(self) -> None:
        self.send_response(405)
        self.send_header("Content-Length", "0")
        self.end_headers()

    do_POST = do_PUT = do_DELETE = do_PATCH = do_HEAD = do_OPTIONS = _no  # noqa: N815

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002
        return  # URL（クエリ）をログへ出さない


def read_proc(name: str) -> str | None:
    """`/proc/net/<name>` を読む。読めなければ None（空文字列と区別し、前提検査を fail-closed にする）。"""
    try:
        return Path("/proc/net", name).read_text(encoding="ascii", errors="replace")[:65536]
    except OSError:
        return None


def host_argv(host: str) -> list[str]:
    """ホストの起動 argv。`.py` は Windows の CreateProcess が shebang を解釈しないため同じ interpreter 経由で起動する。

    実ホストは実行可能バイナリ（`.py` ではない）。`.py` は自己テストの偽ホスト用。
    """
    return [sys.executable, host] if host.endswith(".py") else [host]


APPCONTAINER_FLAG_ENV = "FANDHE_EGRESS_APPCONTAINER"  # appcontainer.py が子へ渡すフラグ（同名・同値。Issue #1402・P2-1）
APPCONTAINER_FLAG_VALUE = "1"
ERROR_DETAIL_MAX = 1000  # 結果 JSON の error に残す例外メッセージの上限（パス全体が読める長さ。秘密情報は含まない）
MKDIR_RETRIES = 100


def make_private_workdir(prefix: str) -> Path:
    """作業ディレクトリを作る。AppContainer 経路の Windows だけ、保護 DACL を付けず親の権限を継承させる（Issue #1401・#1402）。

    Python 3.12.4 以降の `mkdtemp` / `os.mkdir(path, 0o700)` は Windows で「現在のユーザーと管理者だけ」の
    継承しない DACL を付けるため、AppContainer の SID が許可されず配下の mkdir・書き込みが拒否される。
    Windows の一時領域はコンテナ専用フォルダ（`...\\AC\\Temp`）に向け直されており、継承で十分に閉じている。
    この前提が成り立つのは appcontainer.py が環境変数 `APPCONTAINER_FLAG_ENV` を渡す経路だけ。それ以外の
    Windows は利用者の `%TEMP%` の ACL を継承しうるため、従来どおり 0o700（保護 DACL）にする。
    Linux・macOS も 0o700 の非公開ディレクトリ。名前の衝突は `exist_ok` を使わず再試行する。
    """
    inherit_dacl = (sys.platform == "win32"
                    and os.environ.get(APPCONTAINER_FLAG_ENV) == APPCONTAINER_FLAG_VALUE)
    base = Path(tempfile.gettempdir())
    for _ in range(MKDIR_RETRIES):
        path = base / f"{prefix}{secrets.token_hex(8)}"
        try:
            if inherit_dacl:
                os.mkdir(path)
            else:
                os.mkdir(path, 0o700)
        except FileExistsError:
            continue
        return path
    raise FileExistsError(errno.EEXIST, "could not create a unique work directory", str(base))


def describe_error(exc: BaseException) -> str:
    """例外型とマスク済みメッセージ（上限 `ERROR_DETAIL_MAX`）。パスの構造は残し、利用者名・SID だけ伏せる。"""
    return f"{type(exc).__name__}: {mask_identity(str(exc))[:ERROR_DETAIL_MAX]}"


# パス規則（補助・未知の名前用）。`Users` / `home` 直下の要素は、256 文字以内に後続のパス区切りがあれば
# その区切りまでの全文字（空白・引用符を含む。改行は除く）を伏せる。区切りの無い末尾要素だけ、引用符を考慮する:
# 引用符は直後が行末・空白・`),;:`・引用符・区切りのときだけ終端（メッセージを囲む引用符）で、
# それ以外（`O'Brien` の `'B`）は名前の一部として含める。長さ上限は ReDoS 対策。
_USER_PATH_RE = re.compile(
    r"(?i)((?:[A-Za-z]:)?[\\/]+(?:Users|home)[\\/]+)"
    r"(?:[^\\/\r\n]{1,256}(?=[\\/])"
    r"|(?:[^\\/\r\n'\"]|['\"](?=[^\s),;:'\"\\/])){1,256})")
_SID_RE = re.compile(r"\bS-1-\d+(?:-\d+)+\b", re.IGNORECASE)
KNOWN_NAME_MIN_LEN = 2  # 1 文字名は誤置換が多いためパス規則に任せる
KNOWN_NAME_MAX_LEN = 256


def known_user_names() -> list[str]:
    """実行環境から得られる利用者名の候補（長い順）。取得に失敗した源は黙って飛ばす（Issue #1402・P3-a）。

    `USERNAME`・`USER`・`LOGNAME`、`USERPROFILE`・`HOME` の basename、`getpass.getuser()` を使う。
    `Public` / `runner` 等の汎用名も置換対象にする（漏えい防止を優先）。
    """
    cands: list[str] = []
    for key in ("USERNAME", "USER", "LOGNAME"):
        cands.append(os.environ.get(key, ""))
    for key in ("USERPROFILE", "HOME"):
        cands.append(re.split(r"[\\/]+", os.environ.get(key, "").rstrip("\\/"))[-1])
    try:
        cands.append(getpass.getuser())
    except (OSError, KeyError, ImportError, ValueError):
        pass
    names = {c for c in cands if KNOWN_NAME_MIN_LEN <= len(c) <= KNOWN_NAME_MAX_LEN and "\0" not in c}
    return sorted(names, key=lambda n: (-len(n), n))


def mask_identity(text: str) -> str:
    """利用者名を `<user>`、SID（`S-1-...`）を `<sid>` に置換する（Issue #1402・P3-a）。

    1. 既知の利用者名（`known_user_names`）の完全一致を、そのままの形と repr の `'` → `\\'` 形の両方で長い順に置換
    2. 上記で拾えない名前向けに、`Users` / `home` 直下の要素をパス規則（`_USER_PATH_RE`）で置換

    例外型名・errno・winerror・パスの残りの構造は変えない。マスク後に上限で切るため、切断で名前が復活しない。
    """
    for name in known_user_names():
        for form in sorted({name.replace("'", "\\'"), name}, key=lambda f: -len(f)):
            text = text.replace(form, "<user>")
    return _SID_RE.sub("<sid>", _USER_PATH_RE.sub(r"\1<user>", text))


def descendant_pids(root: int) -> set[int]:
    """`root` とその子孫の pid（`ps` の 1 回のスナップショット）。取得できなければ `root` のみ。"""
    pids = {root}
    try:
        out = quiet_io.run_quiet(["ps", "-axo", "pid=,ppid="], timeout=5,
                                 capture_stdout=True).stdout.decode("utf-8", "replace")
        pairs = [tuple(int(x) for x in line.split()[:2]) for line in out.splitlines() if len(line.split()) >= 2]
    except (OSError, subprocess.SubprocessError, ValueError):
        return pids
    grew = True
    while grew:
        grew = False
        for pid, ppid in pairs:
            if ppid in pids and pid not in pids:
                pids.add(pid)
                grew = True
    return pids


def errno_name(exc: OSError) -> str:
    return errno.errorcode.get(exc.errno or 0, "oserror")


def probe_connect(family: int, addr: tuple[Any, ...]) -> str:
    """connect だけを試し（何も送らない）、結果を errno 名で返す。成功なら "connected"。

    ソケットを作れない（例: `ipv6.disable=1` の `AF_INET6` は EAFNOSUPPORT）場合は "unobservable:<errno>"。
    試行そのものが起きていないため `attempts` の根拠にならず、隔離の合格証拠として扱われない（fail-closed）。
    """
    try:
        sock = socket.socket(family, socket.SOCK_STREAM)
    except OSError as exc:
        return f"unobservable:{errno_name(exc)}"
    sock.settimeout(PROBE_TIMEOUT_S)
    try:
        sock.connect(addr)
        return "connected"
    except socket.timeout:
        return "timeout"
    except OSError as exc:
        return errno.errorcode.get(exc.errno or 0, "oserror")
    finally:
        sock.close()


def probe_udp(ip: str, port: int) -> str:
    """UDP の sendto（DNS 形式の固定バイト列）を 1 回送る。宛先は外側/内側のダミー listener。"""
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    except OSError as exc:
        return f"unobservable:{errno_name(exc)}"
    try:
        sock.sendto(b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x01\x00\x01", (ip, port))
        return "sent"
    except OSError as exc:
        return errno.errorcode.get(exc.errno or 0, "oserror")
    finally:
        sock.close()


def probe_dns() -> dict[str, Any]:
    """hosts に無い名前の解決を試み、所要時間と失敗種別を返す（隔離下専用。上流の DNS へ出うるため）。"""
    started = time.monotonic()
    result = "resolved"
    holder: list[str] = []

    def _run() -> None:
        try:
            socket.getaddrinfo(judge.HOST_NXDOMAIN, 80)
            holder.append("resolved")
        except socket.gaierror as exc:
            holder.append(f"gaierror:{exc.errno}")
        except OSError as exc:
            holder.append(errno.errorcode.get(exc.errno or 0, "oserror"))

    thread = threading.Thread(target=_run, daemon=True)
    thread.start()
    thread.join(DNS_TIMEOUT_S)
    result = holder[0] if holder else "timeout"
    return {"result": result, "elapsed_s": round(time.monotonic() - started, 2)}


RESOLVER_SOCKETS = (
    "/run/systemd/resolve/io.systemd.Resolve", "/run/systemd/resolve/resolv.conf",
    "/run/nscd/socket", "/var/run/nscd/socket", "/run/dbus/system_bus_socket",
    "/run/avahi-daemon/socket",
)


def reachable_resolver_sockets() -> list[str]:
    """ホスト側 resolver の Unix ソケットへ接続できる経路を返す（netns で遮断されない名前解決経路）。

    1 件でもあれば DNS の失敗は「送信遮断」の証拠にならない（pass にしない）。
    """
    found: list[str] = []
    if not hasattr(socket, "AF_UNIX"):
        # 古い Windows の Python には AF_UNIX が無い。Unix ソケットの resolver 経路自体が存在しない。
        return found
    for path in RESOLVER_SOCKETS:
        try:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        except OSError:
            continue
        sock.settimeout(1.0)
        try:
            sock.connect(path)
            found.append(path)
        except OSError:
            pass
        finally:
            sock.close()
    return found


def probe_nameservers() -> dict[str, Any]:
    """`/etc/resolv.conf` の nameserver 宛に UDP の connect だけを試し（送信なし）、errno 名を返す。

    名前解決の失敗が netns による遮断由来であることの根拠にする。resolv.conf が読めない・切れている
    （例: `/run` の隠蔽でシンボリックリンクが切れる）場合は nameserver が空になり、試行は観測できない扱い。
    """
    servers: list[str] = []
    try:
        text = Path("/etc/resolv.conf").read_text(encoding="ascii", errors="replace")[:4096]
    except OSError:
        text = ""
    for line in text.splitlines():
        fields = line.split()
        if len(fields) >= 2 and fields[0] == "nameserver" and len(servers) < 3:
            servers.append(fields[1][:64])
    results: dict[str, str] = {}
    for ip in servers:
        family = socket.AF_INET6 if ":" in ip else socket.AF_INET
        try:
            sock = socket.socket(family, socket.SOCK_DGRAM)
        except OSError as exc:
            results[ip] = f"unobservable:{errno_name(exc)}"
            continue
        try:
            sock.connect((ip, 53))
            results[ip] = "connected"
        except OSError as exc:
            results[ip] = errno.errorcode.get(exc.errno or 0, "oserror")
        except ValueError:
            results[ip] = "invalid"
        finally:
            sock.close()
    return {"nameservers": results}


# サンドボックス（Seatbelt 等）が送信を拒否したときの errno 名。実測で観測した値に合わせて見直す（#1387）。
SANDBOX_DENY_ERRNOS = seatbelt.SANDBOX_DENY_ERRNOS
# AppContainer（Windows）が送信を拒否したときの errno 名。実測前は空で、UDP の試行は根拠なし（unobservable）になる。
# 生の errno は `probes["udp_direct"]` に記録される。実測で観測した値を根拠に別コミットで追加する（Issue #1396）。
APPCONTAINER_DENY_ERRNOS: frozenset[str] = frozenset()
NS_UNREACHABLE = frozenset({"ENETUNREACH", "EHOSTUNREACH", "ENETDOWN", "EADDRNOTAVAIL", "EAFNOSUPPORT"})


def dns_attempt_observed(dns: dict[str, Any]) -> bool:
    """DNS の失敗が netns による遮断由来と言えるときだけ True（区別できなければ観測不能）。

    条件: 解決が失敗した・ホスト側 resolver のソケットに届かない・resolv.conf に非ループバックの nameserver が
    1 件以上あり、そのすべてへの UDP connect が「経路なし」の errno で失敗した（`/run` の隠蔽による
    resolv.conf 欠落では nameserver が空になり、この条件を満たさない）。
    """
    result = dns.get("result", "resolved")
    # 解決プローブ自体が観測不能（例外）なら、後続の nameserver 観測が成功しても試行ありとは言えない（fail-closed）。
    if result == "resolved" or str(result).startswith("unobservable:") or dns.get("resolver_sockets"):
        return False
    servers = dns.get("nameservers") or {}
    if not servers or any(ip.startswith("127.") or ip == "::1" for ip in servers):
        return False
    return all(v in NS_UNREACHABLE for v in servers.values())


def host_identity(host: str) -> dict[str, Any]:
    """サンドボックス内で実際に実行するホストバイナリの SHA256 と version（baseline との同一性判定用）。"""
    import hashlib

    ident: dict[str, Any] = {}
    try:
        ident["host_sha256"] = hashlib.sha256(Path(host).read_bytes()).hexdigest()
        ident["host_version"] = quiet_io.run_quiet(
            [host, "--version"], timeout=20, capture_stdout=True).stdout.decode("utf-8", "replace").strip()[:80]
    except (OSError, subprocess.SubprocessError):
        pass
    return ident


def validate_args(args: argparse.Namespace) -> dict[str, int]:
    if args.mode not in judge.MODES:
        raise ValueError("unknown mode")
    if args.page_port != 0 and not (1024 <= args.page_port <= 65535):
        raise ValueError("--page-port must be 0 or an int in [1024, 65535]")
    host = Path(args.host)
    if not host.is_absolute() or not host.is_file() or not os.access(host, os.X_OK):
        raise ValueError("--host must be an absolute path to an executable file")
    try:
        ports = json.loads(args.ports)
    except ValueError as exc:
        raise ValueError("--ports must be JSON") from exc
    needed = (*TCP_ROUTES, "udp_direct")
    if not isinstance(ports, dict) or any(k not in ports for k in needed):
        raise ValueError("--ports must contain every route")
    for key in needed:
        value = ports[key]
        if isinstance(value, bool) or not isinstance(value, int) or not (1024 <= value <= 65535):
            raise ValueError(f"port for {key} must be an int in [1024, 65535]")
    return {k: ports[k] for k in needed}


def load_route(chan: sbd.HostChannel, next_id: list[int], info: sbd.HostInfo, url: str) -> dict[str, Any]:
    """`load` を 1 件送り、結果の種別だけを返す（URL は載せない。final_url はホスト名のみ）。"""
    try:
        res = sbd.call(
            chan, next_id, "load", {"url": url, "timeout_ms": LOAD_TIMEOUT_MS},
            LOAD_TIMEOUT_MS / 1000 + sbd.TIMEOUT_SLACK_S, max_frame=info.response_max,
        )
    except sbd.RpcError as exc:
        return {"ok": False, "kind": exc.kind}
    except sbd.HostError as exc:
        return {"ok": False, "kind": "host_error", "detail": str(exc)[:120]}
    final = res.get("final_url")
    host = None
    if isinstance(final, str):
        try:
            from urllib.parse import urlsplit

            host = urlsplit(final).hostname
        except ValueError:
            host = None
    return {"ok": True, "final_host": host}


def run(args: argparse.Namespace, ports: dict[str, int]) -> dict[str, Any]:
    isolated = args.mode in judge.ISOLATED_MODES
    # netns を前提にする検査・写し listener・構造的な経路は bwrap / docker のモードだけ（#1387）。
    netns = args.mode in judge.NETNS_MODES
    linux = sys.platform.startswith("linux")
    literal = not linux  # macOS・Windows は hosts を変えられないので IP リテラルのページを配る
    result: dict[str, Any] = {"mode": args.mode, "routes": list(judge.routes_for_mode(args.mode))}
    result.update(host_identity(args.host))

    dev, route4, route6 = read_proc("dev"), read_proc("route"), read_proc("ipv6_route")
    if netns:
        ok, why = judge.check_isolation(dev, route4, route6)
        result["isolation_precheck"] = {"ok": ok, "reason": why}
        if not ok:
            result["aborted"] = True
            return result

    server = http.server.ThreadingHTTPServer((judge.IP_ALLOWED, args.page_port), _PageHandler)
    server.daemon_threads = True
    page_port = server.server_address[1]
    _PageHandler.pages = fixtures.build_pages(ports, literal=literal)
    _PageHandler.redirect_to = fixtures.redirect_location(ports, literal=literal)
    with _PageHandler.beacon_lock:
        _PageHandler.beacons = {}
    threading.Thread(target=server.serve_forever, daemon=True).start()

    inner: dict[str, Any] = {}
    if netns:
        for r in TCP_ROUTES:
            ip = judge.IP_INTERNAL_STANDIN if r == "internal" else judge.IP_BLOCKED
            inner[r] = listeners.TcpRecorder(r, ip, ports[r]).start()
        inner["udp_direct"] = listeners.UdpRecorder("udp_direct", judge.IP_BLOCKED, ports["udp_direct"]).start()

    workdir = make_private_workdir("egress-isolation-")
    proc: subprocess.Popen[bytes] | None = None
    stage = "setup"
    guard: monitor.DestinationMonitor | None = None
    try:
        for sub in ("home", "tmp", "xdg-config", "xdg-cache", "xdg-data"):
            (workdir / sub).mkdir()
        env = sbd.build_host_env(workdir)
        env_notes = sorted(k for k in os.environ if k.lower().endswith("_proxy"))
        result["proxy_env_present"] = bool(env_notes)  # 値は記録しない（有無のみ）

        # AppContainer は `nul` を開けないため、内側の起動は quiet_io 経由（#1401）。失敗段階は error_stage に残す。
        stage = "selftest"
        st = quiet_io.run_quiet([*host_argv(args.host), "--selftest"], env=env, timeout=90)
        result["selftest_rc"] = st.returncode

        stage = "spawn_host"
        proc = quiet_io.popen_stdio_host([*host_argv(args.host), "--stdio"], env=env)
        stage = "drive_host"
        # ハーネスが意図的に接続させるダミー宛先（127.0.0.2 / 127.0.0.3）は想定内の試行として除外し、監視を続ける。
        expected = {f"{judge.IP_INTERNAL_STANDIN if r == 'internal' else judge.IP_BLOCKED}:{ports[r]}"
                    for r in TCP_ROUTES} | {f"{judge.IP_BLOCKED}:{ports['udp_direct']}"}
        if linux:
            guard = monitor.DestinationMonitor(proc.pid, page_port, expected)
            guard.start()
        chan = sbd.HostChannel(proc)
        info = sbd.validate_hello(chan.recv(sbd.HANDSHAKE_TIMEOUT_S))  # bounding_box 必須の検査を含む
        chan.request_max = info.request_max
        next_id = [1]
        base = f"http://{judge.IP_ALLOWED if literal else judge.HOST_ALLOWED}:{page_port}"

        loopback = load_route(chan, next_id, info, f"{base}/ok")
        result["loopback_load"] = loopback
        loads: dict[str, Any] = {}
        host_pids = {os.getpid()} | descendant_pids(proc.pid)
        guard_polls: dict[str, int] = {}
        for r in (x for x in PAGE_ROUTES if x in result["routes"] and (netns or x in TCP_ROUTES)):
            polls_before = guard.snapshot_polls() if guard is not None else 0
            loads[r] = load_route(chan, next_id, info, base + fixtures.ROUTE_PATH[r])
            time.sleep(SETTLE_S)
            host_pids |= descendant_pids(proc.pid)  # 子プロセスを後から起こす場合も拾う（拒否ログの帰属用）
            result["host_pids"] = sorted(host_pids)
            guard_polls[r] = (guard.snapshot_polls() - polls_before) if guard is not None else 0  # 経路ごとに完了した走査の回数
        result["loads"] = loads
        result["guard_polls"] = guard_polls
        try:
            sbd.call(chan, next_id, "shutdown", None, sbd.SHUTDOWN_GRACE_S + sbd.TIMEOUT_SLACK_S,
                     max_frame=info.response_max)
        except (sbd.HostError, sbd.RpcError):
            pass
    except (sbd.HostError, OSError, subprocess.SubprocessError) as exc:
        result["error"] = describe_error(exc)
        result["error_stage"] = stage
    finally:
        if guard is None:
            result["guard"] = {"unavailable": f"process-level monitor not supported on {sys.platform}"}
        else:
            guard.stop()
            result["guard"] = {"violation": guard.violation, "unavailable": guard.unavailable,
                               "polls": guard.snapshot_polls(), "expected_seen": sorted(guard.expected_seen)}
        if proc is not None:
            sbd._stop_host(proc, graceful=False)
        shutil.rmtree(workdir, ignore_errors=True)

    # --- (iv)(v) のプローブ（connect だけ・固定宛先） -----------------------------------
    probes: dict[str, Any] = {}
    probe_errors: dict[str, str] = {}

    def guarded(name: str, fn: Any) -> Any:
        """想定外の例外はそのプローブだけを観測不能にし、結果 JSON の出力を妨げない。"""
        try:
            return fn()
        except Exception as exc:  # noqa: BLE001  (最後の砦。型名のみ記録)
            probe_errors[name] = type(exc).__name__
            return f"unobservable:{type(exc).__name__}"

    probes["udp_direct"] = guarded("udp_direct", lambda: probe_udp(judge.IP_BLOCKED, ports["udp_direct"]))
    if netns:
        probes["internal_real"] = {
            **{ip: guarded("internal_real", lambda ip=ip: probe_connect(socket.AF_INET, (ip, 80)))
               for ip in judge.REAL_INTERNAL_V4},
            judge.REAL_INTERNAL_V6: guarded(
                "internal_real",
                lambda: probe_connect(socket.AF_INET6, (judge.REAL_INTERNAL_V6, 80, 0, 0))),
        }
        dns = guarded("dns", probe_dns)
        probes["dns"] = dns if isinstance(dns, dict) else {"result": str(dns)}
        # 失敗時は resolver 到達不明（非空）・nameserver 空として扱い、dns の試行を観測不能に倒す。
        res_socks = guarded("dns", reachable_resolver_sockets)
        probes["dns"]["resolver_sockets"] = res_socks if isinstance(res_socks, list) else [str(res_socks)]
        ns = guarded("dns", probe_nameservers)
        probes["dns"].update(ns if isinstance(ns, dict) else {"nameservers": {}})
    if probe_errors:
        result["probe_errors"] = probe_errors
    result["probes"] = probes
    time.sleep(0.5)

    with _PageHandler.beacon_lock:
        beacons = dict(_PageHandler.beacons)
    result["beacons"] = beacons
    counts = {k: rec.count for k, rec in inner.items()}
    result["inner_counts"] = counts
    result["positive_ok"] = (
        result.get("selftest_rc") == 0 and bool(result.get("loopback_load", {}).get("ok"))
    )
    unreachable = {"ENETUNREACH", "EHOSTUNREACH", "ENETDOWN", "EADDRNOTAVAIL", "EAFNOSUPPORT"}
    real = probes.get("internal_real", {})
    result["attempts"] = {
        **{k: counts.get(k, 0) >= 1 for k in (*TCP_ROUTES, "udp_direct")},
        # 構造的な経路は「経路なし」を示す errno が出たことを試行の根拠とする。
        "internal_real": bool(real) and all(v in unreachable for v in real.values()),
        # 失敗種別だけでは送信遮断の証拠にならない。ホスト側 resolver のソケットに届く環境では試行を観測できない扱い。
        "dns": netns and dns_attempt_observed(probes.get("dns", {})),
    }
    if isolated and not netns:
        # 写し listener が無いモード: UDP はサンドボックスが sendto を拒否した errno を試行の根拠にする。
        # TCP の経路は measure.py が拒否ログ（macOS）から補う。errno は実測で観測した値（EPERM 等）を許可集合に入れる。
        deny_errnos = APPCONTAINER_DENY_ERRNOS if args.mode == "appcontainer" else SANDBOX_DENY_ERRNOS
        result["attempts"]["udp_direct"] = probes.get("udp_direct") in deny_errnos
        # TCP の試行はここでは根拠にしない。失敗ビーコン（`beacons`）はブラウザ内部の要求拒否でも発火し、接続 syscall の
        # 試行や遮断の証拠にならないため補助情報として記録するだけにする。TCP は measure.py が拒否ログ（macOS）から補い、
        # 得られなければ unobservable のままにする（fail-closed。Issue #1395）。
        evidence = {"udp_direct": "errno"} if result["attempts"]["udp_direct"] else {}
        result["attempt_evidence"] = evidence
    for rec in inner.values():
        rec.stop()
    server.shutdown()
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Inner runner for the egress isolation measurement.")
    parser.add_argument("--mode", required=True)
    parser.add_argument("--host", required=True)
    parser.add_argument("--ports", required=True)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--page-port", type=int, default=0,
                        help="Fixed port for the allowed page server (Seatbelt profile needs it before launch).")
    args = parser.parse_args(argv)
    try:
        ports = validate_args(args)
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    try:
        result = run(args, ports)
    except Exception as exc:  # noqa: BLE001  (最後の砦: 結果 JSON を必ず残し、モード全体を観測不能にする)
        result = {"mode": args.mode, "fatal": describe_error(exc),
                  "positive_ok": False, "attempts": {}}
    args.out.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 2 if result.get("aborted") else 0


if __name__ == "__main__":
    sys.exit(main())
