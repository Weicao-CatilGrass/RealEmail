# RealEmail

用 Rust 写的极简邮件服务端：**SMTP 收信 + POP3 取信**，账户写死在本地文件里，
只接收本机账户作为收件人，邮件直接投递到对应账户的本地邮箱（服务器内部转发）。

另外内置一个 `system` 账户：给它发信，邮件主题就是命令。

## 端口

| 协议 | 默认端口 | 说明                       |
| ---- | -------- | -------------------------- |
| SMTP | **3629** | 收信，客户端发信服务器地址 |
| POP3 | **3628** | 取信，第二个端口用 3628    |

两个端口都可以改：

```powershell
realemail serve --smtp-port 3629 --pop3-port 3628
realemail serve --smtp-port 3628 --pop3-port 3629   # 想对调也行
```

## 构建

```bash
make build    # musl 静态链接，产物：target/x86_64-unknown-linux-musl/release/realemail
make check    # cargo check --all-targets
make clean    # cargo clean
```

也可以直接用 cargo 构建（不指定 musl 目标时产物在 `target\release\realemail.exe`）：

```powershell
cargo build --release
```

把 `realemail.exe` 放到任意目录，`accounts.toml`、`mail.key`、`data\` 都会生成在
程序所在目录（用 `--accounts` 可以指到别处）。

## 账户配置

`accounts.toml` 就在程序旁边，格式是一行一个账户，值必须是**加密后的密码**：

```toml
# RealEmail 账户文件
# 密码由 AES-256-GCM 加密，请勿手工填写明文。

alice = "enc:v1:3fK9...."
bob = "enc:v1:9mQ2...."
system = "enc:v1:7xT1...."
```

用命令行维护最省事：

```powershell
realemail add alice          # 新增账户，交互输入两次密码
realemail add bob -p 明文     # 也可直接指定
realemail passwd alice       # 改密码
realemail list               # 看看有哪些账户
realemail encrypt 明文密码    # 只想手工写文件时，生成密文粘贴进去
```

明文密码会被拒绝加载：

```
账户 `alice` 的密码是明文，必须先加密：realemail passwd alice
```

### 加密方式

- 算法：AES-256-GCM，密文格式 `enc:v1:<base64url(nonce || ciphertext)>`；
- 密钥：程序目录下的 `mail.key`（首次运行自动生成 32 字节随机密钥，类 Unix 下权限 0600）；
- `mail.key` 丢了就解不开已有密码，只能删掉账户重新 `add`，请务必备份；
- `accounts.toml` 和 `mail.key` 都已加入 `.gitignore`。

## 启动

```powershell
realemail                # 等同于 realemail serve
realemail serve --host 127.0.0.1 --domain realemail.local
```

启动后会打印监听地址、账户数和数据目录：

```
账户文件: D:\RealEmail\accounts.toml
密钥文件: D:\RealEmail\mail.key
数据目录: D:\RealEmail\data
脚本目录: D:\RealEmail
已加载 3 个账户
SMTP 监听: 0.0.0.0:3629
POP3 监听: 0.0.0.0:3628
```

## 客户端设置

| 项目               | 值                                         |
| ------------------ | ------------------------------------------ |
| 发信服务器（SMTP） | 服务器 IP，端口 3629                       |
| 收信服务器（POP3） | 服务器 IP，端口 3628                       |
| 用户名             | 账户名，例如 `alice`（不是完整邮箱）       |
| 密码               | 该账户的明文密码                           |
| 加密方式           | 无（不支持 TLS/SSL，请只在内网或本机使用） |

收件人地址写成 `bob@任意域名` 都可以，服务器只比较 `@` 前面的账户名。
**只要本地没有这个账户，一律 550 拒收**，不会向外部服务器投递。
寄件人不是本地账户时，整封邮件被静默忽略：返回 `250`，但不投递、不建物流任务。

## 赛博信使：物流轨迹模拟

本地账户之间成功投递一封信后，服务器会在**收件人目录**下创建物流任务：

```
<程序目录>/bob/
  from-alice-20250102-030405.123.json       轨迹本体（JSONL，每行一条物流信息）
  from-alice-20250102-030405.123.meta.json  任务元数据与投递进度
```

轨迹文件格式与约定完全一致：

```json
{"date":"2025-01-02 03:10","courier-name":"Michael Smith","message":"快件已揽收，运单号 SF1234567890"}
{"date":"2025-01-02 03:25","courier-name":"Anna Brown","message":"到达北京转运中心，进入分拣环节"}
{"date":"2025-01-02 04:05","courier-name":"Tom Wilson","message":"已签收，签收凭证已上传，追踪码关闭"}
```

后台每 60 秒轮询一次所有任务：

1. `pending`：读取收件人目录下的 `deepseek-api-key`（其次读程序目录的同名文件），
   调用 LLM 生成 12-16 条轨迹，写入 `.json` 并把状态改为 `ready`；
2. `ready`：逐条检查 `date`，到点就把这条物流信息以邮件形式**同时投递到收件人和寄件人邮箱**，
   全部投完后状态改为 `done`。

起终点取自双方自己设置的资料文件 `<账户>.md`，按 `地址：` 这类行读取，读不到时用 `账户@域名` 兜底：

```markdown
地址：北京市朝阳区望京街道
```

其他行为：

- 收件人目录下没有 `deepseek-api-key`，或 LLM 调用失败：任务标记为 `failed` 并记录原因，**不会伪造内容**；
- 发往 `system` 账户的邮件属于命令入口，不创建物流任务；
- 相关参数：

```powershell
realemail serve --courier-span-minutes 180   # 模拟总时长（交给 LLM 的“总时长”）
realemail serve --courier-interval-seconds 60 # 轮询间隔
realemail serve --llm-endpoint https://api.deepseek.com/chat/completions
realemail serve --llm-model deepseek-chat
```

## system 账户：用邮件执行命令

给 `system` 发信，主题决定执行什么命令：

| 主题                                                     | 动作                                                   |
| -------------------------------------------------------- | ------------------------------------------------------ |
| `设置个人介绍` / `设置简介` / `设置自我介绍` / `set bio` | 把邮件正文写入程序目录下的 `<发件人>.md`，**直接覆盖** |

例如 alice 发一封主题为“设置个人介绍”、正文为“我是 alice”的邮件给
`system@realemail.local`，服务器会在程序目录生成/覆盖 `alice.md`：

```
[system] 执行 `设置个人介绍`，已写入 D:\RealEmail\alice.md
```

细节：

- 发件人必须是本地账户，`stranger@example.com` 发来的邮件不会触发命令；
- 主题支持 RFC 2047 编码（中文主题在真实客户端里通常是 `=?UTF-8?B?...?=` 的形式）；
- 正文支持纯文本、Base64、Quoted-Printable、multipart，以及 UTF-8 / GBK 等字符集；
- 邮件本身仍会正常投递到 `system` 的邮箱，可以登录查看历史。

## 数据目录

```
accounts.toml        账户与加密密码
mail.key             主密钥
alice.md             用户自己设置的个人资料（system 命令写入，物流任务读取）
alice/
  deepseek-api-key   该账户调用 LLM 的密钥
  from-bob-*.json    物流轨迹任务
data/
  alice/
    0000000001.eml   原始邮件（CRLF 换行，含 Return-Path / Received 头）
    uidnext          下一个可用编号
```

## 实现范围

- SMTP：HELO/EHLO、MAIL、RCPT、DATA、RSET、NOOP、VRFY、QUIT、HELP，支持 SIZE / 8BITMIME / PIPELINING 声明；
- POP3：USER/PASS、STAT、LIST、RETR、DELE、NOOP、RSET、TOP、UIDL、CAPA、QUIT；
- 同一邮箱同一时刻只允许一个 POP3 会话；`DELE` 只在正常 `QUIT` 后生效；
- 物流任务按分钟轮询，`date` 到点才投递，没到点就一直等；
- 不支持：TLS、外部域名投递、SMTP 认证、APOP。

## 测试

```powershell
cargo test                       # 单元测试
python tools/e2e_test.py         # 端到端测试（自动起停服务）
```
