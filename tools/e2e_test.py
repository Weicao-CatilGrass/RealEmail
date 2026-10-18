"""RealEmail 端到端测试脚本。

覆盖以下场景：
1. 非本地收件人（例如 carol@）必须被 550 拒绝；
2. 本地账户之间投递，并为收件人创建物流任务；
3. 没有 deepseek-api-key 时任务被标记为 failed，不伪造轨迹；
4. POP3 的 USER/PASS、STAT、LIST、UIDL、TOP、RETR、DELE、QUIT 全流程；
5. 同一邮箱不允许两个 POP3 会话同时登录；
6. 发往 system 的中文主题邮件按“设置个人介绍”写入 alice.md；
7. 非本地发件人的邮件被静默忽略（250 但不投递、不建任务）；
8. 有密钥时调用 LLM 生成轨迹，并按时间同时投递到收件人和寄件人邮箱。

用法：python tools/e2e_test.py
"""

import email
import email.header
import json
import os
import poplib
import shutil
import smtplib
import socket
import subprocess
import sys
import threading
import time
from datetime import datetime, timedelta, timezone
from email.message import EmailMessage
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = ROOT / "target" / "debug" / ("realemail.exe" if os.name == "nt" else "realemail")
WORK = ROOT / "target" / "e2e"
ACCOUNTS = WORK / "accounts.toml"
SMTP_PORT = 13629
POP3_PORT = 13628
HOST = "127.0.0.1"
DOMAIN = "realemail.local"

PASSWORDS = {
    "alice": "alice-pass",
    "bob": "bob-pass",
    "system": "system-pass",
}

# 模拟 LLM 返回的三条轨迹，用相对揽收时刻的 offset-minutes。
# 服务端会强制首条不早于 lead-minutes、相邻至少 5 分钟；
# 测试里把轮询间隔和首条延迟都调小（各 1 分钟），以便很快拿到结果。
MOCK_TRACE = [
    {"courier-name": "Michael Smith", "message": "快件已揽收，运单号 SF1234567890", "offset": 1},
    {"courier-name": "Anna Brown", "message": "到达上海转运中心，进入分拣环节", "offset": 6},
    {"courier-name": "Tom Wilson", "message": "已签收，签收凭证已上传，追踪码关闭", "offset": 11},
]


class MockLLM(BaseHTTPRequestHandler):
    """极简的 DeepSeek 接口模拟，只关心 chat/completions 的返回结构。"""

    prompt = None
    auth = None
    calls = 0

    def do_POST(self) -> None:  # noqa: N802 - 遵循 http.server 的命名
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length)
        MockLLM.calls += 1
        MockLLM.auth = self.headers.get("Authorization")
        try:
            payload = json.loads(raw.decode("utf-8"))
            MockLLM.prompt = payload["messages"][0]["content"]
        except (KeyError, IndexError, json.JSONDecodeError):
            MockLLM.prompt = ""

        lines = []
        for item in MOCK_TRACE:
            lines.append(
                json.dumps(
                    {
                        "date": "",
                        "courier-name": item["courier-name"],
                        "message": item["message"],
                        "offset-minutes": item["offset"],
                    },
                    ensure_ascii=False,
                )
            )
        body = json.dumps(
            {"choices": [{"message": {"content": "\n".join(lines)}}]},
            ensure_ascii=False,
        ).encode("utf-8")

        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args) -> None:
        """静默处理请求日志。"""


def fail(message: str) -> None:
    """打印失败原因并退出，避免长脚本出现一堆 traceback。"""
    print(f"测试失败：{message}")
    log = WORK / "server.log"
    if log.exists():
        print("---- 服务端日志 ----")
        print(log.read_text(encoding="utf-8", errors="replace"))
    sys.exit(1)


def ensure_binary() -> None:
    """保证二进制存在，不存在时先构建。"""
    if BIN.exists():
        return
    print("未找到可执行文件，先执行 cargo build ...")
    if subprocess.run(["cargo", "build"], cwd=ROOT).returncode != 0:
        fail("cargo build 失败")


def prepare_workspace() -> None:
    """准备干净的测试目录，生成账户文件和个人资料。"""
    if WORK.exists():
        shutil.rmtree(WORK)
    WORK.mkdir(parents=True)

    for name, password in PASSWORDS.items():
        result = subprocess.run(
            [str(BIN), "--accounts", str(ACCOUNTS), "add", name, "-p", password],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        if result.returncode != 0:
            fail(f"添加账户 {name} 失败：{result.stderr or result.stdout}")

    write_profile("alice", "地址：北京市朝阳区望京街道")
    write_profile("bob", "地址：上海市浦东新区张江镇")


def write_profile(account: str, text: str) -> None:
    """写入 `{账户}.md`，物流任务的起点/终点就从这里读。"""
    (WORK / f"{account}.md").write_text(text, encoding="utf-8")


def write_api_key(account: str, key: str) -> None:
    """写入账户目录下的 deepseek-api-key。"""
    directory = WORK / account
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "deepseek-api-key").write_text(key, encoding="utf-8")


def start_mock_llm() -> ThreadingHTTPServer:
    """在后台线程里启动模拟 LLM 服务，返回端口由系统分配。"""
    mock = ThreadingHTTPServer(("127.0.0.1", 0), MockLLM)
    threading.Thread(target=mock.serve_forever, daemon=True).start()
    return mock


def start_server(llm_port: int) -> subprocess.Popen:
    """后台启动服务，日志写入 server.log。"""
    log = open(WORK / "server.log", "wb")
    process = subprocess.Popen(
        [
            str(BIN),
            "--accounts",
            str(ACCOUNTS),
            "serve",
            "--host",
            HOST,
            "--smtp-port",
            str(SMTP_PORT),
            "--pop3-port",
            str(POP3_PORT),
            "--domain",
            DOMAIN,
            "--courier-interval-seconds",
            "1",
            "--courier-lead-minutes",
            "1",
            "--courier-span-minutes",
            "60",
            "--llm-endpoint",
            f"http://127.0.0.1:{llm_port}/chat/completions",
        ],
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    log.close()
    for port in (SMTP_PORT, POP3_PORT):
        wait_port(port)
    return process


def wait_port(port: int, timeout: float = 20.0) -> None:
    """等待端口可连接。"""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection((HOST, port), timeout=0.3):
                return
        except OSError:
            time.sleep(0.1)
    fail(f"端口 {port} 未就绪")


def build_message(sender: str, recipient: str, subject: str, body: str) -> bytes:
    """构造一封 UTF-8 邮件（主题与正文会被自动编码）。"""
    message = EmailMessage()
    message["From"] = f"{sender}@{DOMAIN}"
    message["To"] = f"{recipient}@{DOMAIN}"
    message["Subject"] = subject
    message.set_content(body)
    return message.as_bytes()


def send_mail(sender: str, recipient: str, subject: str, body: str) -> None:
    """通过 SMTP 发一封信。"""
    with smtplib.SMTP(HOST, SMTP_PORT, timeout=10) as client:
        client.ehlo("tester")
        client.sendmail(
            f"{sender}@{DOMAIN}",
            [f"{recipient}@{DOMAIN}"],
            build_message(sender, recipient, subject, body),
        )


def mail_count(account: str) -> int:
    """统计某个账户邮箱里的邮件数量。"""
    mailbox = WORK / "data" / account
    return len(list(mailbox.glob("*.eml"))) if mailbox.exists() else 0


def wait_mail_count(account: str, expected: int, timeout: float = 15.0) -> None:
    """等待某个邮箱里的邮件数量达到预期。"""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if mail_count(account) == expected:
            return
        time.sleep(0.05)
    fail(f"邮箱 {account} 中邮件数量未达到 {expected}（当前 {mail_count(account)}）")


def wait_mail_count_ok(account: str, expected: int, timeout: float = 15.0) -> bool:
    """等待邮件数量达到预期，返回是否成功（不直接退出）。"""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if mail_count(account) == expected:
            return True
        time.sleep(0.05)
    return False


def wait_meta_status(account: str, status: str, timeout: float = 15.0) -> dict:
    """等待某个账户下出现指定状态的任务，返回该任务元数据。"""
    deadline = time.time() + timeout
    while time.time() < deadline:
        for path in sorted((WORK / account).glob("from-*.meta.json")):
            meta = json.loads(path.read_text(encoding="utf-8"))
            if meta.get("status") == status:
                return meta
        time.sleep(0.05)
    fail(f"账户 {account} 下没有出现状态为 {status} 的任务")


def test_reject_remote_recipient() -> None:
    """不在账户文件里的收件人必须被拒绝。"""
    with smtplib.SMTP(HOST, SMTP_PORT, timeout=10) as client:
        client.ehlo("tester")
        code, _ = client.mail(f"alice@{DOMAIN}")
        assert code == 250, f"MAIL 返回 {code}"
        code, text = client.rcpt(f"carol@{DOMAIN}")
        if code != 550:
            fail(f"外部收件人应返回 550，实际 {code} {text}")
        client.rset()
    print("通过：外部收件人被 550 拒绝")


def test_local_delivery() -> None:
    """alice 发给 bob 的邮件应创建物流任务，但原件要等签收后才投递。"""
    send_mail("alice", "bob", "第一封测试邮件", "你好 bob，这是正文。\n第二行。")
    # 此时还没有 deepseek-api-key，任务必须被标记为失败而不是伪造轨迹。
    meta = wait_meta_status("bob", "failed")
    if "deepseek-api-key" not in (meta.get("error") or ""):
        fail(f"失败原因不正确：{meta.get('error')}")
    if mail_count("bob") != 0:
        fail("物流未完成前不应投递原件")
    print("通过：本地投递创建任务，无密钥时标记失败且不投递原件")


def test_pop3() -> None:
    """完整的 POP3 收信流程。

    system 账户的命令邮件是立即投递的（不参与物流），适合用来验证协议本身。
    """
    send_mail("alice", "system", "设置个人介绍", "地址：测试地点")

    client = poplib.POP3(HOST, POP3_PORT, timeout=10)
    client.user("system")
    client.pass_("system-pass")

    count, _size = client.stat()
    if count != 1:
        fail(f"system 应有 1 封邮件，实际 {count}")

    if len(client.list()[1]) != 1:
        fail("LIST 返回数量不正确")

    uidl = client.uidl()
    if not uidl[1] or not uidl[1][0].split()[1].isdigit():
        fail("UIDL 返回格式不正确")

    top_text = b"\r\n".join(client.top(1, 2)[1]).decode("utf-8", "replace")
    if "Subject:" not in top_text or "Received: from tester" not in top_text:
        fail("TOP 没有返回邮件头")

    raw = b"\r\n".join(client.retr(1)[1])
    message = email.message_from_bytes(raw)
    subject = str(email.header.make_header(email.header.decode_header(message["Subject"])))
    if subject != "设置个人介绍":
        fail(f"RETR 取回的主题不正确：{subject!r}")
    body = message.get_payload(decode=True).decode("utf-8", "replace")
    if "测试地点" not in body:
        fail("RETR 没有取回正文")
    if not message["Received"] or not message["Return-Path"]:
        fail("邮件缺少 Received / Return-Path 头")

    # 登录锁：第二个会话不能同时登录同一邮箱。
    second = poplib.POP3(HOST, POP3_PORT, timeout=10)
    second.user("system")
    try:
        second.pass_("system-pass")
        fail("同一邮箱不应允许两个 POP3 会话")
    except poplib.error_proto:
        pass
    finally:
        second.close()

    client.dele(1)
    client.quit()
    wait_mail_count("system", 0)
    print("通过：POP3 全流程与邮箱互斥锁")


def test_wrong_password() -> None:
    """错误密码必须被拒绝。"""
    client = poplib.POP3(HOST, POP3_PORT, timeout=10)
    client.user("alice")
    try:
        client.pass_("wrong-password")
        fail("错误密码通过了认证")
    except poplib.error_proto:
        pass
    finally:
        client.close()
    print("通过：错误密码被拒绝")


def test_system_command() -> None:
    """发往 system 的中文主题邮件应写入 alice.md。"""
    bio = "我是 alice，喜欢写信。\n第二行介绍。"
    send_mail("alice", "system", "设置个人介绍", bio)
    target = WORK / "alice.md"
    deadline = time.time() + 5
    while time.time() < deadline and not target.exists():
        time.sleep(0.05)
    if not target.exists():
        fail("alice.md 没有被创建")
    if target.read_text(encoding="utf-8") != bio + "\n":
        fail(f"alice.md 内容不符：{target.read_text(encoding='utf-8')!r}")

    # 再发一封，验证是直接覆盖。
    send_mail("alice", "system", "设置个人介绍", "这是新的介绍。")
    time.sleep(0.5)
    if target.read_text(encoding="utf-8") != "这是新的介绍。\n":
        fail("alice.md 没有被覆盖")
    print("通过：system 账户执行“设置个人介绍”并覆盖写入")


def test_external_sender_ignored() -> None:
    """非本地发件人的邮件要被静默忽略。"""
    with smtplib.SMTP(HOST, SMTP_PORT, timeout=10) as client:
        client.ehlo("tester")
        code, _ = client.mail("stranger@example.com")
        if code != 250:
            fail(f"非本地发件人的 MAIL 应返回 250，实际 {code}")
        code, _ = client.rcpt(f"bob@{DOMAIN}")
        if code != 250:
            fail(f"RCPT 应返回 250，实际 {code}")
        code, text = client.data(build_message("stranger", "bob", "外部来信", "不应被投递"))
        if code != 250:
            fail(f"DATA 应返回 250，实际 {code} {text}")

    time.sleep(1.5)
    if mail_count("bob") != 0:
        fail("非本地发件人的邮件不应投递")
    # 前面 test_local_delivery 留下的失败任务不算数，这里只确认没有新的暂存原件。
    pending = [p for p in (WORK / "bob").glob("from-*.eml") if "外部来信" in p.read_bytes().decode("utf-8", "replace")]
    if pending:
        fail("非本地发件人的邮件不应创建物流任务")
    print("通过：非本地发件人的邮件被忽略")


def test_courier() -> None:
    """有密钥时生成物流轨迹，并按时间投递到收件人和寄件人邮箱。"""
    write_profile("alice", "地址：北京市朝阳区望京街道")
    write_profile("bob", "地址：上海市浦东新区张江镇")
    write_api_key("bob", "test-key")
    MockLLM.prompt = None
    MockLLM.auth = None
    MockLLM.calls = 0

    letter = "奶奶：最近降温了，寄了件羽绒服，还有一盒药，记得饭后再吃。"
    send_mail("alice", "bob", "物流测试", letter)

    # bob：先收 3 条物流通知，签收后再收 1 封邮件原件；alice：3 条物流通知。
    # 服务端强制相邻轨迹至少 5 分钟，三条要等 10 分钟出头。
    print("等待物流轨迹按节奏投递（约 12 分钟）...")
    wait_mail_count("bob", 3, timeout=900)
    wait_mail_count("alice", 3, timeout=900)

    # 签收前收件人不应拿到原件。
    before = mail_count("bob")
    if before != 3:
        fail(f"签收前 bob 就收到了原件：{before} 封")

    if MockLLM.auth != "Bearer test-key":
        fail(f"LLM 请求缺少正确的 Authorization：{MockLLM.auth!r}")
    prompt = MockLLM.prompt or ""
    for needle in (
        "起点（寄出地）：北京市朝阳区望京街道",
        "终点（目的地）：上海市浦东新区张江镇",
        "寄件人：alice",
        "收件人：bob",
    ):
        if needle not in prompt:
            fail(f"提示词缺少变量：{needle}")

    # 方向约束必须写进提示词。
    for needle in ("【方向要求】", "起点城市 → 中转城市 → 终点城市"):
        if needle not in prompt:
            fail(f"提示词缺少方向要求：{needle}")

    # 邮件正文要进提示词，AI 才能生成与信件内容呼应的轨迹。
    for needle in ("信件摘要：奶奶：最近降温了", "饭后再吃"):
        if needle not in prompt:
            fail(f"提示词缺少信件正文：{needle}")

    done = wait_meta_status("bob", "done")
    if done.get("total") != 3 or done.get("sent") != 3:
        fail(f"任务进度不正确：{done}")
    if letter not in done.get("letter", ""):
        fail(f"任务元数据缺少信件正文：{done.get('letter')!r}")

    candidates = [
        path
        for path in (WORK / "bob").glob("from-*.json")
        if not path.name.endswith(".meta.json")
    ]
    matched = None
    for path in candidates:
        lines = [line for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
        if len(lines) == 3:
            matched = (path, lines)
            break
    if matched is None:
        fail("找不到包含 3 条轨迹的任务文件")
    path, lines = matched
    dates = []
    for line in lines:
        entry = json.loads(line)
        if set(entry.keys()) != {"date", "courier-name", "message"}:
            fail(f"轨迹字段不正确：{entry}")
        dates.append(entry["date"])
    if dates != sorted(dates) or len(set(dates)) != 3:
        fail("轨迹时间必须严格递增")

    # 确认投递出去的物流通知能被正常解析（主题是 RFC 2047 编码）。
    # 此时原件尚未到达，bob 邮箱里只有 3 封物流通知。
    updates = []
    for mail_path in sorted((WORK / "data" / "bob").glob("*.eml")):
        message = email.message_from_bytes(mail_path.read_bytes())
        subject = str(email.header.make_header(email.header.decode_header(message["Subject"])))
        if subject.startswith("物流更新"):
            body = message.get_payload(decode=True).decode("utf-8", "replace")
            updates.append((subject, body))
    if len(updates) != 3:
        fail(f"bob 收到的物流通知数量不对：{len(updates)}")
    for subject, body in updates:
        if "运单：" not in body or "负责人：" not in body:
            fail(f"物流通知正文不完整：{body!r}")
        if "物流更新" not in subject:
            fail(f"物流通知主题不正确：{subject!r}")

    # 签收后原件应已投递，暂存文件被清理。
    if not wait_mail_count_ok("bob", 4, timeout=30):
        fail(f"签收后没有收到原件：{mail_count('bob')} 封")
    if list((WORK / "bob").glob("*.eml")):
        fail("签收后暂存原件没有被清理")

    # 最后投递的那封应当是原件（主题为“物流测试”），而不是物流通知。
    originals = []
    for mail_path in sorted((WORK / "data" / "bob").glob("*.eml")):
        message = email.message_from_bytes(mail_path.read_bytes())
        subject = str(email.header.make_header(email.header.decode_header(message["Subject"])))
        if subject == "物流测试":
            originals.append(message)
    if len(originals) != 1:
        fail(f"原件投递数量不对：{len(originals)}")
    body = originals[0].get_payload(decode=True).decode("utf-8", "replace")
    if letter not in body:
        fail(f"原件正文不正确：{body!r}")

    print(f"通过：赛博信使生成轨迹并在签收后投递原件（{path.name}）")


def main() -> None:
    ensure_binary()
    prepare_workspace()
    mock = start_mock_llm()
    server = start_server(mock.server_address[1])
    try:
        test_reject_remote_recipient()
        test_local_delivery()
        test_pop3()
        test_wrong_password()
        test_system_command()
        test_external_sender_ignored()
        test_courier()
        print("全部端到端测试通过")
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
        mock.shutdown()
        if server.returncode not in (0, -15, 1):
            print(f"服务端退出码：{server.returncode}")


if __name__ == "__main__":
    main()
