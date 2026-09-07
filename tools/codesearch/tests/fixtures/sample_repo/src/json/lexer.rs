/// JSON 词法分析器。
///
/// 只处理测试需要的最小子集，不追求完整的 JSON 规范。
pub struct JsonLexer {
    input: Vec<char>,
    position: usize,
}

/// 词法单元。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// 左花括号。
    BraceOpen,
    /// 右花括号。
    BraceClose,
    /// 字符串字面量。
    Str(String),
    /// 冒号。
    Colon,
}

impl JsonLexer {
    /// 从字符串构造词法分析器。
    pub fn new(input: &str) -> Self {
        Self {
            input: input.chars().collect(),
            position: 0,
        }
    }

    /// 读取下一个 token；输入耗尽时返回 `None`。
    pub fn next_token(&mut self) -> Option<Token> {
        self.skip_whitespace();
        let current = *self.input.get(self.position)?;
        self.position += 1;
        match current {
            '{' => Some(Token::BraceOpen),
            '}' => Some(Token::BraceClose),
            ':' => Some(Token::Colon),
            '"' => Some(Token::Str(self.read_string())),
            _ => None,
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.input.get(self.position), Some(c) if c.is_whitespace()) {
            self.position += 1;
        }
    }

    fn read_string(&mut self) -> String {
        let mut out = String::new();
        while let Some(current) = self.input.get(self.position) {
            self.position += 1;
            if *current == '"' {
                break;
            }
            out.push(*current);
        }
        out
    }
}
