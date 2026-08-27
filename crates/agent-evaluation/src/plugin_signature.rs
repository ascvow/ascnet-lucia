//! 插件构建、审批与发布使用的受信 Ed25519 签名控制面。
//!
//! 协议层只校验签名信封结构；本模块持有受信 Keyring、用途约束和吊销状态，并执行真实
//! Ed25519 签名与严格验签。私钥只存在于显式 Signer 中，不进入协议对象或持久化记录。

use agent_evolution_protocol::{
    ArtifactDigest, InvalidPluginEvolution, MutationId, SignatureAlgorithm, SignatureEnvelope,
    SignaturePurpose, SIGNATURE_ENVELOPE_SCHEMA_VERSION,
};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use std::collections::BTreeMap;

/// 一个只持有内存私钥的受信插件签名器。
///
/// `Debug` 输出不会包含私钥；调用方负责从受保护密钥存储构造并限制生命周期。
pub struct TrustedPluginSigner {
    key_id: String,
    purpose: SignaturePurpose,
    signing_key: SigningKey,
}

impl std::fmt::Debug for TrustedPluginSigner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TrustedPluginSigner")
            .field("key_id", &self.key_id)
            .field("purpose", &self.purpose)
            .field("signing_key", &"[REDACTED]")
            .finish()
    }
}

impl TrustedPluginSigner {
    /// 从 32 字节 Ed25519 私钥种子创建限定用途的签名器。
    ///
    /// # Errors
    ///
    /// `key_id` 不是协议允许的稳定标识时返回错误；私钥字节不会写入错误消息。
    pub fn from_secret_bytes(
        key_id: impl Into<String>,
        purpose: SignaturePurpose,
        secret: &[u8; 32],
    ) -> Result<Self, PluginSignatureError> {
        let key_id = key_id.into();
        validate_key_id(&key_id)?;
        Ok(Self {
            key_id,
            purpose,
            signing_key: SigningKey::from_bytes(secret),
        })
    }

    /// 返回可登记到受信 Keyring 的公钥条目。
    pub fn verifying_key(&self) -> TrustedPluginVerifyingKey {
        TrustedPluginVerifyingKey {
            key_id: self.key_id.clone(),
            purpose: self.purpose,
            verifying_key: self.signing_key.verifying_key(),
            revoked_at_ms: None,
        }
    }

    /// 对精确用途、插件、Mutation、主题摘要和有效期生成签名信封。
    ///
    /// # Errors
    ///
    /// 身份、时间或签名信封结构不合法时返回错误。签名只覆盖协议定义的域分离消息。
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        &self,
        plugin_id: impl Into<String>,
        mutation_id: MutationId,
        subject_digest: ArtifactDigest,
        signed_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<SignatureEnvelope, PluginSignatureError> {
        let mut envelope = SignatureEnvelope {
            schema_version: SIGNATURE_ENVELOPE_SCHEMA_VERSION,
            purpose: self.purpose,
            algorithm: SignatureAlgorithm::Ed25519,
            key_id: self.key_id.clone(),
            plugin_id: plugin_id.into(),
            mutation_id,
            subject_digest,
            signature_hex: "0".repeat(128),
            signed_at_ms,
            expires_at_ms,
        };
        let message = envelope.signing_bytes()?;
        envelope.signature_hex = encode_hex(&self.signing_key.sign(&message).to_bytes());
        envelope.validate()?;
        Ok(envelope)
    }
}

/// Keyring 中一把限定用途且可吊销的 Ed25519 公钥。
#[derive(Debug, Clone)]
pub struct TrustedPluginVerifyingKey {
    key_id: String,
    purpose: SignaturePurpose,
    verifying_key: VerifyingKey,
    revoked_at_ms: Option<u64>,
}

impl TrustedPluginVerifyingKey {
    /// 从受信配置中的公钥字节创建 Keyring 条目。
    ///
    /// # Errors
    ///
    /// Key ID 或 Ed25519 公钥编码无效时返回错误。
    pub fn from_public_bytes(
        key_id: impl Into<String>,
        purpose: SignaturePurpose,
        public_key: &[u8; 32],
    ) -> Result<Self, PluginSignatureError> {
        let key_id = key_id.into();
        validate_key_id(&key_id)?;
        let verifying_key = VerifyingKey::from_bytes(public_key)
            .map_err(|_| PluginSignatureError::InvalidPublicKey)?;
        Ok(Self {
            key_id,
            purpose,
            verifying_key,
            revoked_at_ms: None,
        })
    }

    /// 设置密钥吊销时间；该时间及之后签发或验证的信封均被拒绝。
    ///
    /// # Errors
    ///
    /// 吊销时间为零时返回错误。
    pub fn revoke_at(mut self, revoked_at_ms: u64) -> Result<Self, PluginSignatureError> {
        if revoked_at_ms == 0 {
            return Err(PluginSignatureError::InvalidRevocationTime);
        }
        self.revoked_at_ms = Some(revoked_at_ms);
        Ok(self)
    }
}

/// 由受信配置装配、按 Key ID 精确索引的插件签名 Keyring。
#[derive(Debug, Default)]
pub struct TrustedPluginKeyring {
    keys: BTreeMap<String, TrustedPluginVerifyingKey>,
}

impl TrustedPluginKeyring {
    /// 创建空 Keyring；空 Keyring 会拒绝所有签名。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一把公钥，同一 Key ID 不允许覆盖。
    ///
    /// # Errors
    ///
    /// Key ID 已存在时返回错误，避免配置顺序决定信任结果。
    pub fn insert(&mut self, key: TrustedPluginVerifyingKey) -> Result<(), PluginSignatureError> {
        if self.keys.contains_key(&key.key_id) {
            return Err(PluginSignatureError::DuplicateKey(key.key_id));
        }
        self.keys.insert(key.key_id.clone(), key);
        Ok(())
    }

    /// 验证签名信封的结构、用途、有效期、吊销状态和 Ed25519 签名字节。
    ///
    /// # Errors
    ///
    /// Key 不受信、用途错绑、已过期、已吊销、编码或密码学验证失败时返回错误。
    pub fn verify(
        &self,
        envelope: &SignatureEnvelope,
        expected_purpose: SignaturePurpose,
        at_ms: u64,
    ) -> Result<(), PluginSignatureError> {
        envelope.validate()?;
        if envelope.purpose != expected_purpose {
            return Err(PluginSignatureError::PurposeMismatch);
        }
        if !envelope.is_valid_at(at_ms) {
            return Err(PluginSignatureError::Expired);
        }
        let key = self
            .keys
            .get(&envelope.key_id)
            .ok_or_else(|| PluginSignatureError::UnknownKey(envelope.key_id.clone()))?;
        if key.purpose != expected_purpose {
            return Err(PluginSignatureError::PurposeMismatch);
        }
        if key
            .revoked_at_ms
            .is_some_and(|revoked_at| envelope.signed_at_ms >= revoked_at || at_ms >= revoked_at)
        {
            return Err(PluginSignatureError::Revoked);
        }
        let signature_bytes = decode_signature_hex(&envelope.signature_hex)?;
        let signature = Signature::from_bytes(&signature_bytes);
        let message = envelope.signing_bytes()?;
        key.verifying_key
            .verify_strict(&message, &signature)
            .map_err(|_| PluginSignatureError::VerificationFailed)
    }
}

/// 插件密码学签名控制面错误。
#[derive(Debug, thiserror::Error)]
pub enum PluginSignatureError {
    /// 协议信封或身份字段不合法。
    #[error("插件签名协议无效：{0}")]
    Protocol(#[from] InvalidPluginEvolution),
    /// Key ID 不符合协议稳定标识约束。
    #[error("插件签名 Key ID 必须是有界 ASCII 稳定标识")]
    InvalidKeyId,
    /// Ed25519 公钥编码无效。
    #[error("插件签名公钥编码无效")]
    InvalidPublicKey,
    /// Keyring 中已存在相同 Key ID。
    #[error("插件签名 Key ID 重复：{0}")]
    DuplicateKey(String),
    /// 签名引用了 Keyring 中不存在的 Key ID。
    #[error("插件签名 Key ID 不受信：{0}")]
    UnknownKey(String),
    /// 签名用途与调用位置或 Key 用途不一致。
    #[error("插件签名用途不匹配")]
    PurposeMismatch,
    /// 签名在验证时间已过期或尚未生效。
    #[error("插件签名不在有效期内")]
    Expired,
    /// 公钥已被吊销。
    #[error("插件签名密钥已吊销")]
    Revoked,
    /// 吊销时间无效。
    #[error("插件签名密钥吊销时间必须非零")]
    InvalidRevocationTime,
    /// 签名十六进制无法解码为 64 字节。
    #[error("插件 Ed25519 签名编码无效")]
    InvalidSignatureEncoding,
    /// Ed25519 严格验签失败。
    #[error("插件 Ed25519 签名验证失败")]
    VerificationFailed,
}

fn validate_key_id(key_id: &str) -> Result<(), PluginSignatureError> {
    if key_id.is_empty()
        || key_id.len() > 128
        || !key_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(PluginSignatureError::InvalidKeyId);
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_signature_hex(value: &str) -> Result<[u8; 64], PluginSignatureError> {
    if value.len() != 128 {
        return Err(PluginSignatureError::InvalidSignatureEncoding);
    }
    let mut bytes = [0_u8; 64];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = decode_nibble(chunk[0])?;
        let low = decode_nibble(chunk[1])?;
        bytes[index] = (high << 4) | low;
    }
    Ok(bytes)
}

fn decode_nibble(value: u8) -> Result<u8, PluginSignatureError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(PluginSignatureError::InvalidSignatureEncoding),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(seed: char) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
    }

    /// 真实 Ed25519 签名必须通过精确用途 Keyring 验证。
    #[test]
    fn signs_and_verifies_real_ed25519_envelope() {
        let signer = TrustedPluginSigner::from_secret_bytes(
            "builder-key-v1",
            SignaturePurpose::BuildAttestation,
            &[7_u8; 32],
        )
        .expect("应创建签名器");
        let mut keyring = TrustedPluginKeyring::new();
        keyring.insert(signer.verifying_key()).expect("应登记公钥");
        let envelope = signer
            .sign(
                "example.plugin",
                MutationId::generate(),
                digest('a'),
                10,
                100,
            )
            .expect("应生成真实签名");
        keyring
            .verify(&envelope, SignaturePurpose::BuildAttestation, 50)
            .expect("真实签名应通过验证");
    }

    /// 用途、消息、有效期与吊销状态均不能绕过密码学门禁。
    #[test]
    fn rejects_replay_tampering_expiry_and_revocation() {
        let signer = TrustedPluginSigner::from_secret_bytes(
            "release-key-v1",
            SignaturePurpose::PluginRelease,
            &[9_u8; 32],
        )
        .expect("应创建签名器");
        let envelope = signer
            .sign(
                "example.plugin",
                MutationId::generate(),
                digest('b'),
                10,
                100,
            )
            .expect("应生成签名");
        let mut keyring = TrustedPluginKeyring::new();
        keyring.insert(signer.verifying_key()).expect("应登记公钥");
        assert!(matches!(
            keyring.verify(&envelope, SignaturePurpose::CapabilityApproval, 50),
            Err(PluginSignatureError::PurposeMismatch)
        ));
        assert!(matches!(
            keyring.verify(&envelope, SignaturePurpose::PluginRelease, 100),
            Err(PluginSignatureError::Expired)
        ));

        let mut tampered = envelope.clone();
        tampered.subject_digest = digest('c');
        assert!(matches!(
            keyring.verify(&tampered, SignaturePurpose::PluginRelease, 50),
            Err(PluginSignatureError::VerificationFailed)
        ));

        let mut revoked = TrustedPluginKeyring::new();
        revoked
            .insert(
                signer
                    .verifying_key()
                    .revoke_at(40)
                    .expect("应设置吊销时间"),
            )
            .expect("应登记已吊销公钥");
        assert!(matches!(
            revoked.verify(&envelope, SignaturePurpose::PluginRelease, 50),
            Err(PluginSignatureError::Revoked)
        ));
    }
}
