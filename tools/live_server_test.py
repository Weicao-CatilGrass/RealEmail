"""连接真实 RealEmail 服务器，按邮件客户端流程测试。"""

import poplib
import smtplib
from email.message import EmailMessage

HOST = "catilgrass.cn"
SMTP_PORT = 3629
POP3_PORT = 3628
USER = "pzw"
PASS = "TonyGood114514"
EMAIL = f"{USER}@catilgrass.cn"


def section(t):
    print(f"\n===== {t} =====")


section("1. SMTP EHLO 能力")
try:
    with smtplib.SMTP(HOST, SMTP_PORT, timeout=15) as s:
        code, resp = s.ehlo("test")
        print(code, resp.decode())
except Exception as e:
    print("失败:", type(e).__name__, e)

section("2. SMTP AUTH 登录")
try:
    with smtplib.SMTP(HOST, SMTP_PORT, timeout=15) as s:
        s.ehlo("test")
        print("login:", s.login(USER, PASS))
except Exception as e:
    print("失败:", type(e).__name__, e)

section("3. POP3 登录并列出邮件")
try:
    p = poplib.POP3(HOST, POP3_PORT, timeout=15)
    print("问候:", p.getwelcome().decode())
    print("USER:", p.user(USER).decode())
    print("PASS:", p.pass_(PASS).decode())
    count, size = p.stat()
    print(f"共 {count} 封，{size} 字节")
    for i in range(1, min(count, 5) + 1):
        resp, lines, _ = p.retr(i)
        text = b"\n".join(lines).decode("utf-8", "replace")
        subj = [l for l in text.splitlines() if l.lower().startswith("subject:")]
        print(f"  第 {i} 封: {subj}")
    p.quit()
except Exception as e:
    print("失败:", type(e).__name__, e)

section("4. 发一封测试邮件（自己发给自己）")
try:
    msg = EmailMessage()
    msg["From"] = EMAIL
    msg["To"] = EMAIL
    msg["Subject"] = "连机测试：Zed 调试"
    msg.set_content("这是从本地 ZA 环境发往真实服务器的测试邮件。\n")
    with smtplib.SMTP(HOST, SMTP_PORT, timeout=15) as s:
        s.ehlo("test")
        s.login(USER, PASS)
        s.send_message(msg, from_addr=EMAIL, to_addrs=[EMAIL])
    print("发送成功")
except Exception as e:
    print("失败:", type(e).__name__, e)

section("5. 收信确认")
try:
    p = poplib.POP3(HOST, POP3_PORT, timeout=15)
    p.user(USER)
    p.pass_(PASS)
    count, _ = p.stat()
    print(f"现在共 {count} 封")
    if count:
        resp, lines, _ = p.retr(count)
        text = b"\n".join(lines).decode("utf-8", "replace")
        for line in text.splitlines()[:12]:
            print("  ", line)
    p.quit()
except Exception as e:
    print("失败:", type(e).__name__, e)
