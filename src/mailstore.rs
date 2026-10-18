//! 邮件落盘存储。
//!
//! 目录结构：`<数据目录>/<账户名>/<uid>.eml`，另外有一个 `uidnext` 文件记录
//! 下一个可用的 UID，保证重启后编号继续递增，不会重复使用已删除的编号。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

/// 邮箱中的一封邮件。
#[derive(Debug, Clone, Copy)]
pub struct StoredMessage {
    /// 邮件编号，POP3 的 UIDL 直接使用它。
    pub uid: u64,
    /// 邮件字节数（其中换行统一为 CRLF，与 POP3 的 OCTETS 计算方式一致）。
    pub size: u64,
}

/// 邮件存储句柄，可以被多个连接共享。
#[derive(Clone)]
pub struct MailStore {
    root: PathBuf,
    /// 分配 UID 与写盘需要串行执行，避免并发投递抢到同一个编号。
    alloc: Arc<Mutex<()>>,
}

impl MailStore {
    /// 创建存储对象并确保根目录存在。
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("创建邮件根目录失败: {}", root.display()))?;
        Ok(Self {
            root,
            alloc: Arc::new(Mutex::new(())),
        })
    }

    /// 数据根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 单个账户的邮箱目录。
    fn account_dir(&self, account: &str) -> PathBuf {
        self.root.join(account)
    }

    /// 确保账户邮箱目录存在（启动时对每个账户调用一次）。
    pub fn ensure_account(&self, account: &str) -> Result<()> {
        let dir = self.account_dir(account);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建邮箱目录失败: {}", dir.display()))?;
        Ok(())
    }

    /// 投递一封邮件，返回分配到的 UID。
    pub async fn deliver(&self, account: &str, data: &[u8]) -> Result<u64> {
        self.ensure_account(account)?;
        let _guard = self.alloc.lock().await;

        let dir = self.account_dir(account);
        let uid = next_uid(&dir)?;

        // 先写临时文件再重命名，保证 POP3 永远读不到写了一半的邮件。
        let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), uid));
        tokio::fs::write(&tmp, data)
            .await
            .with_context(|| format!("写入临时邮件文件失败: {}", tmp.display()))?;
        let final_path = dir.join(message_file_name(uid));
        tokio::fs::rename(&tmp, &final_path)
            .await
            .with_context(|| format!("邮件改名失败: {}", final_path.display()))?;

        // 更新下一个可用编号。
        tokio::fs::write(dir.join("uidnext"), format!("{}\n", uid + 1)).await?;
        Ok(uid)
    }

    /// 按 UID 升序返回邮箱中当前所有邮件。
    pub async fn snapshot(&self, account: &str) -> Result<Vec<StoredMessage>> {
        let dir = self.account_dir(account);
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            // 邮箱目录还没建立，视为空邮箱。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("读取邮箱目录失败: {}", dir.display()))
            }
        };

        let mut messages = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(stem) = name.strip_suffix(".eml") else {
                continue;
            };
            let Ok(uid) = stem.parse::<u64>() else {
                continue;
            };
            let meta = entry.metadata().await?;
            messages.push(StoredMessage {
                uid,
                size: meta.len(),
            });
        }
        messages.sort_by_key(|message| message.uid);
        Ok(messages)
    }

    /// 读取整封邮件的原始内容。
    pub async fn read(&self, account: &str, uid: u64) -> Result<Vec<u8>> {
        let path = self.account_dir(account).join(message_file_name(uid));
        tokio::fs::read(&path)
            .await
            .with_context(|| format!("读取邮件失败: {}", path.display()))
    }

    /// 删除一封邮件；文件已不存在时视为成功。
    pub async fn delete(&self, account: &str, uid: u64) -> Result<()> {
        let path = self.account_dir(account).join(message_file_name(uid));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("删除邮件失败: {}", path.display())),
        }
    }
}

/// 邮件文件名：补零到固定宽度，保证按文件名排序等于按 UID 排序。
fn message_file_name(uid: u64) -> String {
    format!("{uid:010}.eml")
}

/// 计算下一封邮件应使用的 UID。
///
/// 优先读取 `uidnext`；文件缺失或损坏时退化为扫描目录取最大编号加一。
fn next_uid(dir: &Path) -> Result<u64> {
    if let Ok(text) = std::fs::read_to_string(dir.join("uidnext")) {
        if let Ok(value) = text.trim().parse::<u64>() {
            return Ok(value.max(1));
        }
    }

    let mut max_uid = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy().to_string();
            if let Some(stem) = name.strip_suffix(".eml") {
                if let Ok(uid) = stem.parse::<u64>() {
                    max_uid = max_uid.max(uid);
                }
            }
        }
    }
    Ok(max_uid + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 连续投递应拿到递增的 UID，重启（重建 MailStore）后仍从旧编号继续。
    #[tokio::test]
    async fn 编号连续递增() {
        let dir = std::env::temp_dir().join(format!("realemail-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let store = MailStore::new(dir.clone()).unwrap();
        let first = store.deliver("alice", b"Subject: 1\r\n\r\nhello\r\n").await.unwrap();
        let second = store.deliver("alice", b"Subject: 2\r\n\r\nworld\r\n").await.unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 2);

        // 模拟重启：新建存储对象，编号应接续为 3。
        let store = MailStore::new(dir.clone()).unwrap();
        let third = store.deliver("alice", b"Subject: 3\r\n\r\n!\r\n").await.unwrap();
        assert_eq!(third, 3);

        let messages = store.snapshot("alice").await.unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].uid, 1);

        store.delete("alice", 2).await.unwrap();
        let messages = store.snapshot("alice").await.unwrap();
        assert_eq!(messages.len(), 2);

        let raw = store.read("alice", 1).await.unwrap();
        assert!(raw.starts_with(b"Subject: 1"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
