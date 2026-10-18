//! POP3 收信端（RFC 1939 的简化实现），额外实现了 CAPA / UIDL / TOP。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};

use crate::accounts::Accounts;
use crate::mailstore::{MailStore, StoredMessage};
use crate::proto::{split_command, Conn, MAX_COMMAND_LINE};

/// POP3 服务端共享状态。
pub struct Pop3Ctx {
    /// 服务端主机名，仅用于问候语。
    pub host: String,
    /// 账户集合，用于校验密码。
    pub accounts: Arc<Accounts>,
    /// 本地邮件存储。
    pub store: MailStore,
    /// 已登录账户集合，保证同一个邮箱同时只有一个 POP3 会话在使用。
    pub active: Mutex<HashSet<String>>,
}

/// 会话锁：离开作用域时自动把账户从“占用中”集合里移除。
struct ActiveGuard {
    ctx: Arc<Pop3Ctx>,
    name: String,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.ctx.active.lock() {
            set.remove(&self.name);
        }
    }
}

/// 启动 POP3 监听循环。
pub async fn run(listener: TcpListener, ctx: Arc<Pop3Ctx>) -> Result<()> {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("[pop3] 接受连接失败: {err}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(ctx, stream).await {
                let message = err.to_string();
                if !crate::proto::is_normal_disconnect(&message) {
                    eprintln!("[pop3] 连接异常: {message}");
                }
            }
        });
    }
}

/// 处理一条 POP3 连接。
async fn handle(ctx: Arc<Pop3Ctx>, stream: TcpStream) -> Result<()> {
    let mut conn = Conn::new(stream)?;
    conn.write_line(&format!("+OK {} RealEmail POP3 ready", ctx.host))
        .await?;

    let mut user: Option<String> = None; // USER 指定的账户，尚未验证密码
    let mut authed: Option<String> = None; // 已通过密码验证的账户
    let mut snapshot: Vec<StoredMessage> = Vec::new(); // 登录瞬间的邮件列表快照
    let mut deleted: HashSet<u64> = HashSet::new(); // 本次会话标记删除的邮件
    let mut _guard: Option<ActiveGuard> = None; // 邮箱占用锁，随连接结束释放

    loop {
        let raw = match conn.read_line().await? {
            Some(line) => line,
            None => return Ok(()),
        };
        if raw.len() > MAX_COMMAND_LINE {
            conn.write_line("-ERR command line too long").await?;
            continue;
        }
        let line = String::from_utf8_lossy(&raw);
        let (verb, arg) = split_command(line.trim_end());
        let verb = verb.to_ascii_uppercase();

        // 除认证阶段的命令外，其余命令都必须先登录。
        let needs_auth = matches!(
            verb.as_str(),
            "STAT" | "LIST" | "RETR" | "DELE" | "NOOP" | "RSET" | "TOP" | "UIDL"
        );
        if needs_auth && authed.is_none() {
            conn.write_line("-ERR [AUTH] authenticate with USER/PASS first")
                .await?;
            continue;
        }
        let account = authed.clone().unwrap_or_default();

        match verb.as_str() {
            "CAPA" => {
                conn.write_line("+OK Capability list follows").await?;
                conn.write_line("TOP").await?;
                conn.write_line("UIDL").await?;
                conn.write_line("USER").await?;
                conn.write_line("RESP-CODES").await?;
                conn.write_line("PIPELINING").await?;
                conn.write_line("IMPLEMENTATION RealEmail 0.1.0").await?;
                conn.write_line(".").await?;
            }
            "USER" => {
                if authed.is_some() {
                    conn.write_line("-ERR already authenticated").await?;
                } else if arg.is_empty() {
                    conn.write_line("-ERR [AUTH] USER requires an account name")
                        .await?;
                } else {
                    // 这里不检查账户是否存在，避免暴露账户列表。
                    user = Some(arg.to_string());
                    conn.write_line("+OK send PASS").await?;
                }
            }
            "PASS" => {
                if authed.is_some() {
                    conn.write_line("-ERR already authenticated").await?;
                    continue;
                }
                let Some(name) = user.clone() else {
                    conn.write_line("-ERR [AUTH] send USER first").await?;
                    continue;
                };
                let name = name.to_ascii_lowercase();
                let ok = ctx.accounts.verify(&name, arg);
                if !ok {
                    // 认证失败时稍作延迟，降低暴力猜测速度。
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    user = None;
                    conn.write_line("-ERR [AUTH] authentication failed").await?;
                    continue;
                }

                // 同一邮箱不允许两个 POP3 会话同时操作。
                // 注意锁不能跨 await 持有，所以先把结果记下来再回包。
                let mut locked = false;
                let mut poisoned = false;
                {
                    match ctx.active.lock() {
                        Ok(mut active) => {
                            if active.contains(&name) {
                                locked = true;
                            } else {
                                active.insert(name.clone());
                            }
                        }
                        Err(_) => poisoned = true,
                    }
                }
                if poisoned {
                    conn.write_line("-ERR internal error").await?;
                    continue;
                }
                if locked {
                    conn.write_line("-ERR [IN-USE] mailbox is locked by another session")
                        .await?;
                    continue;
                }
                _guard = Some(ActiveGuard {
                    ctx: ctx.clone(),
                    name: name.clone(),
                });

                snapshot = ctx.store.snapshot(&name).await?;
                deleted.clear();
                authed = Some(name.clone());
                println!("[pop3] {name} 登录成功，邮箱中有 {} 封邮件", snapshot.len());
                conn.write_line(&format!("+OK mailbox ready, {} messages", snapshot.len()))
                    .await?;
            }
            "APOP" | "AUTH" | "STLS" => {
                conn.write_line("-ERR command not supported").await?;
            }
            "STAT" => {
                let (count, size) = visible_stats(&snapshot, &deleted);
                conn.write_line(&format!("+OK {count} {size}")).await?;
            }
            "LIST" => {
                if arg.is_empty() {
                    let (count, size) = visible_stats(&snapshot, &deleted);
                    conn.write_line(&format!("+OK {count} messages ({size} octets)"))
                        .await?;
                    for (index, message) in snapshot.iter().enumerate() {
                        if deleted.contains(&message.uid) {
                            continue;
                        }
                        conn.write_line(&format!("{} {}", index + 1, message.size))
                            .await?;
                    }
                    conn.write_line(".").await?;
                } else {
                    match pick_message(&snapshot, arg) {
                        Some((index, message)) => {
                            if deleted.contains(&message.uid) {
                                conn.write_line("-ERR message already deleted").await?;
                            } else {
                                conn.write_line(&format!("+OK {} {}", index, message.size))
                                    .await?;
                            }
                        }
                        None => conn.write_line("-ERR no such message").await?,
                    }
                }
            }
            "UIDL" => {
                if arg.is_empty() {
                    conn.write_line("+OK unique-id listing follows").await?;
                    for (index, message) in snapshot.iter().enumerate() {
                        if deleted.contains(&message.uid) {
                            continue;
                        }
                        conn.write_line(&format!("{} {}", index + 1, message.uid))
                            .await?;
                    }
                    conn.write_line(".").await?;
                } else {
                    match pick_message(&snapshot, arg) {
                        Some((index, message)) => {
                            if deleted.contains(&message.uid) {
                                conn.write_line("-ERR message already deleted").await?;
                            } else {
                                conn.write_line(&format!("+OK {} {}", index, message.uid))
                                    .await?;
                            }
                        }
                        None => conn.write_line("-ERR no such message").await?,
                    }
                }
            }
            "RETR" => match pick_message(&snapshot, arg) {
                Some((index, message)) if !deleted.contains(&message.uid) => {
                    match ctx.store.read(&account, message.uid).await {
                        Ok(data) => {
                            conn.write_line(&format!("+OK {} octets", data.len()))
                                .await?;
                            let mut payload = dot_stuff(&data);
                            if !payload.ends_with(b"\r\n") {
                                payload.extend_from_slice(b"\r\n");
                            }
                            payload.extend_from_slice(b".\r\n");
                            conn.write_bytes(&payload).await?;
                            conn.flush().await?;
                        }
                        Err(err) => {
                            eprintln!("[pop3] 读取第 {index} 封邮件失败: {err}");
                            conn.write_line("-ERR message unavailable").await?;
                        }
                    }
                }
                Some(_) => conn.write_line("-ERR message already deleted").await?,
                None => conn.write_line("-ERR no such message").await?,
            },
            "TOP" => {
                let mut parts = arg.split_whitespace();
                let index = parts.next().unwrap_or("");
                let line_count = parts
                    .next()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                match pick_message(&snapshot, index) {
                    Some((number, message)) if !deleted.contains(&message.uid) => {
                        match ctx.store.read(&account, message.uid).await {
                            Ok(data) => {
                                conn.write_line("+OK top of message follows").await?;
                                let mut payload = top_of_message(&data, line_count);
                                if !payload.ends_with(b"\r\n") {
                                    payload.extend_from_slice(b"\r\n");
                                }
                                payload.extend_from_slice(b".\r\n");
                                conn.write_bytes(&payload).await?;
                                conn.flush().await?;
                            }
                            Err(err) => {
                                eprintln!("[pop3] 读取第 {number} 封邮件失败: {err}");
                                conn.write_line("-ERR message unavailable").await?;
                            }
                        }
                    }
                    Some(_) => conn.write_line("-ERR message already deleted").await?,
                    None => conn.write_line("-ERR no such message").await?,
                }
            }
            "DELE" => match pick_message(&snapshot, arg) {
                Some((index, message)) if !deleted.contains(&message.uid) => {
                    deleted.insert(message.uid);
                    conn.write_line(&format!("+OK message {index} deleted")).await?;
                }
                Some(_) => conn.write_line("-ERR message already deleted").await?,
                None => conn.write_line("-ERR no such message").await?,
            },
            "RSET" => {
                deleted.clear();
                let (count, size) = visible_stats(&snapshot, &deleted);
                conn.write_line(&format!("+OK maildrop has {count} messages ({size} octets)"))
                    .await?;
            }
            "NOOP" => {
                conn.write_line("+OK").await?;
            }
            "QUIT" => {
                // 只有正常 QUIT 才真正删除标记过的邮件（RFC 1939 UPDATE 阶段）。
                let mut removed = 0usize;
                if let Some(account) = &authed {
                    for uid in &deleted {
                        match ctx.store.delete(account, *uid).await {
                            Ok(()) => removed += 1,
                            Err(err) => eprintln!("[pop3] 删除邮件失败: {err}"),
                        }
                    }
                }
                if removed > 0 {
                    println!("[pop3] {account} 删除 {removed} 封邮件");
                }
                conn.write_line("+OK bye").await?;
                return Ok(());
            }
            other => {
                conn.write_line(&format!("-ERR unknown command: {other}"))
                    .await?;
            }
        }
    }
}

/// 统计未被删除的邮件数量与总字节数。
fn visible_stats(snapshot: &[StoredMessage], deleted: &HashSet<u64>) -> (usize, u64) {
    let mut count = 0;
    let mut size = 0;
    for message in snapshot {
        if deleted.contains(&message.uid) {
            continue;
        }
        count += 1;
        size += message.size;
    }
    (count, size)
}

/// 把命令参数里的序号转换成邮件；序号从 1 开始，越界返回 `None`。
fn pick_message(snapshot: &[StoredMessage], arg: &str) -> Option<(usize, StoredMessage)> {
    let index = arg.trim().parse::<usize>().ok()?;
    if index == 0 || index > snapshot.len() {
        return None;
    }
    Some((index, snapshot[index - 1]))
}

/// SMTP 落盘时保留了行首的点，POP3 取信时需要再补一个点做转义。
fn dot_stuff(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut at_line_start = true;
    for &byte in data {
        if at_line_start && byte == b'.' {
            out.push(b'.');
        }
        out.push(byte);
        at_line_start = byte == b'\n';
    }
    out
}

/// 取信件的头部加上正文的前 `lines` 行，供 TOP 命令使用。
fn top_of_message(data: &[u8], lines: usize) -> Vec<u8> {
    let (header, body) = split_header_body(data);
    let mut out = Vec::with_capacity(header.len() + 64);
    out.extend_from_slice(header);
    out.extend_from_slice(b"\r\n");
    for line in crlf_lines(body).into_iter().take(lines) {
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// 以空行（CRLF CRLF）为界拆分头部与正文。
fn split_header_body(data: &[u8]) -> (&[u8], &[u8]) {
    if let Some(position) = find_subslice(data, b"\r\n\r\n") {
        // 头部保留结尾的 CRLF，但不再包含作为分隔的空行。
        (&data[..position + 2], &data[position + 4..])
    } else {
        (data, &[])
    }
}

/// 在字节串中查找子串首次出现的位置。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 按 CRLF 切分行，返回去掉行尾的内容。
fn crlf_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, &byte) in data.iter().enumerate() {
        if byte == b'\n' {
            let end = if index > start && data[index - 1] == b'\r' {
                index - 1
            } else {
                index
            };
            lines.push(&data[start..end]);
            start = index + 1;
        }
    }
    if start < data.len() {
        lines.push(&data[start..]);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    // 行首的点必须被重复一次，行中间的点不动。
    #[test]
    fn dot_stuffing() {
        let data = b"line1\r\n.dot\r\n..two\r\n";
        let stuffed = dot_stuff(data);
        assert_eq!(
            String::from_utf8(stuffed).unwrap(),
            "line1\r\n..dot\r\n...two\r\n"
        );
    }

    // TOP 要返回完整头部和指定行数的正文。
    #[test]
    fn top_lines() {
        let data = b"Subject: hi\r\nFrom: a\r\n\r\nline1\r\nline2\r\nline3\r\n";
        let out = top_of_message(data, 2);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Subject: hi\r\nFrom: a\r\n\r\nline1\r\nline2\r\n"
        );
    }

    // 未删除的邮件参与统计。
    #[test]
    fn stats_skip_deleted() {
        let snapshot = vec![
            StoredMessage { uid: 1, size: 10 },
            StoredMessage { uid: 2, size: 20 },
        ];
        let mut deleted = HashSet::new();
        assert_eq!(visible_stats(&snapshot, &deleted), (2, 30));
        deleted.insert(1);
        assert_eq!(visible_stats(&snapshot, &deleted), (1, 20));
        assert_eq!(pick_message(&snapshot, "2").unwrap().0, 2);
        assert!(pick_message(&snapshot, "3").is_none());
        assert!(pick_message(&snapshot, "0").is_none());
    }
}
