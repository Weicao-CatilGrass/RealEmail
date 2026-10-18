"""RealEmail 端到端测试脚本。

覆盖以下场景：
1. SMTP 投递：本地账户之间互相发信；
2. 非本地收件人（例如 carol@）必须被 550 拒绝；
3. 发往 system 账户的中文主题邮件按“设置个人介绍”写入 alice.md（覆盖式）；
4. 非本地发件人无法触发 system 命令；
5. POP3 的 USER/PASS、STAT、LIST、UIDL、TOP、RETR、DELE、QUIT 全流程；
6. 同一邮箱不允许两个 POP3 会话同时登录。

用法：python tools/e2e_test.py
"""

import os
import email
import email.header
import poplib
import shutil
import smtplib
import socket
import subprocess
import sys
import time
from email.message import EmailMessage
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
    result = subprocess.run(["cargo", "build"], cwd=ROOT)
    if result.returncode != 0:
        fail("cargo build 失败")


def prepare_workspace() -> None:
    """准备干净的测试目录，并生成账户文件。"""
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


def start_server() -> subprocess.Popen:
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
        client.sendmail(f"{sender}@{DOMAIN}", [f"{recipient}@{DOMAIN}"], build_message(sender, recipient, subject, body))


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
    """alice 发给 bob 的邮件应出现在 bob 的邮箱里。"""
    send_mail("alice", "bob", "第一封测试邮件", "你好 bob，这是正文。\n第二行。")
    wait_mail_count("bob", 1)
    print("通过：本地账户之间可以投递")


def wait_mail_count(account: str, expected: int, timeout: float = 5.0) -> None:
    """等待某个邮箱里的邮件数量达到预期。"""
    mailbox = WORK / "data" / account
    deadline = time.time() + timeout
    while time.time() < deadline:
        count = len(list(mailbox.glob("*.eml"))) if mailbox.exists() else 0
        if count == expected:
            return
        time.sleep(0.05)
    fail(f"邮箱 {account} 中邮件数量未达到 {expected}")


def test_system_command() -> None:
    """发往 system 的中文主题邮件应写入 alice.md。"""
    bio = "我是 alice，喜欢写信。\n第二行介绍。"
    send_mail("alice", "system", "设置个人介绍", bio)
    target = WORK / "alice.md"
    deadline = time.time() + 5
    while time.time() < deadline:
        if target.exists():
            break
        time.sleep(0.05)
    if not target.exists():
        fail("alice.md 没有被创建")
    content = target.read_text(encoding="utf-8")
    expected = bio + "\n"
    if content != expected:
        fail(f"alice.md 内容不符：{content!r} != {expected!r}")

    # 再发一封，验证是直接覆盖。
    send_mail("alice", "system", "设置个人介绍", "这是新的介绍。")
    time.sleep(0.5)
    if target.read_text(encoding="utf-8") != "这是新的介绍。\n":
        fail("alice.md 没有被覆盖")
    print("通过：system 账户执行“设置个人介绍”并覆盖写入")


def test_external_sender_cannot_trigger() -> None:
    """非本地发件人发往 system 时不应改动脚本文件。"""
    target = WORK / "alice.md"
    before = target.read_text(encoding="utf-8")
    with smtplib.SMTP(HOST, SMTP_PORT, timeout=10) as client:
        client.ehlo("tester")
        client.sendmail(
            "stranger@example.com",
            [f"system@{DOMAIN}"],
            build_message("stranger", "system", "设置个人介绍", "外部来信"),
        )
    time.sleep(0.5)
    if target.read_text(encoding="utf-8") != before:
        fail("非本地发件人不应触发 system 命令")
    print("通过：非本地发件人无法触发 system 命令")


def test_pop3() -> None:
    """完整的 POP3 收信流程。"""
    client = poplib.POP3(HOST, POP3_PORT, timeout=10)
    client.user("bob")
    client.pass_("bob-pass")

    count, size = client.stat()
    if count != 1:
        fail(f"bob 应有 1 封邮件，实际 {count}")

    listing = client.list()
    if len(listing[1]) != 1:
        fail("LIST 返回数量不正确")

    uidl = client.uidl()
    if not uidl[1] or not uidl[1][0].split()[1].isdigit():
        fail("UIDL 返回格式不正确")

    # TOP 只返回邮件头和指定行数的正文。
    top_text = b"\r\n".join(client.top(1, 2)[1]).decode("utf-8", "replace")
    if "Subject:" not in top_text or "Received: from tester" not in top_text:
        fail("TOP 没有返回邮件头")

    raw = b"\r\n".join(client.retr(1)[1])
    message = email.message_from_bytes(raw)
    subject = str(email.header.make_header(email.header.decode_header(message["Subject"])))
    if subject != "第一封测试邮件":
        fail(f"RETR 取回的主题不正确：{subject!r}")
    body = message.get_payload(decode=True).decode("utf-8", "replace")
    if "你好 bob" not in body:
        fail("RETR 没有取回正文")
    if not message["Received"] or not message["Return-Path"]:
        fail("邮件缺少 Received / Return-Path 头")

    # 登录锁：第二个会话不能同时登录同一邮箱。
    second = poplib.POP3(HOST, POP3_PORT, timeout=10)
    second.user("bob")
    try:
        second.pass_("bob-pass")
        fail("同一邮箱不应允许两个 POP3 会话")
    except poplib.error_proto:
        pass
    finally:
        second.close()

    # 标记删除并退出，邮件文件应被真正删除。
    client.dele(1)
    client.quit()
    wait_mail_count("bob", 0)
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


def main() -> None:
    ensure_binary()
    prepare_workspace()
    server = start_server()
    try:
        test_reject_remote_recipient()
        test_local_delivery()
        test_system_command()
        test_external_sender_cannot_trigger()
        test_pop3()
        test_wrong_password()
        print("全部端到端测试通过")
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
        if server.returncode not in (0, -15, 1):
            print(f"服务端退出码：{server.returncode}")


if __name__ == "__main__":
    main()
