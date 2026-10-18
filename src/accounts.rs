//! `accounts.toml` 的读取、校验与写回。
//!
//! 文件格式极简，一行一个账户：
//!
//! ```toml
//! alice = "enc:v1:......"
//! bob   = "enc:v1:......"
//! ```
//!
//! 值必须是加密后的密文，出现明文会在加载时报错并提示如何加密。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::crypto::{is_encrypted, Cipher};

/// 账户集合。明文密码只在内存中出现，写盘前一定会重新加密。
pub struct Accounts {
    /// 账户文件路径。
    pub path: PathBuf,
    /// 账户名（统一小写）到明文密码的映射。
    pub map: BTreeMap<String, String>,
}

impl Accounts {
    /// 创建一个空账户集合，用于首次生成 `accounts.toml`。
    pub fn empty(path: PathBuf) -> Self {
        Self {
            path,
            map: BTreeMap::new(),
        }
    }

    /// 加载并校验账户文件；任何账户的密码未加密都会直接报错。
    pub fn load(path: &Path, cipher: &Cipher) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取账户文件失败: {}", path.display()))?;
        let table: toml::Table = toml::from_str(&text)
            .with_context(|| format!("解析账户文件失败，请检查 TOML 语法: {}", path.display()))?;

        let mut map = BTreeMap::new();
        for (key, value) in table.iter() {
            // 账户名会直接用作邮箱目录名，必须先做合法性校验。
            let name = normalize_account_name(key)
                .with_context(|| format!("账户文件 {} 中的账户名 `{key}` 不合法", path.display()))?;
            let stored = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("账户 `{name}` 的值必须是字符串"))?;
            if !is_encrypted(stored) {
                bail!(
                    "账户 `{name}` 的密码是明文，必须先加密：realemail passwd {name}（或 realemail encrypt <密码>）"
                );
            }
            let plaintext = cipher
                .decrypt(stored)
                .with_context(|| format!("账户 `{name}` 的密码解密失败，请确认 mail.key 是否匹配"))?;
            map.insert(name, plaintext);
        }

        if map.is_empty() {
            bail!("账户文件 {} 中没有任何账户", path.display());
        }
        Ok(Self {
            path: path.to_path_buf(),
            map,
        })
    }

    /// 把内存中的账户重新加密后整体写回文件（先写临时文件再改名）。
    pub fn save(&self, cipher: &Cipher) -> Result<()> {
        let mut out = String::new();
        out.push_str("# RealEmail 账户文件\n");
        out.push_str("# 密码由 AES-256-GCM 加密，请勿手工填写明文。\n");
        out.push_str("# 新增账户: realemail add <账户名>\n");
        out.push_str("# 修改密码: realemail passwd <账户名>\n\n");
        for (name, plaintext) in &self.map {
            let encrypted = cipher.encrypt(plaintext)?;
            out.push_str(&format!("{name} = \"{encrypted}\"\n"));
        }

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("accounts.toml");
        let tmp = self.path.with_file_name(format!(".{file_name}.tmp"));
        std::fs::write(&tmp, out)
            .with_context(|| format!("写入临时账户文件失败: {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("替换账户文件失败: {}", self.path.display()))?;
        Ok(())
    }

    /// 校验账户名与密码是否匹配（常数时间比较）。
    pub fn verify(&self, name: &str, password: &str) -> bool {
        match self.map.get(&name.to_ascii_lowercase()) {
            Some(expected) => crate::crypto::constant_time_eq(expected, password),
            None => false,
        }
    }
}

/// 规范化并校验账户名：允许字母、数字以及 `.` `_` `-` `+`，统一转小写。
pub fn normalize_account_name(raw: &str) -> Result<String> {
    let name = raw.trim().to_ascii_lowercase();
    if name.is_empty() {
        bail!("账户名不能为空");
    }
    if name.len() > 64 {
        bail!("账户名过长（最多 64 个字符）");
    }
    if name.starts_with('.') {
        bail!("账户名不能以 `.` 开头");
    }
    for ch in name.chars() {
        if !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '+')) {
            bail!("账户名 `{raw}` 含有非法字符 `{ch}`（允许：a-z 0-9 . _ - +）");
        }
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 合法的账户名会被统一转成小写。
    #[test]
    fn 账户名规范化() {
        assert_eq!(normalize_account_name(" Alice ").unwrap(), "alice");
        assert_eq!(normalize_account_name("bob.smith+tag").unwrap(), "bob.smith+tag");
    }

    // 带非法字符或空名字的账户名必须被拒绝。
    #[test]
    fn 非法账户名被拒绝() {
        assert!(normalize_account_name("").is_err());
        assert!(normalize_account_name("a/b").is_err());
        assert!(normalize_account_name("..hidden").is_err());
        assert!(normalize_account_name("张三").is_err());
    }

    // 保存到磁盘再读回来，密码应当保持一致。
    #[test]
    fn 存盘往返() {
        let dir = std::env::temp_dir().join(format!("realemail-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        let cipher = Cipher::from_key([9u8; 32]);

        let mut accounts = Accounts::empty(path.clone());
        accounts.map.insert("alice".into(), "secret1".into());
        accounts.map.insert("bob".into(), "secret2".into());
        accounts.save(&cipher).unwrap();

        let loaded = Accounts::load(&path, &cipher).unwrap();
        assert!(loaded.verify("alice", "secret1"));
        assert!(!loaded.verify("alice", "wrong"));
        assert!(loaded.verify("BOB", "secret2"));
        assert!(!loaded.verify("carol", "secret2"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
