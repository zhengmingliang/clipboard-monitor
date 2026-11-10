//! 剪贴板正文的静态加密（AES-256-GCM，字段级）。
//!
//! 加密覆盖数据库里真正含剪贴板内容的字段：`contents.body` / `blob` /
//! `meta_json` 与 `events.summary`。时间、类型、应用名、PID 等元数据列保持
//! 明文，以便索引、筛选与 FTS5 检索；这些列本身不含正文。
//!
//! 密钥是 32 字节随机值，优先由系统钥匙串保护：
//! - Windows: DPAPI (`CryptProtectData`，绑定当前用户账户)；
//! - macOS: Keychain 通用密码项（`security` 命令）；
//! - Linux / 钥匙串不可用: 数据目录内 0600 的密钥文件。
//!
//! 密文格式：`"enc:v1:" + base64(nonce[12] || ciphertext || tag[16])`。
//! 读取时兼容不带前缀的历史明文，实现渐进迁移（见 `db.rs` 的迁移逻辑）。
//!
//! 这不是全库加密：拿到数据目录且能绕过 OS 账户隔离的攻击者仍可解密。
//! 需要全库加密时应改用 SQLCipher；本方案的目标是在不引入 C 交叉编译
//! 依赖、不破坏五架构 CI 的前提下，消除"数据库文件里能直接 grep 到密码"。

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{bail, Context, Result};
use base64::Engine;
use std::fs;
use std::path::Path;
use zeroize::Zeroizing;

const PREFIX: &str = "enc:v1:";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const KEY_FILE: &str = "master.key";
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "clipboard-monitor";
#[cfg(target_os = "macos")]
const KEYCHAIN_ACCOUNT: &str = "database-key";
#[cfg(target_os = "macos")]
fn keychain_account(dir: &Path) -> String {
    // 同一台机器上不同 --data-dir 的实例互不覆盖。
    let tag = crate::classify::sha256_hex(dir.to_string_lossy().as_bytes());
    format!("{KEYCHAIN_ACCOUNT}-{}", &tag[..12])
}

/// 数据库加密密钥。内存中用 `Zeroizing` 包装，drop 时清零。
pub struct DbKey(Zeroizing<[u8; KEY_LEN]>);

impl DbKey {
    /// 从数据目录加载或创建密钥。
    ///
    /// 首次运行生成随机密钥并写入钥匙串（失败则回落密钥文件）。已有密钥
    /// 文件但钥匙串可用时，会把密钥迁移进钥匙串并删除明文文件。
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        if let Some(key) = Self::from_keychain(dir)? {
            return Ok(Self(Zeroizing::new(key)));
        }
        let path = dir.join(KEY_FILE);
        if path.exists() {
            let bytes = fs::read(&path).with_context(|| format!("读取密钥 {}", path.display()))?;
            let key = decode_key(&bytes)?;
            // 钥匙串这次可用：迁移进去，之后不再留明文文件。
            if store_in_keychain(dir, &key) {
                let _ = fs::remove_file(&path);
            }
            return Ok(Self(Zeroizing::new(key)));
        }
        let key: [u8; KEY_LEN] = Aes256Gcm::generate_key(&mut OsRng).into();
        if !store_in_keychain(dir, &key) {
            write_key_file(&path, &key)
                .with_context(|| format!("写入密钥文件 {}", path.display()))?;
        }
        Ok(Self(Zeroizing::new(key)))
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<String> {
        let cipher = Aes256Gcm::new_from_slice(self.0.as_slice()).context("初始化 AES-GCM")?;
        let nonce_bytes: [u8; NONCE_LEN] = Aes256Gcm::generate_nonce(&mut OsRng).into();
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
            .map_err(|e| anyhow::anyhow!("加密失败: {e}"))?;
        let mut payload = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        payload.extend_from_slice(&nonce_bytes);
        payload.extend_from_slice(&ciphertext);
        Ok(format!(
            "{PREFIX}{}",
            base64::engine::general_purpose::STANDARD.encode(payload)
        ))
    }

    /// 解密；不带 `enc:v1:` 前缀的值视为历史明文，原样返回（渐进迁移）。
    pub fn decrypt(&self, stored: &str) -> Result<Vec<u8>> {
        let Some(encoded) = stored.strip_prefix(PREFIX) else {
            return Ok(stored.as_bytes().to_vec());
        };
        let payload = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("密文 base64 无效")?;
        if payload.len() < NONCE_LEN + 16 {
            bail!("密文长度不足");
        }
        let (nonce, ciphertext) = payload.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new_from_slice(self.0.as_slice()).context("初始化 AES-GCM")?;
        cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|e| anyhow::anyhow!("解密失败（密钥不匹配或数据被篡改）: {e}"))
    }

    pub fn encrypt_text(&self, text: Option<&str>) -> Result<Option<String>> {
        text.map(|t| self.encrypt(t.as_bytes())).transpose()
    }

    pub fn decrypt_text(&self, stored: Option<&str>) -> Result<Option<String>> {
        stored
            .map(|s| {
                self.decrypt(s)
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
            })
            .transpose()
    }

    #[cfg(not(target_os = "windows"))]
    fn from_keychain(dir: &Path) -> Result<Option<[u8; KEY_LEN]>> {
        #[cfg(target_os = "macos")]
        {
            keychain_read(dir)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = dir;
            Ok(None)
        }
    }

    #[cfg(target_os = "windows")]
    fn from_keychain(dir: &Path) -> Result<Option<[u8; KEY_LEN]>> {
        dpapi_read(dir)
    }
}

fn decode_key(bytes: &[u8]) -> Result<[u8; KEY_LEN]> {
    let text = String::from_utf8_lossy(bytes);
    let encoded = text.trim().strip_prefix("key:v1:").unwrap_or(text.trim());
    let raw = hex::decode(encoded).context("密钥文件不是合法十六进制")?;
    if raw.len() != KEY_LEN {
        bail!("密钥长度应为 {KEY_LEN} 字节，实际 {}", raw.len());
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&raw);
    Ok(key)
}

fn write_key_file(path: &Path, key: &[u8; KEY_LEN]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, format!("key:v1:{}", hex::encode(key)))?;
    crate::config::restrict_file(path);
    Ok(())
}

fn store_in_keychain(dir: &Path, key: &[u8; KEY_LEN]) -> bool {
    #[cfg(target_os = "windows")]
    {
        dpapi_store(dir, key)
    }
    #[cfg(target_os = "macos")]
    {
        keychain_store(dir, key)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (dir, key);
        false
    }
}

// ---------------------------------------------------------------- Windows DPAPI

#[cfg(target_os = "windows")]
fn dpapi_blob_path(dir: &Path) -> std::path::PathBuf {
    dir.join("master.key.dpapi")
}

#[cfg(target_os = "windows")]
fn dpapi_store(dir: &Path, key: &[u8; KEY_LEN]) -> bool {
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: key.len() as u32,
        pbData: key.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &in_blob,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut out_blob,
        )
    };
    if ok == 0 || out_blob.pbData.is_null() {
        return false;
    }
    let bytes = unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) };
    let path = dpapi_blob_path(dir);
    let written = (|| -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, bytes)?;
        crate::config::restrict_file(&path);
        Ok(())
    })()
    .is_ok();
    unsafe { windows_sys::Win32::Foundation::LocalFree(out_blob.pbData as *mut core::ffi::c_void) };
    written
}

#[cfg(target_os = "windows")]
fn dpapi_read(dir: &Path) -> Result<Option<[u8; KEY_LEN]>> {
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    let path = dpapi_blob_path(dir);
    if !path.exists() {
        return Ok(None);
    }
    let protected = fs::read(&path).context("读取 DPAPI 保护的密钥")?;
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: protected.len() as u32,
        pbData: protected.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &in_blob,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut out_blob,
        )
    };
    if ok == 0 || out_blob.pbData.is_null() {
        return Ok(None); // 换用户/域环境时解密失败，回退密钥文件
    }
    let raw = unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) };
    let result = if raw.len() == KEY_LEN {
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(raw);
        Some(key)
    } else {
        None
    };
    unsafe { windows_sys::Win32::Foundation::LocalFree(out_blob.pbData as *mut core::ffi::c_void) };
    Ok(result)
}

// ---------------------------------------------------------------- macOS Keychain

#[cfg(target_os = "macos")]
fn keychain_store(dir: &Path, key: &[u8; KEY_LEN]) -> bool {
    use std::process::Command;
    let encoded = hex::encode(key);
    // -U 更新已存在项；失败（无图形会话/钥匙串锁定）时回退密钥文件。
    Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            &keychain_account(dir),
            "-w",
            &encoded,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(target_os = "macos")]
fn keychain_read(dir: &Path) -> Result<Option<[u8; KEY_LEN]>> {
    use std::process::Command;
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            &keychain_account(dir),
            "-w",
        ])
        .output();
    let Ok(out) = output else {
        return Ok(None);
    };
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(decode_key(text.trim().as_bytes()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> DbKey {
        DbKey(Zeroizing::new([7u8; KEY_LEN]))
    }

    #[test]
    fn roundtrip_text_and_binary() {
        let key = test_key();
        let text = "密码 hunter2 / 中文 / emoji 🔐";
        let encrypted = key.encrypt(text.as_bytes()).unwrap();
        assert!(encrypted.starts_with(PREFIX));
        assert!(!encrypted.contains("hunter2"));
        assert_eq!(key.decrypt(&encrypted).unwrap(), text.as_bytes());
        let binary = vec![0u8, 1, 2, 255, 128];
        assert_eq!(key.decrypt(&key.encrypt(&binary).unwrap()).unwrap(), binary);
    }

    #[test]
    fn legacy_plaintext_is_readable_for_gradual_migration() {
        let key = test_key();
        assert_eq!(
            key.decrypt("历史明文正文").unwrap(),
            "历史明文正文".as_bytes()
        );
        assert_eq!(
            key.decrypt_text(Some("old body")).unwrap().as_deref(),
            Some("old body")
        );
        assert_eq!(key.decrypt_text(None).unwrap(), None);
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let a = DbKey(Zeroizing::new([1u8; KEY_LEN]));
        let b = DbKey(Zeroizing::new([2u8; KEY_LEN]));
        let encrypted = a.encrypt(b"secret").unwrap();
        assert!(b.decrypt(&encrypted).is_err());
    }

    #[test]
    fn tampered_or_truncated_ciphertext_is_rejected() {
        let key = test_key();
        assert!(key.decrypt("enc:v1:!!!!").is_err());
        assert!(key.decrypt("enc:v1:AAAA").is_err());
    }

    #[test]
    fn key_file_is_hex_and_restricted() {
        let dir = std::env::temp_dir().join(format!(
            "clipmon-key-test-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(KEY_FILE);
        let key = [0xABu8; KEY_LEN];
        write_key_file(&path, &key).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("key:v1:"));
        assert_eq!(decode_key(content.as_bytes()).unwrap(), key);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_dir_all(dir);
    }
}
