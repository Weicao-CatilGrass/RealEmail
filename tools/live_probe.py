"""逐条模拟手机客户端（截图配置）的登录流程，定位失败点。"""

import socket

HOST = "catilgrass.cn"
POP3_PORT = 3628
SMTP_PORT = 3629
EMAIL = "pzw@catilgrass.cn"
USER = "pzw"
PASS = "TonyGood114514"


def converse(name, port, lines):
    print(f"\n===== {name} ({HOST}:{port}) =====")
    try:
        sock = socket.create_connection((HOST, port), timeout=15)
    except Exception as exc:
        print("连接失败:", type(exc).__name__, exc)
        return
    reader = sock.makefile("rb")
    print("S:", reader.readline().decode(errors="replace").rstrip())
    for line in lines:
        sock.sendall(line.encode() + b"\r\n")
        reply = reader.readline().decode(errors="replace").rstrip()
        print(f"C: {line}")
        print(f"S: {reply}")
    sock.close()


# 截图里的 Incoming Username 是 pzw@catilgrass.cn
converse(
    "POP3 用完整邮箱做 USER",
    POP3_PORT,
    [f"USER {EMAIL}", f"PASS {PASS}", "STAT", "CAPA", "QUIT"],
)

converse(
    "POP3 用账户名做 USER",
    POP3_PORT,
    [f"USER {USER}", f"PASS {PASS}", "STAT", "QUIT"],
)

# POP3 的 CAPA 是客户端登录前必查的，单独看输出
converse("POP3 CAPA（未登录）", POP3_PORT, ["CAPA", "QUIT"])

# 手机可能先发 NOOP / 空行
converse("POP3 空行后 USER", POP3_PORT, ["", f"USER {USER}", f"PASS {PASS}", "QUIT"])
