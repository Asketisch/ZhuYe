//! # zhuye-secrets — 密钥加密存储(v1.9 S8)
//!
//! 替代"API key 明文写在 config.toml"。对标 PI-Desktop
//! `host-core/src/secrets.rs`(AES-256-GCM + machine key)。
//!
//! ## 加密方案
//!
//! - **machine key**:32 字节随机密钥,首次使用时生成,落盘
//!   `<dir>/.machine.key`,unix 权限 **0600**。已存在但权限过宽
//!   (group/other 可读)→ **打开失败**(fail-closed:密钥文件
//!   可被同机其他用户读等于没加密)。
//! - **每条目派生**:HKDF-SHA256(machine_key, salt=每条目 16 字节
//!   随机)→ 32 字节条目密钥。同明文两次写入产生不同密文
//!   (salt + nonce 都随机),密文间无关联性。
//! - **AES-256-GCM**:认证加密;密文或 AAD 被篡改 → 解密失败
//!   (Err,绝不返回垃圾)。AAD 绑定条目名 + 存储版本,防条目
//!   换名重放。
//!
//! ## 存储
//!
//! `<dir>/secrets.json`:
//! ```json
//! { "version": 1, "entries": { "openai": { "salt": "…", "nonce": "…", "ct": "…", "created_at": "…" } } }
//! ```
//! 所有二进制字段 hex 编码;文件里**不含**任何明文或可逆材料。
//!
//! ## 诚实说明(与 openhuman keyring_consent 对照)
//!
//! 本 crate 用**文件型 machine key**,不是 OS 钥匙串(Keychain /
//! Credential Manager / Secret Service)。保护边界是"文件系统权限 +
//! AEAD 认证加密",能挡"备份/同步/误提交把明文带出去"和"同机低权
//! 用户读 key 文件";**root/同 UID 进程仍可读**。接 OS 钥匙串是
//! 后续可插拔 provider 的事,不在本层假装解决。
//!
//! ## 并发
//!
//! 每次 set/delete 重新读盘 → 修改 → 原子写(tmp + rename),
//! 单进程内由内部 `Mutex` 串行化。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// 存储格式版本(AAD 绑定,防跨版本混用密文)。
const FORMAT_VERSION: u64 = 1;
/// machine key 字节数(AES-256)。
const KEY_LEN: usize = 32;
/// GCM nonce 字节数(标准 96-bit)。
const NONCE_LEN: usize = 12;
/// 每条目 HKDF salt 字节数。
const SALT_LEN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("machine key file {0} has insecure permissions (must be 0600 on unix)")]
    InsecureKeyPermissions(PathBuf),
    #[error("machine key file corrupt (expected {KEY_LEN} bytes, got {got})")]
    BadKeyLen { got: usize },
    #[error("secrets store corrupt: {0}")]
    Corrupt(String),
    /// 认证失败:密文被篡改 / key 不对 / AAD 不匹配。**绝不**返回部分明文。
    #[error("authentication failed: ciphertext tampered or wrong machine key ({0})")]
    AuthFailed(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u64,
    entries: BTreeMap<String, EncryptedEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncryptedEntry {
    /// HKDF salt(hex)。
    salt: String,
    /// GCM nonce(hex)。
    nonce: String,
    /// 密文(hex,含 GCM tag)。
    ct: String,
    created_at: String,
}

/// 密钥存储。`dir` 下持有 `.machine.key` 与 `secrets.json`。
pub struct SecretStore {
    dir: PathBuf,
    machine_key: [u8; KEY_LEN],
    /// 单进程内 set/delete/list 串行化(读盘-改-原子写)。
    io_lock: Mutex<()>,
}

/// 手写 Debug:**绝不**输出 machine key 字节。
impl std::fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretStore")
            .field("dir", &self.dir)
            .field("machine_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl SecretStore {
    /// 打开(必要时生成 machine key)。unix 上校验已有 key 文件权限。
    pub fn open(dir: impl AsRef<Path>) -> Result<SecretStore, SecretError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let key_path = dir.join(".machine.key");
        let machine_key: [u8; KEY_LEN] = if key_path.exists() {
            check_unix_key_perms(&key_path)?;
            let bytes = std::fs::read(&key_path)?;
            bytes
                .try_into()
                .map_err(|v: Vec<u8>| SecretError::BadKeyLen { got: v.len() })?
        } else {
            let key = Aes256Gcm::generate_key(&mut OsRng);
            let bytes: [u8; KEY_LEN] = key.into();
            write_new_0600(&key_path, &bytes)?;
            bytes
        };
        Ok(SecretStore {
            dir,
            machine_key,
            io_lock: Mutex::new(()),
        })
    }

    /// 存一条密钥(覆盖同名)。
    pub fn set(&self, name: &str, plaintext: &str) -> Result<(), SecretError> {
        if name.is_empty() {
            return Err(SecretError::Corrupt("entry name must not be empty".into()));
        }
        let _g = self.io_lock.lock();
        let mut store = self.read_store()?;
        // 每条目独立 salt + nonce:同明文两次写入密文无关。
        let mut salt = [0u8; SALT_LEN];
        let salt_bytes = Aes256Gcm::generate_key(&mut OsRng); // 32B 随机源
        salt.copy_from_slice(&salt_bytes[..SALT_LEN]);
        let entry_key = derive_entry_key(&self.machine_key, &salt);
        let cipher = Aes256Gcm::new_from_slice(&entry_key)
            .map_err(|_| SecretError::Corrupt("key init".into()))?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let aad = aad_for(name);
        let ct = cipher
            .encrypt(
                &nonce,
                aes_gcm::aead::Payload {
                    msg: plaintext.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| SecretError::AuthFailed(format!("encrypt {name}")))?;
        store.entries.insert(
            name.to_string(),
            EncryptedEntry {
                salt: hex_encode(&salt),
                nonce: hex_encode(nonce.as_slice()),
                ct: hex_encode(&ct),
                created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            },
        );
        self.write_store(&store)
    }

    /// 取一条密钥;不存在 → `Ok(None)`;认证失败 → Err。
    pub fn get(&self, name: &str) -> Result<Option<String>, SecretError> {
        let _g = self.io_lock.lock();
        let store = self.read_store()?;
        let Some(entry) = store.entries.get(name) else {
            return Ok(None);
        };
        let salt = hex_decode(&entry.salt, "salt")?;
        let salt: [u8; SALT_LEN] = salt.try_into().map_err(|v: Vec<u8>| {
            SecretError::Corrupt(format!("salt len {} != {SALT_LEN}", v.len()))
        })?;
        let nonce_bytes = hex_decode(&entry.nonce, "nonce")?;
        let ct = hex_decode(&entry.ct, "ct")?;
        if nonce_bytes.len() != NONCE_LEN {
            return Err(SecretError::Corrupt(format!(
                "nonce len {} != {NONCE_LEN}",
                nonce_bytes.len()
            )));
        }
        let entry_key = derive_entry_key(&self.machine_key, &salt);
        let cipher = Aes256Gcm::new_from_slice(&entry_key)
            .map_err(|_| SecretError::Corrupt("key init".into()))?;
        let aad = aad_for(name);
        let pt = cipher
            .decrypt(
                Nonce::from_slice(&nonce_bytes),
                aes_gcm::aead::Payload {
                    msg: &ct,
                    aad: &aad,
                },
            )
            .map_err(|_| SecretError::AuthFailed(name.to_string()))?;
        String::from_utf8(pt)
            .map(Some)
            .map_err(|_| SecretError::Corrupt("plaintext not utf-8".into()))
    }

    /// 删除一条;返回是否存在。
    pub fn delete(&self, name: &str) -> Result<bool, SecretError> {
        let _g = self.io_lock.lock();
        let mut store = self.read_store()?;
        Ok(store
            .entries
            .remove(name)
            .is_some_and(|_| self.write_store(&store).is_ok()))
    }

    /// 列出全部条目名(排序)。
    pub fn list(&self) -> Result<Vec<String>, SecretError> {
        let _g = self.io_lock.lock();
        let store = self.read_store()?;
        Ok(store.entries.keys().cloned().collect())
    }

    /// 密钥文件路径(测试 / 展示用)。
    pub fn machine_key_path(&self) -> PathBuf {
        self.dir.join(".machine.key")
    }

    fn store_path(&self) -> PathBuf {
        self.dir.join("secrets.json")
    }

    fn read_store(&self) -> Result<StoreFile, SecretError> {
        let path = self.store_path();
        if !path.exists() {
            return Ok(StoreFile {
                version: FORMAT_VERSION,
                entries: BTreeMap::new(),
            });
        }
        let raw = std::fs::read_to_string(&path)?;
        let store: StoreFile = serde_json::from_str(&raw)?;
        if store.version != FORMAT_VERSION {
            return Err(SecretError::Corrupt(format!(
                "unsupported store version {} (expected {FORMAT_VERSION})",
                store.version
            )));
        }
        Ok(store)
    }

    fn write_store(&self, store: &StoreFile) -> Result<(), SecretError> {
        let raw = serde_json::to_string_pretty(store)?;
        let path = self.store_path();
        // 原子写:tmp + rename(避免半截 JSON)。
        let tmp = path.with_extension("json.tmp");
        {
            use std::io::Write;
            #[cfg(unix)]
            let mut f = {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&tmp)?
            };
            #[cfg(not(unix))]
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            f.write_all(raw.as_bytes())?;
            f.flush()?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// AAD:版本 + 条目名 —— 换名重放(把 A 的密文登记成 B)会被认证拒绝。
fn aad_for(name: &str) -> Vec<u8> {
    format!("zhuye-secrets:v{FORMAT_VERSION}:{name}").into_bytes()
}

/// HKDF-SHA256(machine_key, salt) → 32B 条目密钥。
fn derive_entry_key(machine_key: &[u8; KEY_LEN], salt: &[u8; SALT_LEN]) -> [u8; KEY_LEN] {
    let hk = Hkdf::<Sha256>::new(Some(salt), machine_key);
    let mut okm = [0u8; KEY_LEN];
    hk.expand(b"zhuye-secrets entry", &mut okm)
        .expect("HKDF expand with valid length");
    okm
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str, what: &str) -> Result<Vec<u8>, SecretError> {
    if s.len() & 1 == 1 {
        return Err(SecretError::Corrupt(format!("{what} hex odd length")));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| SecretError::Corrupt(format!("{what} hex invalid")))
        })
        .collect()
}

#[cfg(unix)]
fn check_unix_key_perms(path: &Path) -> Result<(), SecretError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    // group/other 任何读写位 → 拒绝(fail-closed)。
    if mode & 0o077 != 0 {
        return Err(SecretError::InsecureKeyPermissions(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_unix_key_perms(_path: &Path) -> Result<(), SecretError> {
    Ok(())
}

/// 新文件 0600 写入(create_new 拒绝覆盖,防 symlink 预置攻击)。
fn write_new_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    let mut f = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (tempfile::TempDir, SecretStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn set_get_roundtrip() {
        let (_d, store) = tmp_store();
        store.set("openai", "sk-abcdefghijklmnopqrstuv").unwrap();
        assert_eq!(
            store.get("openai").unwrap(),
            Some("sk-abcdefghijklmnopqrstuv".to_string())
        );
    }

    #[test]
    fn get_missing_returns_none() {
        let (_d, store) = tmp_store();
        assert_eq!(store.get("nope").unwrap(), None);
    }

    #[test]
    fn set_overwrites_and_delete_works() {
        let (_d, store) = tmp_store();
        store.set("k", "v1").unwrap();
        store.set("k", "v2").unwrap();
        assert_eq!(store.get("k").unwrap(), Some("v2".into()));
        assert!(store.delete("k").unwrap());
        assert_eq!(store.get("k").unwrap(), None);
        assert!(!store.delete("k").unwrap());
    }

    #[test]
    fn list_is_sorted_names() {
        let (_d, store) = tmp_store();
        store.set("b", "1").unwrap();
        store.set("a", "2").unwrap();
        store.set("c", "3").unwrap();
        assert_eq!(store.list().unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn ciphertext_at_rest_has_no_plaintext() {
        let (dir, store) = tmp_store();
        let secret = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";
        store.set("anthropic", secret).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("secrets.json")).unwrap();
        assert!(!raw.contains(secret), "明文泄漏到落盘文件: {raw}");
        // hex 密文 + 元数据在。
        assert!(raw.contains("\"ct\""));
        assert!(raw.contains("\"salt\""));
        assert!(raw.contains("\"nonce\""));
    }

    #[test]
    fn same_plaintext_twice_yields_unrelated_ciphertext() {
        let (dir, store) = tmp_store();
        store.set("k1", "same-secret").unwrap();
        store.set("k2", "same-secret").unwrap();
        let raw = std::fs::read_to_string(dir.path().join("secrets.json")).unwrap();
        // 两条目的 salt / nonce / ct 互不相同(随机化生效)。
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let e1 = &v["entries"]["k1"];
        let e2 = &v["entries"]["k2"];
        assert_ne!(e1["salt"], e2["salt"]);
        assert_ne!(e1["nonce"], e2["nonce"]);
        assert_ne!(e1["ct"], e2["ct"]);
    }

    #[test]
    fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = SecretStore::open(dir.path()).unwrap();
            store.set("api", "persist-me").unwrap();
        }
        let store = SecretStore::open(dir.path()).unwrap();
        assert_eq!(store.get("api").unwrap(), Some("persist-me".into()));
    }

    #[test]
    fn wrong_machine_key_fails_auth_not_garbage() {
        // 把密文文件复制到另一个 store(不同 machine key)→ 认证失败。
        let (dir_a, store_a) = tmp_store();
        store_a.set("x", "cross-store-secret").unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let store_b = SecretStore::open(dir_b.path()).unwrap();
        // machine key 必然不同(独立随机生成)。
        std::fs::copy(
            dir_a.path().join("secrets.json"),
            dir_b.path().join("secrets.json"),
        )
        .unwrap();
        let err = store_b.get("x").unwrap_err();
        assert!(
            matches!(err, SecretError::AuthFailed(_)),
            "跨 store 读取必须认证失败,实际:{err}"
        );
    }

    #[test]
    fn tampered_ciphertext_fails_auth() {
        let (dir, store) = tmp_store();
        store.set("k", "tamper-me").unwrap();
        let path = dir.path().join("secrets.json");
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // 翻转密文第一个 hex 字符。
        let ct = v["entries"]["k"]["ct"].as_str().unwrap().to_string();
        let flipped: String = ct
            .chars()
            .enumerate()
            .map(|(i, c)| {
                if i == 0 {
                    if c == '0' { '1' } else { '0' }
                } else {
                    c
                }
            })
            .collect();
        assert_ne!(ct, flipped);
        v["entries"]["k"]["ct"] = serde_json::Value::String(flipped);
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
        assert!(matches!(store.get("k"), Err(SecretError::AuthFailed(_))));
    }

    #[test]
    fn renamed_entry_replay_fails_auth() {
        // AAD 绑定条目名:把 k1 的密文在文件里改名为 k2 → 认证拒绝。
        let (dir, store) = tmp_store();
        store.set("k1", "rename-attack").unwrap();
        let path = dir.path().join("secrets.json");
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let entry = v["entries"]["k1"].clone();
        v["entries"]["k2"] = entry;
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
        assert!(matches!(store.get("k2"), Err(SecretError::AuthFailed(_))));
    }

    #[test]
    fn empty_name_rejected() {
        let (_d, store) = tmp_store();
        assert!(store.set("", "v").is_err());
    }

    #[test]
    fn machine_key_file_is_0600_and_reused() {
        let (dir, store) = tmp_store();
        let key_path = store.machine_key_path();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "machine key 必须 0600");
        }
        let k1 = std::fs::read(&key_path).unwrap();
        drop(store);
        let store2 = SecretStore::open(dir.path()).unwrap();
        let k2 = std::fs::read(store2.machine_key_path()).unwrap();
        assert_eq!(k1, k2, "machine key 必须复用,不能每次重生成");
    }

    #[cfg(unix)]
    #[test]
    fn insecure_key_permissions_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join(".machine.key");
        std::fs::write(&key_path, [7u8; 32]).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = SecretStore::open(dir.path()).unwrap_err();
        assert!(
            matches!(err, SecretError::InsecureKeyPermissions(_)),
            "过宽权限必须拒开,实际:{err}"
        );
    }

    #[test]
    fn corrupted_store_fails_cleanly() {
        let (dir, _store) = tmp_store();
        std::fs::write(dir.path().join("secrets.json"), "{{{not json").unwrap();
        let store = SecretStore::open(dir.path()).unwrap();
        assert!(matches!(store.get("k"), Err(SecretError::Json(_))));
    }

    #[test]
    fn wrong_version_fails_cleanly() {
        let (dir, _store) = tmp_store();
        std::fs::write(
            dir.path().join("secrets.json"),
            r#"{"version": 99, "entries": {}}"#,
        )
        .unwrap();
        let store = SecretStore::open(dir.path()).unwrap();
        assert!(matches!(store.get("k"), Err(SecretError::Corrupt(_))));
    }
}
