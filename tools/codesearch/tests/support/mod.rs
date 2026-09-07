//! 集成测试共用支撑代码：fixture 仓库与本地 HTTP Embedding 服务。
//!
//! 这里的 Embedding 服务是一个**真实的 HTTP 服务**，走完整的
//! OpenAI-compatible `/embeddings` 协议，因此测试覆盖的是真实的 Provider 适配层，
//! 而不是被替换掉的接口。向量本身由确定性词袋哈希投影生成，
//! 只保证"相同输入得到相同向量"与"共享词条越多越接近"，
//! 不能用来证明真实模型的语义检索质量。

#![allow(dead_code)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 测试统一使用的向量维度。取值较小以加快测试。
pub const DIMENSIONS: usize = 128;

// ---------------------------------------------------------------------------
// fixture 仓库
// ---------------------------------------------------------------------------

/// 把 `tests/fixtures/sample_repo` 复制到临时目录，并补齐无法提交进 git 的文件。
///
/// 返回临时目录句柄；句柄释放时目录被删除。
pub fn sample_repo() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("创建临时目录");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample_repo");
    copy_dir(&source, temp.path()).expect("复制 fixture 仓库");

    // 以下文件被本仓库或 fixture 自身的 .gitignore 排除，无法提交，因此运行时生成。
    // `.env` 用于验证敏感文件规则，内容是明显的假值。
    write_file(
        &temp.path().join(".env"),
        "APP_ENDPOINT=http://127.0.0.1:8080\nAPP_TOKEN=not-a-real-token-fixture-only\n",
    );
    // `generated/` 命中 fixture 自己的 .gitignore，用于验证 ignore 规则生效。
    write_file(
        &temp.path().join("generated").join("cache.rs"),
        "// 自动生成的文件，应被 .gitignore 排除。\npub fn generated_retry_stub() {}\n",
    );
    // 二进制文件必须被跳过且给出可观察的原因。
    let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(&[0u8; 64]);
    std::fs::create_dir_all(temp.path().join("assets")).expect("创建 assets");
    std::fs::write(temp.path().join("assets").join("logo.png"), &png).expect("写入 png");
    // CRLF、末尾无换行与多字节字符的组合。
    write_file(
        &temp.path().join("src")
            .join("crlf_notes.rs"),
        "/// 处理中文注释与 CRLF 换行。\r\npub fn 计算退避(次数: u32) -> u32 {\r\n    次数 * 2\r\n}",
    );
    temp
}

/// 写文件并自动创建父目录。
pub fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("创建父目录");
    }
    std::fs::write(path, contents).expect("写入文件");
}

/// 读取 workspace 内的文件。
pub fn read_file(path: &Path) -> String {
    std::fs::read_to_string(path).expect("读取文件")
}

fn copy_dir(source: &Path, target: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 生成指向本地 Embedding 服务的配置文件内容。
///
/// `api_key_env` 留空表示服务不需要鉴权；配置里只写环境变量名，从不写入真实凭证。
pub fn config_toml(base_url: &str, api_key_env: &str) -> String {
    format!(
        r#"[embedding]
provider = "openai_compatible"
base_url = "{base_url}"
model = "fixture-embed"
api_key_env = "{api_key_env}"
dimensions = {DIMENSIONS}
batch_size = 8
max_concurrency = 2
max_retries = 2
timeout_secs = 10

[index]
# 测试里显式触发索引，避免每次查询都重扫目录导致断言不稳定。
refresh_on_search = false
"#
    )
}

// ---------------------------------------------------------------------------
// 本地 HTTP Embedding 服务
// ---------------------------------------------------------------------------

/// 服务对下一批请求的响应方式，用于验证错误处理与重试。
#[derive(Debug, Clone)]
pub enum Reply {
    /// 正常返回确定性向量。
    Ok,
    /// 返回指定状态码与错误体。
    Status(u16),
    /// 返回的向量数量比请求少一个。
    MissingVector,
    /// 返回维度错误的向量。
    WrongDimensions(usize),
    /// 返回会溢出成 Infinity 的向量。
    ///
    /// 合法 JSON 无法表示 NaN，但 `1e40` 这类超出 f32 范围的取值会解析成 `inf`，
    /// 这是真实服务可能出现的情况。
    NotFinite,
    /// 返回空向量。
    EmptyVector,
    /// 打乱 `data` 顺序，但保留正确的 `index` 字段。
    Shuffled,
    /// 省略 `index` 字段，按数组顺序返回。
    WithoutIndex,
    /// 返回无法解析的响应体。
    Malformed,
    /// 返回重复的 `index`。
    DuplicateIndex,
}

/// 服务端记录的一次请求。
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// 请求体中的 `model`。
    pub model: String,
    /// 请求体中的 `input` 列表。
    pub inputs: Vec<String>,
    /// `Authorization` 头，若存在。
    pub authorization: Option<String>,
    /// 请求体中是否带 `dimensions`。
    pub dimensions: Option<usize>,
    /// `encoding_format` 字段。
    pub encoding_format: Option<String>,
}

#[derive(Debug, Default)]
struct ServerState {
    /// 按顺序消费的响应脚本；用尽后一律正常返回。
    script: Vec<Reply>,
    requests: Vec<RecordedRequest>,
}

/// 本地 Embedding 服务句柄。
pub struct EmbeddingServer {
    base_url: String,
    state: Arc<Mutex<ServerState>>,
    hits: Arc<AtomicUsize>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl EmbeddingServer {
    /// 在随机端口启动服务。
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定端口");
        let address = listener.local_addr().expect("本地地址");
        let base_url = format!("http://{address}/v1");
        let state = Arc::new(Mutex::new(ServerState::default()));
        let hits = Arc::new(AtomicUsize::new(0));
        let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();

        let task_state = Arc::clone(&state);
        let task_hits = Arc::clone(&hits);
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let state = Arc::clone(&task_state);
                        let hits = Arc::clone(&task_hits);
                        tokio::spawn(async move {
                            if let Err(error) = serve_connection(stream, state, hits).await {
                                eprintln!("测试 Embedding 服务连接错误: {error}");
                            }
                        });
                    }
                }
            }
        });

        Self {
            base_url,
            state,
            hits,
            shutdown: Some(shutdown),
            handle: Some(handle),
        }
    }

    /// 形如 `http://127.0.0.1:PORT/v1` 的基础地址。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 收到的请求总数（按 HTTP 请求计，不是输入条数）。
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// 设置后续响应脚本，按顺序消费。
    pub fn script(&self, replies: Vec<Reply>) {
        self.state.lock().expect("锁").script = replies;
    }

    /// 取出记录的请求。
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().expect("锁").requests.clone()
    }

    /// 清空请求记录与计数。
    pub fn reset(&self) {
        let mut guard = self.state.lock().expect("锁");
        guard.requests.clear();
        guard.script.clear();
        self.hits.store(0, Ordering::SeqCst);
    }
}

impl Drop for EmbeddingServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

async fn serve_connection(
    mut stream: tokio::net::TcpStream,
    state: Arc<Mutex<ServerState>>,
    hits: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    // 读取请求头，再按 Content-Length 读取请求体。
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_subslice(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 1 << 20 {
            return Ok(());
        }
    };
    let header_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length = header_value(&header_text, "content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < header_end + content_length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body =
        String::from_utf8_lossy(&buffer[header_end..header_end + content_length]).to_string();

    hits.fetch_add(1, Ordering::SeqCst);
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let inputs: Vec<String> = parsed
        .get("input")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();

    let reply = {
        let mut guard = state.lock().expect("锁");
        guard.requests.push(RecordedRequest {
            model: parsed
                .get("model")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            inputs: inputs.clone(),
            authorization: header_value(&header_text, "authorization"),
            dimensions: parsed
                .get("dimensions")
                .and_then(|value| value.as_u64())
                .map(|value| value as usize),
            encoding_format: parsed
                .get("encoding_format")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string()),
        });
        if guard.script.is_empty() {
            Reply::Ok
        } else {
            guard.script.remove(0)
        }
    };

    let response = build_response(&reply, &inputs);
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

fn build_response(reply: &Reply, inputs: &[String]) -> String {
    match reply {
        Reply::Status(code) => http_response(
            *code,
            &serde_json::json!({ "error": { "message": "injected failure" } }).to_string(),
        ),
        Reply::Malformed => http_response(200, "this is not json"),
        _ => {
            let mut items: Vec<serde_json::Value> = Vec::new();
            for (index, input) in inputs.iter().enumerate() {
                let embedding: Vec<serde_json::Value> = match reply {
                    Reply::WrongDimensions(dimensions) => number_values(&vec![0.5f32; *dimensions]),
                    Reply::NotFinite => {
                        let mut values = number_values(&embed_text(input));
                        // 超出 f32 范围的合法 JSON 数值，反序列化后变成 inf。
                        values[0] = serde_json::from_str("1e40").expect("合法 JSON 数值");
                        values
                    }
                    Reply::EmptyVector => Vec::new(),
                    _ => number_values(&embed_text(input)),
                };
                let mut item = serde_json::json!({ "embedding": embedding });
                if !matches!(reply, Reply::WithoutIndex) {
                    let recorded = if matches!(reply, Reply::DuplicateIndex) {
                        0
                    } else {
                        index
                    };
                    item["index"] = serde_json::json!(recorded);
                }
                items.push(item);
            }
            if matches!(reply, Reply::MissingVector) {
                items.pop();
            }
            if matches!(reply, Reply::Shuffled) {
                items.reverse();
            }
            let body = serde_json::json!({
                "object": "list",
                "model": "fixture-embed",
                "data": items,
                "usage": { "prompt_tokens": inputs.len(), "total_tokens": inputs.len() }
            });
            http_response(200, &body.to_string())
        }
    }
}

fn number_values(vector: &[f32]) -> Vec<serde_json::Value> {
    vector
        .iter()
        .map(|value| serde_json::json!(value))
        .collect()
}

fn http_response(status: u16, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let mut out = String::new();
    let _ = write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    out
}

fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case(name) {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 确定性词袋哈希投影。
///
/// 与生产代码无关，只用于让本地服务返回稳定且"共享词条更接近"的向量。
pub fn embed_text(input: &str) -> Vec<f32> {
    let mut vector = vec![0.0f32; DIMENSIONS];
    let mut any = false;
    for term in tokenize(input) {
        any = true;
        let hash = blake3::hash(term.as_bytes());
        let bytes = hash.as_bytes();
        for slot in 0..4 {
            let offset = slot * 5;
            let index = u32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]) as usize
                % DIMENSIONS;
            let sign = if bytes[offset + 4].is_multiple_of(2) {
                1.0
            } else {
                -1.0
            };
            vector[index] += sign;
        }
    }
    if !any {
        vector[0] = 1.0;
    }
    vector
}

/// 极简分词：按非字母数字切分，并把连续汉字切成二元片段。
fn tokenize(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut ascii = String::new();
    let mut cjk: Vec<char> = Vec::new();

    let flush_ascii = |ascii: &mut String, out: &mut Vec<String>| {
        if !ascii.is_empty() {
            // camelCase 与 snake_case 都拆成小写片段，保持与索引侧一致的粒度。
            for part in split_words(ascii) {
                out.push(part);
            }
            ascii.clear();
        }
    };
    let flush_cjk = |cjk: &mut Vec<char>, out: &mut Vec<String>| {
        if cjk.is_empty() {
            return;
        }
        if cjk.len() == 1 {
            out.push(cjk[0].to_string());
        } else {
            for window in cjk.windows(2) {
                out.push(window.iter().collect());
            }
        }
        cjk.clear();
    };

    for character in input.chars() {
        if is_cjk(character) {
            flush_ascii(&mut ascii, &mut out);
            cjk.push(character);
        } else if character.is_alphanumeric() || character == '_' {
            flush_cjk(&mut cjk, &mut out);
            ascii.push(character);
        } else {
            flush_ascii(&mut ascii, &mut out);
            flush_cjk(&mut cjk, &mut out);
        }
    }
    flush_ascii(&mut ascii, &mut out);
    flush_cjk(&mut cjk, &mut out);
    out
}

fn split_words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for character in text.chars() {
        if character == '_' {
            if !current.is_empty() {
                out.push(current.to_lowercase());
                current.clear();
            }
            previous_lower = false;
            continue;
        }
        if character.is_uppercase() && previous_lower && !current.is_empty() {
            out.push(current.to_lowercase());
            current.clear();
        }
        previous_lower = character.is_lowercase() || character.is_numeric();
        current.push(character);
    }
    if !current.is_empty() {
        out.push(current.to_lowercase());
    }
    // 同时保留整体形式，让完整标识符查询也能对齐。
    let whole = text.to_lowercase();
    if !out.contains(&whole) {
        out.push(whole);
    }
    out
}

fn is_cjk(character: char) -> bool {
    matches!(character as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

/// 统计每条输入被请求了多少次，用于验证缓存复用。
pub fn input_counts(requests: &[RecordedRequest]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for request in requests {
        for input in &request.inputs {
            *counts.entry(input.clone()).or_insert(0) += 1;
        }
    }
    counts
}
