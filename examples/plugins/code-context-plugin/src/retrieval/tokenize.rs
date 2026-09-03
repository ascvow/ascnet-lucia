//! 检索词元切分。
//!
//! 代码检索的词元切分与普通文本检索不同：查询往往是 `PermissionManager`、`ALLOW_ALWAYS`、
//! `tool_call_id`、`execute_command` 这类**标识符**，而它们在源码里以各种命名风格出现。
//! 只按空白切词会让"找 permission 相关的代码"完全命中不到 `PermissionManager`。
//!
//! # 切分规则
//!
//! ```text
//! PermissionManager   → permissionmanager, permission, manager
//! ALLOW_ALWAYS        → allow, always
//! tool_call_id        → tool, call, id
//! parseJSONData       → parsejsondata, parse, json, data
//! HTTPServer          → httpserver, http, server
//! src/permission.rs   → src, permission, rs
//! ```
//!
//! 完整标识符本身也作为词元保留，使精确标识符查询得到比子词匹配更强的信号。
//!
//! # 中日韩文本
//!
//! 本仓库的文档注释是中文，而中文没有词边界。这里采用检索领域对无分词器场景的通行做法：
//! **同时产出单字与双字（bigram）词元**。单字保证召回，双字提供区分度——只有单字时
//! "权限"与"限权"无法区分。不引入分词器：那会带来词典依赖与体积代价，收益不成比例。

/// 词元的最小长度（按字符计）。
///
/// 单个 ASCII 字母几乎不携带检索信号，且会让倒排表急剧膨胀。中日韩单字不受此限制——
/// 单个汉字往往就是一个完整语素。
const MIN_TOKEN_CHARS: usize = 2;

/// 单次切分产出的最大词元数。
///
/// 防御异常输入（压缩后的单行 JS、超长生成代码）撑爆内存。
const MAX_TOKENS: usize = 4096;

/// 判断是否为中日韩表意文字或假名。
///
/// 除切词外，token 预算估算也需要它：中日韩字符与 ASCII 的 token 密度差一个量级，
/// 用同一套系数会让预算估算偏离数倍。
pub(crate) fn is_cjk(character: char) -> bool {
    matches!(character as u32,
        0x3040..=0x30FF      // 平假名、片假名
        | 0x3400..=0x4DBF    // 扩展 A
        | 0x4E00..=0x9FFF    // 基本区
        | 0xF900..=0xFAFF    // 兼容表意文字
        | 0x20000..=0x2A6DF  // 扩展 B
    )
}

/// 把驼峰与帕斯卡命名拆成子词。
///
/// 两类边界：小写或数字后接大写（`fooBar`），以及连续大写后接小写（`HTTPServer` 的
/// `P` 与 `S` 之间）。后者是首字母缩写结尾的标志，缺了它 `HTTPServer` 会被整体保留，
/// 查询 `server` 就命中不到。
fn split_camel_case(part: &str) -> Vec<String> {
    let chars: Vec<char> = part.chars().collect();
    if chars.len() < 2 {
        return vec![part.to_string()];
    }
    let mut pieces = Vec::new();
    let mut start = 0usize;
    for index in 1..chars.len() {
        let previous = chars[index - 1];
        let current = chars[index];
        let enters_word = !previous.is_uppercase() && current.is_uppercase();
        let acronym_ends = previous.is_uppercase()
            && current.is_uppercase()
            && chars.get(index + 1).is_some_and(|next| next.is_lowercase());
        if enters_word || acronym_ends {
            pieces.push(chars[start..index].iter().collect());
            start = index;
        }
    }
    pieces.push(chars[start..].iter().collect());
    pieces
}

/// 把中日韩字符串切成单字与双字词元。
fn push_cjk_tokens(run: &[char], out: &mut Vec<String>) {
    for (index, character) in run.iter().enumerate() {
        out.push(character.to_string());
        if let Some(next) = run.get(index + 1) {
            out.push([*character, *next].iter().collect());
        }
    }
}

/// 把一段文本切成检索词元。
///
/// 输出全部小写。同一词元可能重复出现，调用方据此统计词频。
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut ascii_part = String::new();
    let mut cjk_run: Vec<char> = Vec::new();

    /// 处理累积的 ASCII 标识符片段。
    fn flush_ascii(part: &mut String, out: &mut Vec<String>) {
        if part.is_empty() {
            return;
        }
        let lower = part.to_lowercase();
        let pieces = split_camel_case(part);
        if pieces.len() > 1 {
            // 完整标识符与其子词都入索引：前者让精确查询更强，后者提供部分匹配能力。
            if lower.chars().count() >= MIN_TOKEN_CHARS {
                out.push(lower);
            }
            for piece in pieces {
                let piece = piece.to_lowercase();
                if piece.chars().count() >= MIN_TOKEN_CHARS {
                    out.push(piece);
                }
            }
        } else if lower.chars().count() >= MIN_TOKEN_CHARS {
            out.push(lower);
        }
        part.clear();
    }

    for character in text.chars() {
        if is_cjk(character) {
            flush_ascii(&mut ascii_part, &mut out);
            cjk_run.push(character);
            continue;
        }
        if !cjk_run.is_empty() {
            push_cjk_tokens(&cjk_run, &mut out);
            cjk_run.clear();
        }
        if character.is_alphanumeric() || character == '_' {
            // 下划线在这里不参与累积：它是命名分隔符，直接触发一次切分。
            if character == '_' {
                flush_ascii(&mut ascii_part, &mut out);
            } else {
                ascii_part.push(character);
            }
        } else {
            flush_ascii(&mut ascii_part, &mut out);
        }
        if out.len() >= MAX_TOKENS {
            return out;
        }
    }
    flush_ascii(&mut ascii_part, &mut out);
    if !cjk_run.is_empty() {
        push_cjk_tokens(&cjk_run, &mut out);
    }
    out.truncate(MAX_TOKENS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 断言切分结果包含全部期望词元。
    fn assert_contains(text: &str, expected: &[&str]) {
        let tokens = tokenize(text);
        for item in expected {
            assert!(
                tokens.iter().any(|token| token == item),
                "{text} 的词元 {tokens:?} 应包含 {item}"
            );
        }
    }

    /// 帕斯卡命名必须同时产出完整标识符与子词。
    #[test]
    fn splits_pascal_case() {
        assert_contains(
            "PermissionManager",
            &["permissionmanager", "permission", "manager"],
        );
    }

    /// 驼峰命名必须被拆开。
    #[test]
    fn splits_camel_case() {
        assert_contains("toolCallId", &["toolcallid", "tool", "call", "id"]);
    }

    /// 首字母缩写后接单词必须正确断开。
    #[test]
    fn splits_acronym_boundaries() {
        assert_contains("HTTPServer", &["httpserver", "http", "server"]);
        assert_contains("parseJSONData", &["parse", "json", "data"]);
    }

    /// 蛇形命名必须按下划线切分。
    #[test]
    fn splits_snake_case() {
        assert_contains("tool_call_id", &["tool", "call", "id"]);
        assert_contains("execute_command", &["execute", "command"]);
    }

    /// 全大写常量必须被拆成单词。
    #[test]
    fn splits_screaming_snake_case() {
        assert_contains("ALLOW_ALWAYS", &["allow", "always"]);
    }

    /// 路径必须按分隔符切分。
    #[test]
    fn splits_paths() {
        assert_contains(
            "src/permission/manager.rs",
            &["src", "permission", "manager"],
        );
    }

    /// 限定名必须按分隔符切分。
    #[test]
    fn splits_qualified_names() {
        assert_contains(
            "PermissionManager::check",
            &["permission", "manager", "check"],
        );
    }

    /// 数字后接大写必须断开。
    #[test]
    fn splits_after_digits() {
        assert_contains("sha256Digest", &["sha256", "digest"]);
    }

    /// 单字符词元必须被丢弃，避免倒排表被噪声撑爆。
    #[test]
    fn drops_single_character_tokens() {
        let tokens = tokenize("a b cd");
        assert!(!tokens.iter().any(|token| token == "a"));
        assert!(tokens.iter().any(|token| token == "cd"));
    }

    /// 中文必须产出单字与双字词元。
    #[test]
    fn produces_cjk_unigrams_and_bigrams() {
        let tokens = tokenize("权限检查");
        assert!(tokens.iter().any(|token| token == "权"));
        assert!(tokens.iter().any(|token| token == "权限"));
        assert!(tokens.iter().any(|token| token == "检查"));
    }

    /// 中英混排必须各自正确切分。
    #[test]
    fn handles_mixed_cjk_and_ascii() {
        assert_contains(
            "判断 PermissionManager 是否需要审批",
            &["permission", "manager", "判断", "审批"],
        );
    }

    /// 自然语言查询必须被切成可匹配的词元。
    #[test]
    fn tokenizes_natural_language_query() {
        assert_contains(
            "where does the agent decide whether a tool requires user approval?",
            &["agent", "decide", "tool", "requires", "user", "approval"],
        );
    }

    /// 空输入与纯符号输入不得 panic 或产出空串词元。
    #[test]
    fn handles_degenerate_input() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("!!! ??? ...").is_empty());
        assert!(tokenize("___").is_empty());
    }

    /// 词元数量必须有上限，防御压缩代码这类异常输入。
    #[test]
    fn caps_token_count() {
        let huge: String = (0..5000).map(|index| format!("word{index} ")).collect();
        assert!(tokenize(&huge).len() <= MAX_TOKENS);
    }
}
