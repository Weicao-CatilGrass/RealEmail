//! 网络连接与按行读写的公共工具。
//!
//! SMTP 与 POP3 都是逐行交互的文本协议，这里把“读一行、写一行、空闲超时”
//! 等细节统一封装，两个服务端只需要关心各自的状态机。

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// 连接空闲多久没有新数据就主动断开，避免半开连接长期占用资源。
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// 单行数据的硬性上限，防止恶意客户端用超长行耗尽内存。
pub const MAX_LINE: usize = 512 * 1024;

/// 命令行（SMTP/POP3 指令）的长度上限，RFC 建议不超过 512 字节，这里放宽一些。
pub const MAX_COMMAND_LINE: usize = 2048;

/// 一条已经建立好的 TCP 连接。
pub struct Conn {
    peer: SocketAddr,
    reader: BufReader<OwnedReadHalf>,
    writer: BufWriter<OwnedWriteHalf>,
}

impl Conn {
    /// 包装一条原始 TCP 连接。
    pub fn new(stream: TcpStream) -> Result<Self> {
        let peer = stream.peer_addr().context("获取对端地址失败")?;
        // 邮件协议大量小包往返，关闭 Nagle 能明显降低交互延迟。
        let _ = stream.set_nodelay(true);
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            peer,
            reader: BufReader::with_capacity(8 * 1024, read_half),
            writer: BufWriter::with_capacity(8 * 1024, write_half),
        })
    }

    /// 对端地址，用于日志。
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// 读取一行，返回去掉行尾 CRLF/LF 的内容；`None` 表示对端已关闭连接。
    pub async fn read_line(&mut self) -> Result<Option<Vec<u8>>> {
        let mut buf = Vec::new();
        loop {
            // 先取出缓冲区里的这一段数据并复制成独立缓冲，
            // 这样才能在该段数据仍被引用时调用 consume。
            let (chunk, eof) = {
                let available = match timeout(IDLE_TIMEOUT, self.reader.fill_buf()).await {
                    Ok(Ok(data)) => data,
                    Ok(Err(err)) => return Err(err.into()),
                    Err(_) => bail!("idle timeout"),
                };
                if available.is_empty() {
                    (Vec::new(), true)
                } else {
                    (available.to_vec(), false)
                }
            };

            if eof {
                // 对端关闭：若有残留数据就当作最后一行处理。
                if buf.is_empty() {
                    return Ok(None);
                }
                break;
            }

            if let Some(pos) = chunk.iter().position(|&byte| byte == b'\n') {
                buf.extend_from_slice(&chunk[..=pos]);
                self.reader.consume(pos + 1);
                break;
            }

            buf.extend_from_slice(&chunk);
            self.reader.consume(chunk.len());
            if buf.len() > MAX_LINE {
                bail!("line too long");
            }
        }

        if buf.len() > MAX_LINE {
            bail!("line too long");
        }
        // 去掉行尾的 \r\n，也兼容只发 \n 或 \r 的客户端。
        while matches!(buf.last(), Some(b'\r') | Some(b'\n')) {
            buf.pop();
        }
        Ok(Some(buf))
    }

    /// 写一行并补上 CRLF，然后立即刷新。
    pub async fn write_line(&mut self, line: &str) -> Result<()> {
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.write_all(b"\r\n").await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// 原样写入一段二进制数据（用于 RETR 这类大块响应）。
    pub async fn write_bytes(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data).await?;
        Ok(())
    }

    /// 刷新写缓冲。
    pub async fn flush(&mut self) -> Result<()> {
        self.writer.flush().await?;
        Ok(())
    }
}

/// 把一行拆成“命令字”和“参数”两部分。
///
/// 命令字原样返回（由调用方决定是否转大写），参数去掉首尾空白。
pub fn split_command(line: &str) -> (&str, &str) {
    match line.find([' ', '\t']) {
        Some(index) => (&line[..index], line[index + 1..].trim()),
        None => (line, ""),
    }
}

/// 判断连接错误是否属于“客户端正常断开”，这类情况不需要打印日志。
pub fn is_normal_disconnect(message: &str) -> bool {
    message == "idle timeout"
        || message.contains("aborted")
        || message.contains("reset by peer")
        || message.contains("Broken pipe")
        || message.contains("unexpected end of file")
        || message.contains("connection closed")
}

#[cfg(test)]
mod tests {
    use super::*;

    // 命令与参数应正确拆分，且多余空白被清理。
    #[test]
    fn split_works() {
        assert_eq!(split_command("RETR 3"), ("RETR", "3"));
        assert_eq!(split_command("quit"), ("quit", ""));
        assert_eq!(split_command("PASS   a b c "), ("PASS", "a b c"));
        assert_eq!(split_command("MAIL\tFROM:<a@b>"), ("MAIL", "FROM:<a@b>"));
    }

    // 常见的断开错误不应被当成异常。
    #[test]
    fn disconnect_detection() {
        assert!(is_normal_disconnect("idle timeout"));
        assert!(is_normal_disconnect(
            "An established connection was aborted by the software in your host machine. (os error 10053)"
        ));
        assert!(!is_normal_disconnect("line too long"));
    }
}
