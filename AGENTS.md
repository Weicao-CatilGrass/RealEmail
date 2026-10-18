# AGENTS.md

本文件记录本仓库的协作约定，修改代码前请先读一遍。

## 项目简介

RealEmail 是一个用 Rust 写的极简邮件服务端：SMTP 负责收信，POP3 负责取信。
服务器只接收“本机账户”作为收件人，收到后直接投递到对应账户的本地邮箱，不做外部转发。

- SMTP 默认端口：3629
- POP3 默认端口：3628
- 账户文件：程序所在目录下的 `accounts.toml`
- 密码：AES-256-GCM 加密后写入 `accounts.toml`，密钥为同目录的 `mail.key`

## 硬性约定

1. **代码注释一律使用中文**。包括模块级 `//!`、函数/字段的 `///`、行内 `//`。
2. **提交信息使用中文，且不使用约定式提交（Conventional Commits）**。
    - 不要写 `feat:` `fix:` `chore:` `refactor:` 之类的前缀。
    - 正确示例：`搭建 POP3 与 SMTP 服务端`
    - 正确示例：`修复 POP3 删除邮件后编号错乱的问题`
    - 错误示例：`feat: add pop3 server`
3. 协议回包（`+OK`、`-ERR`、`250` 等）必须保持英文 ASCII，方便各类邮件客户端解析；
   日志、报错信息、注释使用中文。
4. `accounts.toml` 与 `mail.key` 属于敏感文件，不提交到仓库。

## 常用命令

```powershell
make build                     # musl 静态链接构建
make check                     # cargo check --all-targets
make clean                     # 清理构建产物
cargo build --release          # 构建
cargo test                     # 单元测试
python tools/e2e_test.py       # 端到端测试（自动起停服务）
cargo run -- list              # 查看账户
cargo run -- add alice         # 新增账户（交互输入密码）
cargo run -- passwd alice      # 修改密码
cargo run -- encrypt 明文密码   # 生成密文，手工填入 accounts.toml
cargo run -- serve             # 启动服务
```

## 赛博信使（物流任务）

本地账户之间投递成功后，会在**收件人目录** `./{收件人}/` 创建：

- `from-{寄件人}-{UTC 时间}.json`：物流轨迹 JSONL，字段固定为
  `{"date":"YYYY-MM-DD HH:MM","courier-name":"英文名","message":"物流描述"}`；
- `from-{寄件人}-{UTC 时间}.meta.json`：任务元数据与投递进度（pending/ready/done/failed）。

规则：

- 后台每 60 秒轮询一次（`--courier-interval-seconds`），先调 LLM 生成，再逐条按时间投递；
- 密钥优先读 `./{收件人}/deepseek-api-key`，其次 `./deepseek-api-key`；
  没有密钥或调用失败只标记 `failed`，不允许伪造轨迹；
- 到点的轨迹同时投递到收件人和寄件人邮箱，投递走邮件存储，不再触发新任务；
- 起终点来自双方 `./{账户}.md` 中的 `地址：` 行，缺省用 `账户@域名`；
- 发往 `system` 的邮件不创建物流任务；
- 寄件人不是本地账户时整封邮件忽略（SMTP 仍回 250）。

## system 账户命令

发往 `system` 账户的邮件按主题执行命令，目前只有一条：

- 主题 `设置个人介绍`（别名：`设置简介`、`设置自我介绍`、`set bio`）→ 把正文写入
  程序目录（即 `accounts.toml` 所在目录）下的 `<发件人账户名>.md`，直接覆盖。

约束：

- 发件人必须是本地账户，否则忽略命令；
- 新增命令时在 `src/commands.rs` 的 `SystemCommand` 与 `match_subject` 中扩展，
  并在 README 的表格里补一行。

## 目录结构

```
src/
  main.rs      # 命令行入口与服务启动
  crypto.rs    # 密码加解密（AES-256-GCM）
  accounts.rs  # accounts.toml 读写与校验
  mailstore.rs # 邮件落盘存储
  smtp.rs      # SMTP 收信
  pop3.rs      # POP3 取信
  proto.rs     # 行级协议读写工具
  commands.rs  # system 账户的邮件命令与 MIME 解码
  courier.rs   # 赛博信使物流任务：LLM 生成、轮询、按时间投递
tools/
  e2e_test.py  # 端到端测试脚本
```
