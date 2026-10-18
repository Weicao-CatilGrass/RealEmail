//! SMTP 收信端（RFC 5321 的简化实现）。
//!
//! 只支持“服务器本地账户”作为收件人：`RCPT TO` 的本地部分必须能在
//! `accounts.toml` 中找到，否则直接返回 550。通过校验的邮件会被立即投递
//! 到对应账户的本地邮箱，也就是题目要求的“服务器内部转发”。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use tokio::net::{TcpListener, TcpStream};

use crate::accounts::Accounts;
use crate::commands::{self, SYSTEM_ACCOUNT};
use crate::courier::{self, CourierConfig};
use crate::mailstore::MailStore;
use crate::proto::{split_command, Conn, MAX_COMMAND_LINE};

/// SMTP 服务端运行所需的共享状态。
pub struct SmtpCtx {
    /// 服务端自己的主机名，用于问候语和 Received 头。
    pub host: String,
    /// 账户名（小写）到明文密码的映射，SMTP 只用到名字集合。
    pub accounts: Arc<Accounts>,
    /// 本地邮件存储。
    pub store: MailStore,
    /// system 命令脚本的存放目录（程序所在目录）。
    pub workspace: PathBuf,
    /// 物流任务配置，投递成功后用于登记赛博信使任务。
    pub courier: Arc<CourierConfig>,
    /// 单封邮件允许的最大字节数。
    pub max_message: usize,
}

/// 读取 DATA 阶段内容的结果。
enum DataRead {
    /// 完整读到了一封邮件。
    Complete(Vec<u8>),
    /// 超过大小限制，内容已丢弃。
    TooLarge,
}

/// 启动 SMTP 监听循环。
pub async fn run(listener: TcpListener, ctx: Arc<SmtpCtx>) -> Result<()> {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                // 单个连接失败不应该让整个服务退出。
                eprintln!("[smtp] 接受连接失败: {err}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(ctx, stream).await {
                let message = err.to_string();
                // 客户端正常断开不打印日志，避免刷屏。
                if !crate::proto::is_normal_disconnect(&message) {
                    eprintln!("[smtp] 连接异常: {message}");
                }
            }
        });
    }
}

/// 处理一条 SMTP 连接。
async fn handle(ctx: Arc<SmtpCtx>, stream: TcpStream) -> Result<()> {
    let mut conn = Conn::new(stream)?;
    let peer = conn.peer();
    conn.write_line(&format!("220 {} ESMTP RealEmail ready", ctx.host))
        .await?;

    let mut helo: Option<String> = None;
    let mut mail_from: Option<String> = None;
    let mut recipients: Vec<String> = Vec::new();
    let mut counter: u64 = 0;

    loop {
        let raw = match conn.read_line().await? {
            Some(line) => line,
            None => return Ok(()),
        };
        if raw.len() > MAX_COMMAND_LINE {
            conn.write_line("500 5.5.2 Line too long").await?;
            continue;
        }
        let line = String::from_utf8_lossy(&raw).trim().to_string();
        if line.is_empty() {
            conn.write_line("500 5.5.2 Empty command").await?;
            continue;
        }

        let (verb, arg) = split_command(&line);
        match verb.to_ascii_uppercase().as_str() {
            "EHLO" => {
                if arg.is_empty() {
                    conn.write_line("501 5.5.4 EHLO requires a domain").await?;
                    continue;
                }
                helo = Some(arg.to_string());
                mail_from = None;
                recipients.clear();
                // EHLO 使用多行应答，最后一行不带连字符。
                conn.write_line(&format!("250-{}", ctx.host)).await?;
                conn.write_line("250-PIPELINING").await?;
                conn.write_line(&format!("250-SIZE {}", ctx.max_message))
                    .await?;
                conn.write_line("250-8BITMIME").await?;
                conn.write_line("250-ENHANCEDSTATUSCODES").await?;
                conn.write_line("250 HELP").await?;
            }
            "HELO" => {
                helo = Some(arg.to_string());
                mail_from = None;
                recipients.clear();
                conn.write_line(&format!("250 {}", ctx.host)).await?;
            }
            "MAIL" => {
                if helo.is_none() {
                    conn.write_line("503 5.5.1 Send HELO/EHLO first").await?;
                    continue;
                }
                if mail_from.is_some() {
                    conn.write_line("503 5.5.1 Nested MAIL command").await?;
                    continue;
                }
                // 参数形如 "FROM:<alice@example.com> SIZE=123"，只取尖括号里的地址。
                if !arg.to_ascii_uppercase().starts_with("FROM:") {
                    conn.write_line("501 5.5.4 Syntax: MAIL FROM:<address>")
                        .await?;
                    continue;
                }
                match parse_path(&arg[5..]) {
                    Some(address) => {
                        mail_from = Some(address);
                        recipients.clear();
                        conn.write_line("250 2.1.0 Ok").await?;
                    }
                    None => conn.write_line("501 5.5.2 Malformed sender address").await?,
                }
            }
            "RCPT" => {
                if mail_from.is_none() {
                    conn.write_line("503 5.5.1 Need MAIL before RCPT").await?;
                    continue;
                }
                if !arg.to_ascii_uppercase().starts_with("TO:") {
                    conn.write_line("501 5.5.4 Syntax: RCPT TO:<address>").await?;
                    continue;
                }
                match parse_path(&arg[3..]) {
                    Some(address) => match local_account(&address, &ctx.accounts.map) {
                        Some(name) => {
                            if !recipients.contains(&name) {
                                recipients.push(name.clone());
                            }
                            conn.write_line(&format!("250 2.1.5 Ok <{}@{}>", name, ctx.host))
                                .await?;
                        }
                        // 服务器只收本地账户，外部地址一律拒绝。
                        None => {
                            conn.write_line("550 5.1.1 No such user here").await?;
                        }
                    },
                    None => conn.write_line("501 5.5.2 Malformed recipient address").await?,
                }
            }
            "DATA" => {
                if mail_from.is_none() {
                    conn.write_line("503 5.5.1 Need MAIL before DATA").await?;
                    continue;
                }
                if recipients.is_empty() {
                    conn.write_line("554 5.5.1 No valid recipients").await?;
                    continue;
                }
                conn.write_line("354 End data with <CR><LF>.<CR><LF>")
                    .await?;

                let body = match read_data(&mut conn, ctx.max_message).await? {
                    DataRead::Complete(body) => body,
                    DataRead::TooLarge => {
                        conn.write_line("552 5.3.4 Message too big").await?;
                        mail_from = None;
                        recipients.clear();
                        continue;
                    }
                };

                counter += 1;
                let queue_id = format!("{}.{}.{}", unix_seconds(), std::process::id(), counter);
                let full = build_message(&ctx, helo.as_deref(), &peer, &queue_id, mail_from.as_deref(), &body);
                // 寄件人也必须是本地账户，否则这封信直接忽略（既不投递也不建任务）。
                let sender_account = mail_from
                    .as_deref()
                    .and_then(|address| local_account(address, &ctx.accounts.map));
                let Some(sender_account) = sender_account else {
                    println!(
                        "[smtp] 寄件人 <{}> 不是服务器账户，忽略该邮件",
                        mail_from.as_deref().unwrap_or("")
                    );
                    conn.write_line("250 2.0.0 Ok: ignored").await?;
                    mail_from = None;
                    recipients.clear();
                    continue;
                };

                // 内部转发：把同一封邮件投递到所有本地收件人。
                let mut delivered = 0usize;
                for recipient in &recipients {
                    match ctx.store.deliver(recipient, &full).await {
                        Ok(uid) => {
                            delivered += 1;
                            println!(
                                "[smtp] 收到 <{}> 发往 <{}> 的邮件，编号 {}",
                                sender_account, recipient, uid
                            );
                        }
                        Err(err) => eprintln!("[smtp] 投递到 `{recipient}` 失败: {err}"),
                    }
                }

                // 发往 system 账户的邮件：按主题执行命令（例如设置个人介绍）。
                if recipients.iter().any(|name| name == SYSTEM_ACCOUNT) {
                    if sender_account != SYSTEM_ACCOUNT {
                        match commands::handle(&ctx.workspace, &sender_account, &full).await {
                            Ok(Some(outcome)) => println!(
                                "[system] 执行 `{}`，已写入 {}",
                                outcome.command,
                                outcome.path.display()
                            ),
                            Ok(None) => {}
                            Err(err) => eprintln!("[system] 命令执行失败: {err}"),
                        }
                    } else {
                        eprintln!("[system] 忽略 system 自己发出的命令邮件");
                    }
                }

                // 为普通收件人创建物流任务（system 是命令入口，不参与模拟）。
                let subject = commands::subject_of(&full);
                for recipient in &recipients {
                    if recipient == SYSTEM_ACCOUNT || delivered == 0 {
                        continue;
                    }
                    if let Err(err) = courier::create_task(
                        &ctx.courier,
                        &sender_account,
                        recipient,
                        &subject,
                    )
                    .await
                    {
                        eprintln!("[courier] 创建任务失败: {err}");
                    }
                }

                if delivered == 0 {
                    conn.write_line("451 4.3.0 Temporary delivery failure").await?;
                } else {
                    conn.write_line(&format!("250 2.0.0 Ok: queued as {queue_id}"))
                        .await?;
                }
                mail_from = None;
                recipients.clear();
            }
            "RSET" => {
                mail_from = None;
                recipients.clear();
                conn.write_line("250 2.0.0 Ok").await?;
            }
            "NOOP" => {
                conn.write_line("250 2.0.0 Ok").await?;
            }
            "VRFY" => {
                conn.write_line("252 2.5.2 Cannot VRFY user").await?;
            }
            "EXPN" | "AUTH" | "STARTTLS" => {
                conn.write_line("502 5.5.1 Command not implemented").await?;
            }
            "HELP" => {
                conn.write_line("214 2.0.0 Commands: EHLO HELO MAIL RCPT DATA RSET NOOP QUIT")
                    .await?;
            }
            "QUIT" => {
                conn.write_line("221 2.0.0 Bye").await?;
                return Ok(());
            }
            other => {
                conn.write_line(&format!("500 5.5.2 Command unrecognized: {other}"))
                    .await?;
            }
        }
    }
}

/// 读取 DATA 段，直到单独一行的 `.` 为止。
///
/// 行首的 `.` 会被去掉一层（SMTP 的点填充规则），换行统一为 CRLF。
/// 超限时继续把剩余数据读完再返回，避免协议错位。
async fn read_data(conn: &mut Conn, max_size: usize) -> Result<DataRead> {
    let mut data: Vec<u8> = Vec::new();
    let mut too_large = false;

    loop {
        let line = match conn.read_line().await? {
            Some(line) => line,
            None => bail!("client closed during DATA"),
        };
        if line == b"." {
            break;
        }
        // 去掉客户端加上的那一个点。
        let line = if line.first() == Some(&b'.') {
            &line[1..]
        } else {
            &line[..]
        };

        if too_large {
            continue;
        }
        if data.len() + line.len() + 2 > max_size {
            too_large = true;
            data.clear();
            continue;
        }
        data.extend_from_slice(line);
        data.extend_from_slice(b"\r\n");
    }

    if too_large {
        return Ok(DataRead::TooLarge);
    }
    Ok(DataRead::Complete(data))
}

/// 在原始邮件前补上 Return-Path 与 Received 头。
fn build_message(
    ctx: &SmtpCtx,
    helo: Option<&str>,
    peer: &std::net::SocketAddr,
    queue_id: &str,
    mail_from: Option<&str>,
    body: &[u8],
) -> Vec<u8> {
    let date = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S +0000")
        .to_string();
    let from = mail_from.unwrap_or("");
    let helo = helo.unwrap_or("unknown");

    let mut out = Vec::with_capacity(body.len() + 256);
    out.extend_from_slice(format!("Return-Path: <{from}>\r\n").as_bytes());
    out.extend_from_slice(
        format!(
            "Received: from {helo} ([{}]) by {} with ESMTP id {}; {date}\r\n",
            peer.ip(),
            ctx.host,
            queue_id
        )
        .as_bytes(),
    );
    out.extend_from_slice(body);
    out
}

/// 从 `MAIL FROM:` / `RCPT TO:` 的参数里取出地址本身。
///
/// 支持 `<a@b>` 与裸地址两种写法；空路径 `<>` 返回空字符串。
fn parse_path(arg: &str) -> Option<String> {
    let text = arg.trim();
    let inner = match (text.find('<'), text.rfind('>')) {
        (Some(left), Some(right)) if right > left => &text[left + 1..right],
        (None, _) => text.split_whitespace().next().unwrap_or(""),
        _ => return None,
    };
    let inner = inner.trim();
    if inner.len() > 320 {
        return None;
    }
    if inner.is_empty() {
        return Some(String::new());
    }
    // 地址里不允许出现空白、尖括号和控制字符。
    if inner
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '<' || ch == '>')
    {
        return None;
    }
    Some(inner.to_string())
}

/// 判断收件地址是否对应本地账户，返回规范化后的账户名。
fn local_account(address: &str, accounts: &BTreeMap<String, String>) -> Option<String> {
    let address = address.trim();
    // 只比较 `@` 前的本地部分，域名不参与判断。
    let local = match address.rsplit_once('@') {
        Some((local, _domain)) => local,
        None => address,
    };
    let local = local.to_ascii_lowercase();
    if accounts.contains_key(&local) {
        Some(local)
    } else {
        None
    }
}

/// 当前 Unix 时间戳（秒）。
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 各种地址写法都应能解析出干净地址。
    #[test]
    fn parse_address() {
        assert_eq!(parse_path("<a@b.com>").unwrap(), "a@b.com");
        assert_eq!(parse_path("<a@b.com> SIZE=100").unwrap(), "a@b.com");
        assert_eq!(parse_path("a@b.com").unwrap(), "a@b.com");
        assert_eq!(parse_path("<>").unwrap(), "");
        assert!(parse_path("<a b@c>").is_none());
    }

    // 只有本地账户才是合法收件人，且域名大小写无关。
    #[test]
    fn local_recipients() {
        let mut accounts = BTreeMap::new();
        accounts.insert("alice".to_string(), String::new());
        assert_eq!(
            local_account("Alice@Example.COM", &accounts).unwrap(),
            "alice"
        );
        assert_eq!(local_account("alice", &accounts).unwrap(), "alice");
        assert!(local_account("bob@example.com", &accounts).is_none());
    }
}
