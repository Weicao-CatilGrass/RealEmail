"""用真实 DeepSeek API 跑一遍物流生成，检查轨迹是否与信件内容呼应。

用法：
    python3 tools/llm_live_test.py <api-key>
"""

import json
import shutil
import smtplib
import subprocess
import sys
import time
from email.message import EmailMessage
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORK = Path("/tmp/real_email_llm")
BIN = ROOT / "build" / "realemail"
SMTP_PORT = 23629
POP3_PORT = 23628
DOMAIN = "catilgrass.cn"

if len(sys.argv) < 2:
    print("用法：python3 tools/llm_live_test.py <api-key>")
    sys.exit(1)
KEY = sys.argv[1]


def run(cmd):
    subprocess.run(cmd, check=True, capture_output=True)


if WORK.exists():
    shutil.rmtree(WORK)
WORK.mkdir(parents=True)

for name in ("alice", "bob"):
    run([str(BIN), "--accounts", str(WORK / "accounts.toml"), "add", name, "-p", "pw123456"])

(WORK / "alice.md").write_text("地址：黑龙江省哈尔滨市道里区中央大街\n", encoding="utf-8")
(WORK / "bob.md").write_text("地址：海南省三亚市吉阳区大东海\n", encoding="utf-8")
(WORK / "bob").mkdir(exist_ok=True)
(WORK / "bob" / "deepseek-api-key").write_text(KEY, encoding="utf-8")

log = open(WORK / "server.log", "wb")
server = subprocess.Popen(
    [
        str(BIN),
        "--accounts", str(WORK / "accounts.toml"),
        "--data-dir", str(WORK / "data"),
        "serve",
        "--host", "127.0.0.1",
        "--domain", DOMAIN,
        "--smtp-port", str(SMTP_PORT),
        "--pop3-port", str(POP3_PORT),
        "--courier-interval-seconds", "5",
        "--courier-span-minutes", "4320",
        "--llm-model", "deepseek-flash",
    ],
    stdout=log,
    stderr=subprocess.STDOUT,
)

try:
    time.sleep(2)
    letter = (
        "奶奶：\n\n"
        "最近上海降温很快，我寄了一盒需要冷藏的生物药剂，还有一件易碎的陶瓷花瓶，"
        "请一定轻拿轻放。药剂要避光、保持 2-8 度，花瓶是我在景德镇自己做的，碎了就没法补了。\n\n"
        "孙子上"
    )
    msg = EmailMessage()
    msg["From"] = f"alice@{DOMAIN}"
    msg["To"] = f"bob@{DOMAIN}"
    msg["Subject"] = "急件：药品和花瓶"
    msg.set_content(letter)
    with smtplib.SMTP("127.0.0.1", SMTP_PORT, timeout=10) as smtp:
        smtp.ehlo("localhost")
        smtp.send_message(msg, from_addr=f"alice@{DOMAIN}", to_addrs=[f"bob@{DOMAIN}"])
    print("已发送带剧情的邮件，等待 LLM 生成 ...")

    deadline = time.time() + 240
    meta = None
    while time.time() < deadline:
        metas = list((WORK / "bob").glob("from-*.meta.json"))
        if metas:
            meta = json.loads(metas[0].read_text(encoding="utf-8"))
            if meta["status"] in ("ready", "done", "failed"):
                break
        time.sleep(2)

    print("\n===== 任务元数据 =====")
    print(json.dumps(meta, ensure_ascii=False, indent=2) if meta else "没有生成任务")

    if meta and meta["status"] != "failed":
        trace_file = next(
            p for p in (WORK / "bob").glob("from-*.json") if not p.name.endswith(".meta.json")
        )
        print("\n===== 生成的物流轨迹 =====")
        for line in trace_file.read_text(encoding="utf-8").splitlines():
            entry = json.loads(line)
            print(f"{entry['date']}  {entry['courier-name']:<16} {entry['message']}")
finally:
    server.terminate()
    server.wait(timeout=10)
    log.close()
    print("\n===== 服务端日志 =====")
    print((WORK / "server.log").read_text(encoding="utf-8", errors="replace"))
