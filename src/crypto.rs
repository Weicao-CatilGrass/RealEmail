//! 密码加解密。
//!
//! `accounts.toml` 中不允许出现明文密码，统一保存为 `enc:v1:<base64>` 形式，
//! 加密算法为 AES-256-GCM（随机 12 字节 nonce，密文尾部自带 16 字节认证标签）。
//!
//! 密钥保存在与 `accounts.toml` 同一目录的 `mail.key` 中，首次运行时自动生成。
//! 密钥一旦丢失，已加密的密码无法恢复，只能重新设置。

use std::fs;
use std::path::Path;

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use subtle::ConstantTimeEq;

/// 密文的固定前缀，用于识别“已加密”的值以及区分后续版本。
pub const PREFIX: &str = "enc:v1:";

/// AES-256 要求的密钥长度。
const KEY_LEN: usize = 32;

/// GCM 推荐的 nonce 长度。
const NONCE_LEN: usize = 12;

/// 持有主密钥的加解密器。
pub struct Cipher {
    key: [u8; KEY_LEN],
}

impl Cipher {
    /// 从文件加载密钥；文件不存在时自动生成一个新的随机密钥。
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let text = fs::read_to_string(path)
                .with_context(|| format!("读取密钥文件失败: {}", path.display()))?;
            let bytes = URL_SAFE_NO_PAD
                .decode(text.trim())
                .with_context(|| format!("密钥文件内容不是合法的 Base64: {}", path.display()))?;
            if bytes.len() != KEY_LEN {
                bail!(
                    "密钥文件 {} 长度不正确：需要 {} 字节，实际 {} 字节",
                    path.display(),
                    KEY_LEN,
                    bytes.len()
                );
            }
            let mut key = [0u8; KEY_LEN];
            key.copy_from_slice(&bytes);
            return Ok(Self { key });
        }

        // 首次运行：生成 32 字节随机密钥写盘。
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let mut key = [0u8; KEY_LEN];
        let mut rng = OsRng;
        rng.fill_bytes(&mut key);
        fs::write(path, URL_SAFE_NO_PAD.encode(key))
            .with_context(|| format!("写入密钥文件失败: {}", path.display()))?;
        restrict_permissions(path);
        eprintln!(
            "已生成新的加密密钥: {}（请妥善保管，丢失后已加密的密码将无法解密）",
            path.display()
        );
        Ok(Self { key })
    }

    /// 加密明文密码，返回可直接写入 `accounts.toml` 的字符串。
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let cipher = self.engine()?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| anyhow!("密码加密失败"))?;
        // 密文布局：nonce(12) || ciphertext || tag(16)
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(nonce.as_slice());
        blob.extend_from_slice(&ciphertext);
        Ok(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(blob)))
    }

    /// 解密 `accounts.toml` 中的密文值。
    pub fn decrypt(&self, value: &str) -> Result<String> {
        let body = value
            .strip_prefix(PREFIX)
            .ok_or_else(|| anyhow!("密码不是 enc:v1: 格式的密文"))?;
        let blob = URL_SAFE_NO_PAD
            .decode(body)
            .context("密文不是合法的 Base64")?;
        if blob.len() < NONCE_LEN + 16 {
            bail!("密文长度不足，文件可能已损坏");
        }
        let cipher = self.engine()?;
        let nonce = Nonce::from_slice(&blob[..NONCE_LEN]);
        let plaintext = cipher
            .decrypt(nonce, &blob[NONCE_LEN..])
            .map_err(|_| anyhow!("密码解密失败，密钥与密文不匹配"))?;
        String::from_utf8(plaintext).context("解密结果不是合法的 UTF-8 文本")
    }

    /// 构造 AES-256-GCM 实例。
    fn engine(&self) -> Result<Aes256Gcm> {
        Aes256Gcm::new_from_slice(&self.key).map_err(|_| anyhow!("密钥长度不合法"))
    }

    /// 用给定密钥构造加解密器，仅供单元测试使用。
    #[cfg(test)]
    pub fn from_key(key: [u8; KEY_LEN]) -> Self {
        Self { key }
    }
}

/// 判断某个值是否已经是我们支持的加密格式。
pub fn is_encrypted(value: &str) -> bool {
    value.starts_with(PREFIX)
}

/// 常数时间比较两个字符串，避免通过响应时间猜测密码。
pub fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes().ct_eq(right.as_bytes()).into()
}

/// 在类 Unix 系统上把密钥文件权限收紧到 0600，Windows 上不处理。
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    // 加密后再解密应还原出原始密码，且两次加密结果不同（随机 nonce）。
    #[test]
    fn roundtrip() {
        let cipher = Cipher::from_key([7u8; KEY_LEN]);
        let first = cipher.encrypt("p@ss word").unwrap();
        let second = cipher.encrypt("p@ss word").unwrap();
        assert!(is_encrypted(&first));
        assert_ne!(first, second);
        assert_eq!(cipher.decrypt(&first).unwrap(), "p@ss word");
        assert_eq!(cipher.decrypt(&second).unwrap(), "p@ss word");
    }

    // 换一个密钥必须解密失败。
    #[test]
    fn wrong_key_fails() {
        let a = Cipher::from_key([1u8; KEY_LEN]);
        let b = Cipher::from_key([2u8; KEY_LEN]);
        let value = a.encrypt("secret").unwrap();
        assert!(b.decrypt(&value).is_err());
    }

    // 明文不能被当成密文接受。
    #[test]
    fn plaintext_rejected() {
        assert!(!is_encrypted("plain-text-password"));
        let cipher = Cipher::from_key([3u8; KEY_LEN]);
        assert!(cipher.decrypt("plain-text-password").is_err());
    }
}
