//! 会话标题的模型生成、清洗与来源标记。
//!
//! 首条用户消息截断出来的兜底标题只能反映"用户怎么开口"，遇到"hi"、纯报错粘贴或纯
//! 附件输入就没有信息量。这里在轮次成功结束后追加一次极小的模型调用，用首问首答概括
//! 主题，并把结果写回 `SessionRecord.title`。
//!
//! 三条边界：
//!
//! - **不进 Agent 循环**。标题是界面元数据，不参与 ReAct、不写消息、不经过 Plugin Host。
//! - **不信任模型输出**。模型返回的字符串一律经过 [`sanitize_title`] 清洗，长度与字符集
//!   都收敛到单行可显示范围内。
//! - **失败静默**。模型不可用、返回空或超时都保留兜底标题，只有 `metadata` 里的来源标记
//!   决定是否已经取过名字。

use crate::app::App;
use crate::event::UiEvent;
use agent_core::model::{
    MessageRole, ModelGateway, ModelMessage, ModelRequest, PromptCacheRetention, ReasoningLevel,
    ToolChoice,
};
use agent_session::{SessionId, SessionRecord};
use anyhow::{anyhow, Result};
use std::sync::Arc;

/// 标题在会话列表中的最大显示宽度（终端列）。
///
/// 面板一行还要放消息数与相对时间，24 列足够表达主题，也留得下右侧元数据。
pub(crate) const TITLE_MAX_WIDTH: usize = 24;

/// 送给模型的首问首答各自的最大字符数。
///
/// 判断话题只需要开场几段，长日志和整段代码不会让标题更准，只会增加费用与延迟。
const TITLE_CONTEXT_CHARS: usize = 500;

/// 标题请求的最大输出 token 数。
///
/// 标题是十几个字，给 64 已经足够；上限越小，模型越不会附带解释。
const TITLE_MAX_TOKENS: u32 = 64;

/// `SessionRecord.metadata` 中记录标题来源的键。
pub(crate) const TITLE_SOURCE_KEY: &str = "title.source";

/// 由模型生成并写入的标题来源标记值。
const TITLE_SOURCE_MODEL: &str = "model";

/// 会话标题的系统提示。
///
/// 明确要求"只输出标题"并把长度、语言、禁止前缀写在同一处：标题是单行紧凑字符串，
/// 模型任何寒暄都会让列表出现"好的，标题是……"这类噪声。
const TITLE_SYSTEM_PROMPT: &str = "你负责为终端里的编码会话起标题。只输出标题本身，不要引号、\
标点、序号、前缀或任何解释。标题长度 4 到 16 个字（或等长的词），使用对话的主要语言，\
点明这段对话在做什么。";

/// 会话标题生成器；持有与主 Agent 相同的模型路由。
pub(crate) struct SessionTitleGenerator {
    gateway: ModelGateway,
    provider: String,
    model: String,
    stream: bool,
}

impl SessionTitleGenerator {
    /// 用主 Agent 已生效的路由创建生成器。
    ///
    /// 复用主 Agent 的网关与模型：标题属于同一会话的元数据，换一个小模型只会让命名风格
    /// 与对话内容脱节。`provider` 与 `model` 必须是网关注册表中真实存在的键。
    pub(crate) fn new(
        gateway: ModelGateway,
        provider: impl Into<String>,
        model: impl Into<String>,
        stream: bool,
    ) -> Result<Self> {
        let provider = provider.into();
        let model = model.into();
        if provider.trim().is_empty() || model.trim().is_empty() {
            return Err(anyhow!("会话标题生成的模型路由不能为空"));
        }
        Ok(Self {
            gateway,
            provider,
            model,
            stream,
        })
    }

    /// 让模型为一段对话取标题，返回清洗后的单行结果。
    ///
    /// 关闭工具、推理与提示缓存：这是一次性小请求，开启缓存只会污染主对话的分区统计。
    /// 会话路由使用独立前缀，避免与真实对话的缓存键互相覆盖。
    ///
    /// # Errors
    ///
    /// 模型调用失败、返回空内容或清洗后没有可显示字符时返回错误；调用方只保留兜底标题。
    pub(crate) async fn generate(&self, session_id: &SessionId, prompt: String) -> Result<String> {
        let mut request = ModelRequest::new(
            self.model.clone(),
            vec![ModelMessage::text(MessageRole::User, prompt)],
        );
        request.system = Some(TITLE_SYSTEM_PROMPT.to_string());
        request.tool_choice = ToolChoice::None;
        request.reasoning = ReasoningLevel::Off;
        request.cache_retention = PromptCacheRetention::None;
        request.session_id = Some(format!("session-title:{}", session_id.as_str()));
        request.max_tokens = Some(TITLE_MAX_TOKENS);
        let response = if self.stream {
            self.gateway
                .stream(&self.provider, request)
                .await?
                .result()
                .await?
        } else {
            self.gateway.complete(&self.provider, request).await?
        };
        sanitize_title(&response.text_content())
            .ok_or_else(|| anyhow!("模型没有返回可用的会话标题"))
    }
}

/// 判断记录里的标题是否已经由模型生成。
pub(crate) fn title_from_model(record: &SessionRecord) -> bool {
    record
        .metadata
        .get(TITLE_SOURCE_KEY)
        .and_then(serde_json::Value::as_str)
        == Some(TITLE_SOURCE_MODEL)
}

/// 把模型标题写入记录，并标记来源。
///
/// 标记与标题必须同时写入：只写标题会让下一次运行因为"不知道名字是怎么来的"而重复
/// 请求模型，只写标记又会让界面继续显示兜底标题。
pub(crate) fn apply_model_title(record: &mut SessionRecord, title: String) {
    record.title = Some(title);
    record.metadata.insert(
        TITLE_SOURCE_KEY.to_string(),
        serde_json::Value::String(TITLE_SOURCE_MODEL.to_string()),
    );
}

/// 把模型输出清洗成可显示的单行标题。
///
/// 依次处理：取首个非空行、去掉列表与标题前缀、去掉成对引号、压缩空白、按显示宽度截断。
/// 任一环节后没有内容就返回 `None`——空标题比无标题更糟，它会让列表出现一排空行。
pub(crate) fn sanitize_title(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|line| !line.is_empty())?;
    let line = line
        .trim_start_matches(['#', '-', '*', '>', '·', ' '])
        .trim_start_matches("标题：")
        .trim_start_matches("标题:")
        .trim();
    let line = line.trim_matches(['"', '\'', '`', '“', '”', '‘', '’', '「', '」', '『', '』']);
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    // 省略号自己也占一列：先留出这一列再截断，否则宽字符标题会超出一列。
    let title = if unicode_width::UnicodeWidthStr::width(collapsed.as_str()) <= TITLE_MAX_WIDTH {
        collapsed
    } else {
        crate::ui::markdown::truncate_line(&collapsed, TITLE_MAX_WIDTH - 1)
    };
    (!title.is_empty()).then_some(title)
}

/// 从会话消息中提取首问首答，拼成标题请求的用户消息。
///
/// 固定取"第一条用户文本 + 第一条助手文本"，不看后续轮次：这样同一个会话在任何时候补名
/// 都会得到同一份输入，旧会话回填与当场生成的结果一致。纯附件输入没有文本时返回 `None`。
pub(crate) fn title_prompt(messages: &[ModelMessage]) -> Option<String> {
    let first_text = |role: MessageRole| {
        messages
            .iter()
            .filter(|message| message.role == role)
            .map(ModelMessage::text_content)
            .map(|text| text.trim().to_string())
            .find(|text| !text.is_empty())
            .map(|text| truncate_chars(&text, TITLE_CONTEXT_CHARS))
    };
    let question = first_text(MessageRole::User)?;
    let answer = first_text(MessageRole::Assistant)?;
    Some(format!(
        "用户：{question}\n\n助手：{answer}\n\n请为这段对话取标题。"
    ))
}

/// 按字符（而不是字节）截断文本，避免把多字节字符切成半个。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((boundary, _)) => format!("{}…", &text[..boundary]),
        None => text.to_string(),
    }
}

impl App {
    /// 在轮次成功结束后安排一次标题生成。
    ///
    /// 只在"还没有模型标题"且"本进程尚未为该会话尝试过"时触发：失败会保留兜底标题，
    /// 每次运行都重试只会把一次故障变成持续的费用与延迟。用户切换会话后尝试记录失效，
    /// 新会话重新计数。
    pub(crate) fn schedule_session_title(&mut self) {
        let Some(generator) = self.title_generator.as_ref().map(Arc::clone) else {
            return;
        };
        if title_from_model(&self.session_record) {
            return;
        }
        if self.title_attempted.as_ref() == Some(&self.session_record.id) {
            return;
        }
        let Some(prompt) = title_prompt(self.session_record.session.messages()) else {
            return;
        };
        let session_id = self.session_record.id.clone();
        self.title_attempted = Some(session_id.clone());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = generator
                .generate(&session_id, prompt)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = tx.send(UiEvent::SessionTitleReady { session_id, result });
        });
    }

    /// 接收标题生成结果并暂存，等待写入会话存储。
    ///
    /// 会话已经切换或结果失败时直接丢弃。成功时只暂存，不立刻改 `session_record`：
    /// 运行中的轮次持有一份更早的记录副本，等它落盘再由 [`flush_session_title`] 应用，
    /// 才能避免用旧 revision 覆盖刚写入的消息。
    pub(crate) fn handle_session_title(
        &mut self,
        session_id: SessionId,
        result: std::result::Result<String, String>,
    ) {
        if self.session_record.id != session_id {
            return;
        }
        let Ok(title) = result else {
            return;
        };
        if title_from_model(&self.session_record) {
            return;
        }
        self.pending_session_title = Some(title);
    }
}

/// 把暂存的标题写入会话存储，成功后同步本地记录。
///
/// 运行中或还有排队输入时直接返回：此时 `session_record` 即将被这一次运行的完成态替换，
/// 现在保存会与运行自身的 CAS 抢同一个 revision，冲突会把它挤进分叉保存路径。等下一个
/// 完成态落地后再写，标题只是延后几秒出现。
pub(crate) async fn flush_session_title(app: &mut App) {
    if app.running || !app.queued_inputs.is_empty() {
        return;
    }
    let Some(title) = app.pending_session_title.clone() else {
        return;
    };
    if title_from_model(&app.session_record) {
        app.pending_session_title = None;
        return;
    }
    let mut record = app.session_record.clone();
    apply_model_title(&mut record, title);
    let expected_revision = (record.revision > 0).then_some(record.revision);
    match app
        .session_store
        .save(record.clone(), expected_revision)
        .await
    {
        Ok(saved) => {
            app.session_record = saved;
            app.pending_session_title = None;
            // 面板已打开时列表是加载时快照，标题要等下次打开才可见；这里不重载，避免
            // 浏览动作把刚写入的会话再读一遍。
        }
        Err(_) => {
            // 记录尚未落盘或 revision 已前进：保留暂存内容，交给下一次完成态再试。
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::model::{ChatModel, ModelEventStream, ModelResponse, ProviderAdapter};
    use agent_core::Session;
    use agent_session::{MemorySessionStore, SessionStore};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    /// 记录收到的请求并返回固定文本，用于验证请求形态与清洗链路。
    struct StubTitleModel {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
        reply: &'static str,
    }

    #[async_trait::async_trait]
    impl ChatModel for StubTitleModel {
        async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
            self.requests
                .lock()
                .expect("标题请求记录锁不应中毒")
                .push(req);
            Ok(ModelResponse::text(self.reply))
        }

        async fn stream(&self, req: ModelRequest) -> ModelEventStream {
            self.requests
                .lock()
                .expect("标题请求记录锁不应中毒")
                .push(req);
            let (sender, stream) = ModelEventStream::channel();
            sender.done(ModelResponse::text(self.reply));
            stream
        }
    }

    impl ProviderAdapter for StubTitleModel {
        fn name(&self) -> &'static str {
            "stub-title"
        }
    }

    /// 构造一个已注册固定回复的生成器，并返回它记录的请求。
    fn stub_generator(
        reply: &'static str,
    ) -> (SessionTitleGenerator, Arc<Mutex<Vec<ModelRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let gateway = ModelGateway::new();
        gateway
            .register(
                "title-test",
                Arc::new(StubTitleModel {
                    requests: Arc::clone(&requests),
                    reply,
                }),
            )
            .expect("注册标题测试模型");
        let generator = SessionTitleGenerator::new(gateway, "title-test", "title-model", false)
            .expect("生成器应可创建");
        (generator, requests)
    }

    /// 构造一段首问首答会话。
    fn record_with_dialogue() -> SessionRecord {
        let mut record =
            SessionRecord::new(SessionId::generate(), Session::new()).expect("创建测试记录");
        record.session = Session::from_parts(
            None,
            vec![
                ModelMessage::text(MessageRole::User, "bun check 在 CI 上失败，怎么排查？"),
                ModelMessage::text(MessageRole::Assistant, "先看 clippy 步骤的输出。"),
            ],
        );
        record
    }

    /// 模型常带引号、序号或"标题："前缀；这些都要在入库前剥掉。
    #[test]
    fn sanitize_strips_prefixes_quotes_and_extra_lines() {
        for raw in [
            "「clippy 告警清零」",
            "\"clippy 告警清零\"",
            "`clippy 告警清零`",
            "# clippy 告警清零",
            "标题：clippy 告警清零",
            "  clippy 告警清零  \n\n解释：这是标题",
        ] {
            assert_eq!(
                sanitize_title(raw).as_deref(),
                Some("clippy 告警清零"),
                "raw={raw:?}"
            );
        }
    }

    /// 压缩内部空白，避免标题在列表里被撑成两行的视觉噪点。
    #[test]
    fn sanitize_collapses_internal_whitespace() {
        assert_eq!(
            sanitize_title("ci   失败\t排查").as_deref(),
            Some("ci 失败 排查")
        );
    }

    /// 空输出必须判为失败：空标题会让列表出现一排空行，比兜底标题更糟。
    #[test]
    fn sanitize_rejects_empty_output() {
        for raw in ["", "   ", "\n\n", "\"\"", "标题：", "```"] {
            assert_eq!(sanitize_title(raw), None, "raw={raw:?}");
        }
    }

    /// 超宽标题按终端列截断，多字节字符不能被切半。
    #[test]
    fn sanitize_truncates_to_display_width() {
        let title = sanitize_title(&"很".repeat(80)).expect("应得到截断后的标题");
        assert!(
            unicode_width::UnicodeWidthStr::width(title.as_str()) <= TITLE_MAX_WIDTH,
            "标题宽度超限：{title}（{} 列）",
            unicode_width::UnicodeWidthStr::width(title.as_str())
        );
        assert!(title.ends_with('…'));
    }

    /// 输入固定取首问首答：后续轮次不参与，旧会话回填与当场生成才会得到同一份输入。
    #[test]
    fn prompt_uses_first_question_and_answer_only() {
        let messages = vec![
            ModelMessage::text(MessageRole::User, "第一条提问"),
            ModelMessage::text(MessageRole::Assistant, "第一条回答"),
            ModelMessage::text(MessageRole::User, "第二条提问"),
            ModelMessage::text(MessageRole::Assistant, "第二条回答"),
        ];
        let prompt = title_prompt(&messages).expect("首问首答齐全时应生成提示");
        assert!(prompt.contains("第一条提问"));
        assert!(prompt.contains("第一条回答"));
        assert!(!prompt.contains("第二条提问"));
        assert!(!prompt.contains("第二条回答"));
    }

    /// 只有提问没有回答时不请求模型：此时只能猜意图，等回答落地再取名更准。
    #[test]
    fn prompt_requires_an_answer() {
        let messages = vec![ModelMessage::text(MessageRole::User, "只有提问")];
        assert_eq!(title_prompt(&messages), None);
    }

    /// 纯附件输入没有文本，不应生成空提示去问模型。
    #[test]
    fn prompt_requires_text_content() {
        assert_eq!(title_prompt(&[]), None);
    }

    /// 长日志只截取开头，避免为标题付出整段日志的输入费用。
    #[test]
    fn prompt_truncates_long_turns() {
        let messages = vec![
            ModelMessage::text(MessageRole::User, "问".repeat(2_000)),
            ModelMessage::text(MessageRole::Assistant, "答".repeat(2_000)),
        ];
        let prompt = title_prompt(&messages).expect("应有提示");
        assert!(
            prompt.chars().count() < 1_100,
            "提示长度应被截断：{}",
            prompt.chars().count()
        );
        assert!(prompt.contains('…'));
    }

    /// 标题请求必须关闭工具、推理与缓存，并使用独立的会话路由键。
    #[tokio::test]
    async fn request_disables_tools_reasoning_and_cache() {
        let (generator, requests) = stub_generator("clippy 告警清零");
        let session_id = SessionId::new("title-session").expect("创建测试会话标识");
        let title = generator
            .generate(&session_id, "用户：问\n\n助手：答".to_string())
            .await
            .expect("应返回标题");

        assert_eq!(title, "clippy 告警清零");
        let requests = requests.lock().expect("请求记录锁不应中毒");
        let request = requests.first().expect("应记录一次请求");
        assert_eq!(request.model, "title-model");
        assert_eq!(request.tool_choice, ToolChoice::None);
        assert_eq!(request.reasoning, ReasoningLevel::Off);
        assert_eq!(request.cache_retention, PromptCacheRetention::None);
        assert_eq!(request.max_tokens, Some(TITLE_MAX_TOKENS));
        assert!(request.tools.is_empty());
        assert_eq!(
            request.session_id.as_deref(),
            Some("session-title:title-session"),
            "标题请求必须使用独立缓存路由"
        );
        assert_eq!(request.system.as_deref(), Some(TITLE_SYSTEM_PROMPT));
    }

    /// 模型返回空内容时判为失败，调用方保留兜底标题。
    #[tokio::test]
    async fn generate_rejects_empty_model_output() {
        let (generator, _requests) = stub_generator("   \n  ");
        let session_id = SessionId::new("empty-title").expect("创建测试会话标识");
        assert!(generator
            .generate(&session_id, "用户：问\n\n助手：答".to_string())
            .await
            .is_err());
    }

    /// 来源标记与标题必须同时写入，否则下一次运行会重复请求模型。
    #[test]
    fn apply_marks_source_together_with_title() {
        let mut record = record_with_dialogue();
        assert!(!title_from_model(&record));
        apply_model_title(&mut record, "CI 排查".to_string());
        assert_eq!(record.title.as_deref(), Some("CI 排查"));
        assert!(title_from_model(&record));
    }

    /// 会话切换后到达的标题必须丢弃，不能写到新会话上。
    #[tokio::test]
    async fn handle_drops_title_for_other_session() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        let other = SessionId::generate();
        app.handle_session_title(other, Ok("别的会话".to_string()));
        assert_eq!(app.pending_session_title, None);
    }

    /// 只暂存不直接改记录：运行中的轮次持有一份独立副本，提前改会被它覆盖。
    #[tokio::test]
    async fn handle_stages_title_without_touching_record() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        let session_id = app.session_record.id.clone();
        app.handle_session_title(session_id, Ok("待写入标题".to_string()));
        assert_eq!(app.pending_session_title.as_deref(), Some("待写入标题"));
        assert_eq!(app.session_record.title, None);
    }

    /// 切换会话时必须丢弃上一个会话的暂存标题，否则新会话会继承旧名字。
    #[tokio::test]
    async fn replace_session_drops_staged_title() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.pending_session_title = Some("旧会话标题".to_string());

        app.replace_session(record_with_dialogue(), None);

        assert_eq!(app.pending_session_title, None);
    }

    /// 空闲时暂存标题应写入存储，并推进 revision。
    #[tokio::test]
    async fn flush_persists_title_and_bumps_revision() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        let record = record_with_dialogue();
        let saved = store.save(record, None).await.expect("首次保存测试记录");
        let revision = saved.revision;
        let mut app = App::new(tx, "测试模型".into())
            .with_persistent_session(Arc::clone(&store), saved.clone());
        app.pending_session_title = Some("CI 排查".to_string());

        flush_session_title(&mut app).await;

        assert_eq!(app.pending_session_title, None, "写入成功后应清空暂存");
        assert!(app.session_record.revision > revision, "标题写入是新修订");
        let stored = store
            .load(&saved.id)
            .await
            .expect("读取会话")
            .expect("会话应存在");
        assert_eq!(stored.title.as_deref(), Some("CI 排查"));
        assert!(title_from_model(&stored));
    }

    /// 运行中或还有排队输入时不写入：此刻保存会与那一轮的 CAS 抢同一个 revision。
    #[tokio::test]
    async fn flush_waits_while_a_run_is_active() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        let record = record_with_dialogue();
        let saved = store.save(record, None).await.expect("首次保存测试记录");
        let mut app = App::new(tx, "测试模型".into())
            .with_persistent_session(Arc::clone(&store), saved.clone());
        app.running = true;
        app.pending_session_title = Some("CI 排查".to_string());

        flush_session_title(&mut app).await;

        assert_eq!(
            app.pending_session_title.as_deref(),
            Some("CI 排查"),
            "运行中应保留暂存，等完成态落地后再写"
        );
        let stored = store
            .load(&saved.id)
            .await
            .expect("读取会话")
            .expect("会话应存在");
        assert_eq!(stored.title, None, "运行期间不得改写存储");
    }

    /// 请求过标题的会话在本进程内不重复请求；不同会话各自计数。
    #[tokio::test]
    async fn schedule_attempts_once_per_session() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (generator, _requests) = stub_generator("CI 排查");
        let record = record_with_dialogue();
        let session_id = record.id.clone();
        let mut app = App::new(tx, "测试模型".into())
            .with_title_generator(Arc::new(generator))
            .with_persistent_session(Arc::new(MemorySessionStore::new()), record);

        app.schedule_session_title();
        assert_eq!(app.title_attempted.as_ref(), Some(&session_id));
        // 生成在后台任务里完成，事件要等调度器跑一轮。
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("应发出标题事件")
            .expect("事件通道不应关闭");
        assert!(
            matches!(
                event,
                UiEvent::SessionTitleReady { session_id: id, result: Ok(_) } if id == session_id
            ),
            "应请求当前会话的标题"
        );

        // 同一会话再跑一轮：不重复请求，也不产生第二个事件。
        app.schedule_session_title();
        assert!(rx.try_recv().is_err(), "同一会话不应重复请求标题");
    }

    /// 没有生成器（demo 模式、测试环境）时完全不动，保留首条消息兜底标题。
    #[tokio::test]
    async fn schedule_is_noop_without_generator() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx, "测试模型".into());
        app.schedule_session_title();
        assert_eq!(app.title_attempted, None);
    }

    /// 已有模型标题的会话不再请求。
    #[tokio::test]
    async fn schedule_skips_sessions_with_model_title() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (generator, _requests) = stub_generator("CI 排查");
        let mut record = record_with_dialogue();
        apply_model_title(&mut record, "已有标题".to_string());
        let mut app = App::new(tx, "测试模型".into())
            .with_title_generator(Arc::new(generator))
            .with_persistent_session(Arc::new(MemorySessionStore::new()), record);

        app.schedule_session_title();
        assert_eq!(app.title_attempted, None, "已有模型标题不应再请求");
    }
}
