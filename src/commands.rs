//! `system` 账户的邮件命令。
//!
//! 发往 `system` 账户的邮件会按主题触发对应动作。目前支持：
//!
//! - 主题 `设置个人介绍`（也接受 `设置简介`、`设置自我介绍`、`set bio`）
//!   → 把邮件正文写入程序目录下的 `<发件人账户>.md`，直接覆盖原文件。
//!
//! 为了让真实邮件客户端的邮件也能正确处理，这里实现了 RFC 2047 编码主题、
//! Base64 / Quoted-Printable 正文、multipart 以及 UTF-8、GBK 等字符集解码。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;

/// 触发命令的系统账户名。
pub const SYSTEM_ACCOUNT: &str = "system";

/// 已支持的系统命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemCommand {
    /// 设置个人介绍：把正文写入 `<账户名>.md`。
    SetBio,
}

/// 命令执行结果，仅用于日志输出。
pub struct CommandOutcome {
    /// 命令的可读名称。
    pub command: &'static str,
    /// 被写入的文件。
    pub path: PathBuf,
}

/// 根据主题匹配命令；主题先做去空白、去尾部冒号、忽略大小写处理。
pub fn match_subject(subject: &str) -> Option<SystemCommand> {
    let normalized = subject
        .trim()
        .trim_end_matches([':', '：'])
        .trim()
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<String>();
    match normalized.as_str() {
        "设置个人介绍" | "设置简介" | "设置自我介绍" | "setbio" | "setprofile" => {
            Some(SystemCommand::SetBio)
        }
        _ => None,
    }
}

/// 处理一封发往 `system` 的邮件。
///
/// `sender` 必须是已经校验过的本地账户名，返回值 `None` 表示主题未命中任何命令。
pub async fn handle(workspace: &Path, sender: &str, raw: &[u8]) -> Result<Option<CommandOutcome>> {
    let (headers, body) = split_headers(raw);
    let subject = header(&headers, "subject")
        .map(decode_rfc2047)
        .unwrap_or_default();

    let Some(command) = match_subject(&subject) else {
        println!("[system] 主题 `{}` 未匹配任何命令，已忽略", subject);
        return Ok(None);
    };

    match command {
        SystemCommand::SetBio => {
            let text = extract_text(&headers, body);
            let content = normalize_text(&text);
            let path = workspace.join(format!("{sender}.md"));
            tokio::fs::write(&path, content.as_bytes())
                .await
                .with_context(|| format!("写入个人介绍失败: {}", path.display()))?;
            Ok(Some(CommandOutcome {
                command: "设置个人介绍",
                path,
            }))
        }
    }
}

/// 取出邮件主题（已解码 RFC 2047），供物流任务使用。
pub fn subject_of(raw: &[u8]) -> String {
    let (headers, _) = split_headers(raw);
    header(&headers, "subject")
        .map(decode_rfc2047)
        .unwrap_or_default()
}

/// 从邮件的 Content-Type 出发提取可读正文。
fn extract_text(headers: &[(String, String)], body: &[u8]) -> String {
    extract_text_inner(headers, body, 0)
}

/// 递归提取正文；multipart 优先取 text/plain，其次取第一个非空的部分。
fn extract_text_inner(headers: &[(String, String)], body: &[u8], depth: usize) -> String {
    let content_type = header(headers, "content-type").unwrap_or("text/plain");
    let (mime, params) = parse_content_type(content_type);

    if mime.starts_with("multipart/") && depth < 4 {
        if let Some(boundary) = param(&params, "boundary") {
            let mut fallback = String::new();
            for part in split_multipart(body, boundary) {
                let (part_headers, part_body) = split_headers(part);
                let text = extract_text_inner(&part_headers, part_body, depth + 1);
                let part_type = parse_content_type(
                    header(&part_headers, "content-type").unwrap_or("text/plain"),
                )
                .0;
                if part_type == "text/plain" && !text.trim().is_empty() {
                    return text;
                }
                if !text.trim().is_empty() && fallback.is_empty() {
                    fallback = text;
                }
            }
            if !fallback.is_empty() {
                return fallback;
            }
        }
    }

    let bytes = decode_transfer(body, header(headers, "content-transfer-encoding"));
    let charset = param(&params, "charset").unwrap_or("utf-8");
    decode_bytes(&bytes, charset)
}

/// 按 Content-Transfer-Encoding 还原字节内容。
fn decode_transfer(body: &[u8], encoding: Option<&str>) -> Vec<u8> {
    match encoding.map(|value| value.trim().to_ascii_lowercase()).as_deref() {
        Some("base64") => {
            let cleaned: Vec<u8> = body
                .iter()
                .copied()
                .filter(|byte| !byte.is_ascii_whitespace())
                .collect();
            STANDARD
                .decode(&cleaned)
                .unwrap_or_else(|_| body.to_vec())
        }
        Some("quoted-printable") => decode_quoted_printable(body),
        _ => body.to_vec(),
    }
}

/// 按字符集把字节解码成字符串，未知字符集时按 UTF-8 宽松处理。
fn decode_bytes(bytes: &[u8], charset: &str) -> String {
    if let Some(encoding) = encoding_rs::Encoding::for_label(charset.trim().as_bytes()) {
        let (text, _, _) = encoding.decode(bytes);
        text.into_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// 解析邮件头，返回（小写字段名, 字段值）以及正文起始位置。
///
/// 支持 RFC 5322 的行折叠：以空格或制表符开头的续行会拼接到上一个字段。
fn split_headers(raw: &[u8]) -> (Vec<(String, String)>, &[u8]) {
    let (head, body) = if let Some(position) = find_subslice(raw, b"\r\n\r\n") {
        (&raw[..position], &raw[position + 4..])
    } else if let Some(position) = find_subslice(raw, b"\n\n") {
        (&raw[..position], &raw[position + 2..])
    } else {
        (raw, &raw[raw.len()..])
    };

    let text = String::from_utf8_lossy(head);
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = headers.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    (headers, body)
}

/// 取某个邮件头的值。
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// 解析 `text/plain; charset=utf-8` 这类 Content-Type。
fn parse_content_type(value: &str) -> (String, Vec<(String, String)>) {
    let mut pieces = value.split(';');
    let mime = pieces
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let mut params = Vec::new();
    for piece in pieces {
        if let Some((key, val)) = piece.split_once('=') {
            let val = val.trim().trim_matches('"').to_string();
            params.push((key.trim().to_ascii_lowercase(), val));
        }
    }
    let mime = if mime.is_empty() {
        "text/plain".to_string()
    } else {
        mime
    };
    (mime, params)
}

/// 取 Content-Type 的参数值。
fn param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// 按 boundary 切分 multipart 正文。
fn split_multipart<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let delimiter = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();
    let mut positions = Vec::new();
    let mut cursor = 0usize;
    while cursor <= body.len() {
        match find_subslice(&body[cursor..], &delimiter) {
            Some(offset) => {
                positions.push(cursor + offset);
                cursor += offset + delimiter.len();
            }
            None => break,
        }
    }

    for window in positions.windows(2) {
        let start = window[0] + delimiter.len();
        let end = window[1];
        if end <= start {
            continue;
        }
        let part = trim_leading_crlf(&body[start..end]);
        // 以 "--" 开头的是结束分隔线，直接跳过。
        if part.starts_with(b"--") {
            continue;
        }
        parts.push(part);
    }
    parts
}

/// 去掉字节串开头的空行。
fn trim_leading_crlf(mut data: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = data.split_first() {
        if first == b'\r' || first == b'\n' {
            data = rest;
        } else {
            break;
        }
    }
    data
}

/// 解码 RFC 2047 形式的主题，例如 `=?UTF-8?B?6K6+572u?=`。
fn decode_rfc2047(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    let mut previous_was_encoded = false;

    while let Some(start) = rest.find("=?") {
        out.push_str(&rest[..start]);
        if previous_was_encoded {
            // 相邻两个编码词之间的空白不应出现在结果里。
            while out.ends_with(' ') {
                out.pop();
            }
        }
        let after = &rest[start + 2..];
        match parse_encoded_word(after) {
            Some((decoded, consumed)) => {
                out.push_str(&decoded);
                rest = &after[consumed..];
                previous_was_encoded = true;
            }
            None => {
                out.push_str("=?");
                rest = after;
                previous_was_encoded = false;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 解析一个编码词，返回解码结果以及消耗掉的字节数。
fn parse_encoded_word(input: &str) -> Option<(String, usize)> {
    let charset_end = input.find('?')?;
    let charset = &input[..charset_end];
    let rest = &input[charset_end + 1..];
    let encoding_end = rest.find('?')?;
    let encoding = rest[..encoding_end].to_ascii_uppercase();
    let rest = &rest[encoding_end + 1..];
    let text_end = rest.find("?=")?;
    let encoded = &rest[..text_end];
    let consumed = charset_end + 1 + encoding_end + 1 + text_end + 2;

    let bytes = match encoding.as_str() {
        "B" => STANDARD.decode(encoded.trim()).ok()?,
        "Q" => decode_quoted_printable(encoded.replace('_', " ").as_bytes()),
        _ => return None,
    };
    Some((decode_bytes(&bytes, charset), consumed))
}

/// 解码 Quoted-Printable 内容。
fn decode_quoted_printable(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut index = 0usize;
    while index < data.len() {
        match data[index] {
            b'=' => {
                // 软换行：`=` 后直接跟换行表示该行未结束。
                if index + 1 < data.len() && data[index + 1] == b'\n' {
                    index += 2;
                    continue;
                }
                if index + 2 < data.len()
                    && data[index + 1] == b'\r'
                    && data[index + 2] == b'\n'
                {
                    index += 3;
                    continue;
                }
                if index + 2 < data.len() {
                    if let (Some(high), Some(low)) =
                        (hex_value(data[index + 1]), hex_value(data[index + 2]))
                    {
                        out.push(high * 16 + low);
                        index += 3;
                        continue;
                    }
                }
                out.push(b'=');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

/// 单个十六进制字符转数值。
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 统一换行为 LF，去掉尾部空白，并保证文件以单个换行结尾。
fn normalize_text(text: &str) -> String {
    let unified = text.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = unified.trim_end();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

/// 在字节串里查找子串首次出现的位置。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 主题匹配忽略空白、大小写和尾部冒号。
    #[test]
    fn subject_matching() {
        assert_eq!(match_subject("设置个人介绍"), Some(SystemCommand::SetBio));
        assert_eq!(match_subject("  设置个人介绍： "), Some(SystemCommand::SetBio));
        assert_eq!(match_subject("Set  Bio"), Some(SystemCommand::SetBio));
        assert_eq!(match_subject("删除账号"), None);
    }

    // RC 2047 的 Base64 与 Quoted-Printable 主题都应能还原。
    #[test]
    fn rfc2047_subject() {
        // "设置个人介绍" 的 UTF-8 Base64
        assert_eq!(
            decode_rfc2047("=?UTF-8?B?6K6+572u5Liq5Lq65LuL57uN?="),
            "设置个人介绍"
        );
        assert_eq!(decode_rfc2047("=?UTF-8?Q?hello?="), "hello");
    }

    // 纯文本正文按原样取出。
    #[test]
    fn plain_body() {
        let raw = b"Subject: hi\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n\xe4\xbd\xa0\xe5\xa5\xbd\r\n";
        let (headers, body) = split_headers(raw);
        assert_eq!(extract_text(&headers, body).trim(), "你好");
    }

    // Quoted-Printable 正文需要先解码再写文件。
    #[test]
    fn quoted_printable_body() {
        let raw = b"Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n\xe4\xbd\xa0=\r\n\xe5\xa5\xbd\r\n";
        let (headers, body) = split_headers(raw);
        assert_eq!(extract_text(&headers, body).trim(), "你好");
    }

    // 尾部换行会被整理成单个 \n。
    #[test]
    fn normalize_text_works() {
        assert_eq!(normalize_text("a\r\nb\r\n\r\n"), "a\nb\n");
        assert_eq!(normalize_text("   "), "");
    }
}
