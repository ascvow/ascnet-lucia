//! M8 真实 Component 构建 Inspector 测试。

use agent_evaluation::ManifestComponentInspector;
use agent_evolution::{ComponentInspectionRequest, ComponentInspector};
use agent_evolution_protocol::ArtifactDigest;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};
use tempfile::tempdir;

/// 计算测试字节的强类型 SHA-256 摘要。
fn digest(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes))).expect("测试摘要应合法")
}

/// 写入一个通过 Host 正式 schema 校验的最小 manifest。
fn write_manifest(root: &Path, plugin_id: &str) -> std::path::PathBuf {
    let path = root.join("plugin.toml");
    fs::write(
        &path,
        format!(
            "[plugin]\nid = \"{plugin_id}\"\nname = \"Inspector\"\nversion = \"0.1.0\"\napi_version = \"0.7.0\"\nwasm = \"example.wasm\"\n"
        ),
    )
    .expect("写入测试 manifest");
    path
}

/// Component 摘要不匹配时必须在类型扫描前失败关闭。
#[test]
fn rejects_component_digest_mismatch_before_scan() {
    let root = tempdir().expect("创建临时目录");
    let manifest = write_manifest(root.path(), "example");
    let component = root.path().join("example.wasm");
    fs::write(&component, b"not-a-component").expect("写入测试 Component");
    let mut inspector = ManifestComponentInspector::new(manifest, digest(b"scanner-v1"));
    let error = inspector
        .inspect(&ComponentInspectionRequest {
            component_path: component,
            component_digest: digest(b"different"),
            component_size_bytes: b"not-a-component".len() as u64,
            plugin_id: "example".to_string(),
        })
        .expect_err("错绑摘要必须拒绝");
    assert!(error.to_string().contains("字节身份"));
}

/// manifest 插件身份不匹配时不得继续编译不受信 Component。
#[test]
fn rejects_manifest_identity_before_scan() {
    let root = tempdir().expect("创建临时目录");
    let manifest = write_manifest(root.path(), "other");
    let bytes = b"not-a-component";
    let component = root.path().join("example.wasm");
    fs::write(&component, bytes).expect("写入测试 Component");
    let mut inspector = ManifestComponentInspector::new(manifest, digest(b"scanner-v1"));
    let error = inspector
        .inspect(&ComponentInspectionRequest {
            component_path: component,
            component_digest: digest(bytes),
            component_size_bytes: bytes.len() as u64,
            plugin_id: "example".to_string(),
        })
        .expect_err("错绑插件身份必须拒绝");
    assert!(error.to_string().contains("manifest 身份"));
}

/// 真实 Echo Component 必须生成与实际插件身份绑定的接口和能力扫描结果。
#[test]
#[ignore = "需要预构建 examples/plugins/echo-plugin 的真实 WASM Component"]
fn scans_real_echo_component() {
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bundle_root = repository_root.join("examples/plugins/echo-plugin");
    let manifest = bundle_root.join("plugin.toml");
    let component = bundle_root.join("target/wasm32-wasip2/release/echo_plugin.wasm");
    let bytes = fs::read(&component).expect("读取真实 Echo Component");
    let mut inspector = ManifestComponentInspector::new(manifest, digest(b"scanner-v1"));
    let result = inspector
        .inspect(&ComponentInspectionRequest {
            component_path: component,
            component_digest: digest(&bytes),
            component_size_bytes: bytes.len() as u64,
            plugin_id: "echo".to_string(),
        })
        .expect("真实 Echo Component 应通过扫描");

    assert_eq!(result.interface.plugin_id, "echo");
    assert_eq!(result.interface.component_digest, digest(&bytes));
    assert!(!result.interface.exports.is_empty());
    assert!(result.capabilities.requested.capabilities.is_empty());
    assert!(result.capabilities.provided.capabilities.is_empty());
}
