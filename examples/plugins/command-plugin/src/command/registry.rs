//! 命令注册、解析、补全和内建命令定义。

use super::*;

/// 注册表内部保存的命令 owner、定义和可选内置处理器。
#[derive(Debug, Clone)]
pub(super) struct RegisteredCommand {
    pub(super) owner_plugin_id: String,
    pub(super) spec: CommandSpec,
    pub(super) builtin: Option<BuiltinCommand>,
}

/// 命令准备阶段的内部结果。
pub(super) enum Prepared {
    Builtin {
        command: BuiltinCommand,
        invocation: CommandInvocation,
    },
    Callback {
        owner_plugin_id: String,
        handler: CommandHandlerRef,
        invocation: CommandInvocation,
    },
    Error {
        message: String,
        usage: Option<String>,
    },
}

/// 官方内置命令的稳定路由标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BuiltinCommand {
    Help,
    Resume,
    New,
    Sessions,
    Clear,
    /// 请求官方 Context 插件在下一轮强制压缩较旧历史。
    Compact,
    Exit,
}

/// 按规范名称和别名索引命令的内存注册表。
#[derive(Debug)]
pub(super) struct CommandRegistry {
    pub(super) commands: BTreeMap<String, RegisteredCommand>,
    pub(super) aliases: BTreeMap<String, String>,
    pub(super) generation: u64,
}

impl CommandRegistry {
    /// 创建只包含官方默认命令的第一代注册表。
    pub(super) fn with_builtins() -> Self {
        let mut registry = Self {
            commands: BTreeMap::new(),
            aliases: BTreeMap::new(),
            generation: 1,
        };
        for (spec, command) in builtin_specs() {
            registry.insert_unchecked(RegisteredCommand {
                owner_plugin_id: BUILTIN_OWNER.into(),
                spec,
                builtin: Some(command),
            });
        }
        registry
    }

    /// 注册或替换同一 owner 的命令，同时保持冲突检查事务性。
    pub(super) fn register(
        &mut self,
        owner_plugin_id: String,
        spec: CommandSpec,
    ) -> Result<String> {
        validate_spec(&spec, false)?;
        let canonical = spec.name.clone();
        let existing = self.commands.get(&canonical);
        if let Some(existing) = existing {
            if existing.builtin.is_some() || existing.owner_plugin_id != owner_plugin_id {
                return Err(anyhow!("命令 `/{canonical}` 已由其他插件注册"));
            }
        }

        let excluded = existing
            .map(|entry| command_names(&entry.spec))
            .unwrap_or_default();
        for name in command_names(&spec) {
            if excluded.contains(&name) {
                continue;
            }
            if self.commands.contains_key(&name) || self.aliases.contains_key(&name) {
                return Err(anyhow!("命令名称或别名 `/{name}` 已被占用"));
            }
        }

        if existing.is_some() {
            self.remove_unchecked(&canonical);
        }
        self.insert_unchecked(RegisteredCommand {
            owner_plugin_id,
            spec,
            builtin: None,
        });
        self.generation = self.generation.saturating_add(1);
        Ok(canonical)
    }

    /// 注销命令，未知名称保持幂等且不递增 generation。
    pub(super) fn unregister(&mut self, owner_plugin_id: &str, name: &str) -> Result<bool> {
        let Some(canonical) = canonical_command_name(name) else {
            return Err(anyhow!("无效命令名：{name}"));
        };
        let canonical = self
            .resolve_name(&canonical)
            .map(str::to_string)
            .unwrap_or(canonical);
        let Some(entry) = self.commands.get(&canonical) else {
            return Ok(false);
        };
        if entry.builtin.is_some() {
            return Err(anyhow!("官方内置命令 `/{canonical}` 不能被注销"));
        }
        if entry.owner_plugin_id != owner_plugin_id {
            return Err(anyhow!("不能注销其他插件拥有的命令 `/{canonical}`"));
        }
        self.remove_unchecked(&canonical);
        self.generation = self.generation.saturating_add(1);
        Ok(true)
    }

    /// 返回按规范名称排序且不暴露 owner 的命令定义。
    pub(super) fn snapshot(&self) -> CommandSnapshot {
        CommandSnapshot {
            generation: self.generation,
            commands: self
                .commands
                .values()
                .map(|entry| entry.spec.clone())
                .collect(),
        }
    }

    /// 批量移除回调服务已从 Host 目录消失的第三方命令。
    pub(super) fn prune_unavailable_handlers(
        &mut self,
        services_by_owner: &BTreeMap<String, BTreeSet<String>>,
    ) -> usize {
        let stale = self
            .commands
            .iter()
            .filter_map(|(name, entry)| {
                if entry.builtin.is_some() {
                    return None;
                }
                let handler = entry.spec.handler.as_ref()?;
                let available = services_by_owner
                    .get(&entry.owner_plugin_id)
                    .is_some_and(|services| services.contains(&handler.service));
                (!available).then(|| name.clone())
            })
            .collect::<Vec<_>>();
        for name in &stale {
            self.remove_unchecked(name);
        }
        if !stale.is_empty() {
            self.generation = self.generation.saturating_add(1);
        }
        stale.len()
    }

    /// 识别当前参数，并按注册时保存的可信 owner 生成本地、回调或宿主计划。
    pub(super) fn prepare_completion(
        &self,
        request: PrepareCompletionRequest,
    ) -> PrepareCompletionResponse {
        let cursor = match request.cursor {
            Some(cursor) => match usize::try_from(cursor) {
                Ok(cursor) => cursor,
                Err(_) => {
                    return PrepareCompletionResponse::Error {
                        message: "补全光标超出当前平台支持范围".into(),
                    }
                }
            },
            None => request.input.len(),
        };
        let cursor = match parse_completion_cursor(&request.input, cursor) {
            Ok(Some(cursor)) => cursor,
            Ok(None) => return PrepareCompletionResponse::NoMatch,
            Err(message) => return PrepareCompletionResponse::Error { message },
        };
        let Some(name) = canonical_command_name(&cursor.command) else {
            return PrepareCompletionResponse::NoMatch;
        };
        let Some(canonical) = self.resolve_name(&name) else {
            return PrepareCompletionResponse::NoMatch;
        };
        let entry = &self.commands[canonical];
        let Some((argument_index, argument)) =
            completion_argument(&entry.spec, cursor.argument_index)
        else {
            return PrepareCompletionResponse::NoMatch;
        };
        let limit = normalize_completion_limit(request.limit);
        let argument_index = match u16::try_from(argument_index) {
            Ok(index) => index,
            Err(_) => {
                return PrepareCompletionResponse::Error {
                    message: "命令参数数量超出补全协议支持范围".into(),
                }
            }
        };
        let context = CompletionContext {
            command: canonical.into(),
            argument: argument.name.clone(),
            argument_index,
            prefix: cursor.prefix.clone(),
            replacement_start: match u32::try_from(cursor.replacement_start) {
                Ok(start) => start,
                Err(_) => {
                    return PrepareCompletionResponse::Error {
                        message: "补全替换范围超出协议支持范围".into(),
                    }
                }
            },
            replacement_end: match u32::try_from(cursor.replacement_end) {
                Ok(end) => end,
                Err(_) => {
                    return PrepareCompletionResponse::Error {
                        message: "补全替换范围超出协议支持范围".into(),
                    }
                }
            },
        };
        let completion_request = CommandCompletionRequest {
            command: canonical.into(),
            argument: argument.name.clone(),
            prefix: cursor.prefix,
            input: request.input,
            limit,
        };

        match &argument.completion {
            CompletionSource::Static { items } => PrepareCompletionResponse::Candidates {
                context,
                items: filter_completion_items(
                    items.iter().cloned(),
                    &completion_request.prefix,
                    limit,
                ),
            },
            CompletionSource::Callback => {
                let Some(handler) = entry.spec.handler.as_ref() else {
                    return PrepareCompletionResponse::NoMatch;
                };
                PrepareCompletionResponse::Callback {
                    context,
                    owner_plugin_id: entry.owner_plugin_id.clone(),
                    service: handler.service.clone(),
                    request: CommandCallbackRequest::Complete {
                        handler_id: handler.handler_id.clone(),
                        request: completion_request,
                    },
                }
            }
            CompletionSource::Surface { source } => PrepareCompletionResponse::Surface {
                context,
                request: SurfaceCompletionRequest {
                    source: source.clone(),
                    request: completion_request,
                },
            },
            CompletionSource::None => match &argument.kind {
                ArgumentKind::Choice { values } => PrepareCompletionResponse::Candidates {
                    context,
                    items: filter_completion_items(
                        values.iter().map(|value| CompletionItem {
                            label: value.clone(),
                            insert_text: value.clone(),
                            description: None,
                        }),
                        &completion_request.prefix,
                        limit,
                    ),
                },
                ArgumentKind::Session => PrepareCompletionResponse::Surface {
                    context,
                    request: SurfaceCompletionRequest {
                        source: SESSION_COMPLETION_SOURCE.into(),
                        request: completion_request,
                    },
                },
                _ => PrepareCompletionResponse::NoMatch,
            },
        }
    }

    /// 解析命令行、解析别名并绑定类型化位置参数。
    pub(super) fn prepare(&self, input: &str, agent_idle: bool) -> Prepared {
        let parsed = match ParsedCommandLine::parse(input) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Prepared::Error {
                    message: error.to_string(),
                    usage: None,
                }
            }
        };
        let Some(name) = canonical_command_name(&parsed.name) else {
            return Prepared::Error {
                message: format!("无效命令名：{}", parsed.name),
                usage: None,
            };
        };
        let Some(canonical) = self.resolve_name(&name) else {
            return Prepared::Error {
                message: format!("未知命令：/{}", parsed.name),
                usage: Some("输入 /help 查看可用命令".into()),
            };
        };
        let entry = &self.commands[canonical];
        if entry.spec.availability == CommandAvailability::IdleOnly && !agent_idle {
            return Prepared::Error {
                message: format!("命令 `/{canonical}` 只能在 Agent 空闲时执行"),
                usage: Some(entry.spec.display_usage()),
            };
        }
        let invocation = match bind_arguments(&entry.spec, parsed.arguments, input) {
            Ok(invocation) => invocation,
            Err(message) => {
                return Prepared::Error {
                    message,
                    usage: Some(entry.spec.display_usage()),
                }
            }
        };
        if let Some(builtin) = entry.builtin {
            return Prepared::Builtin {
                command: builtin,
                invocation,
            };
        }
        let Some(handler) = entry.spec.handler.clone() else {
            return Prepared::Error {
                message: format!("命令 `/{canonical}` 没有可用处理器"),
                usage: Some(entry.spec.display_usage()),
            };
        };
        Prepared::Callback {
            owner_plugin_id: entry.owner_plugin_id.clone(),
            handler,
            invocation,
        }
    }

    /// 生成全部命令或单个命令的帮助文本。
    pub(super) fn help(&self, target: Option<&str>) -> String {
        if let Some(target) = target {
            let target = target.trim_start_matches('/');
            let Some(name) = canonical_command_name(target) else {
                return format!("无效命令名：{target}");
            };
            let Some(canonical) = self.resolve_name(&name) else {
                return format!("未知命令：/{target}");
            };
            let spec = &self.commands[canonical].spec;
            let mut output = format!(
                "{}\n{}\n\n{}",
                spec.display_usage(),
                spec.summary,
                spec.description
            );
            if !spec.aliases.is_empty() {
                output.push_str(&format!("\n\n别名：/{}", spec.aliases.join("、/")));
            }
            if !spec.arguments.is_empty() {
                output.push_str("\n\n参数：");
                for argument in &spec.arguments {
                    output.push_str(&format!("\n  {}  {}", argument.name, argument.description));
                }
            }
            return output;
        }

        let mut output = String::from("可用命令：");
        for entry in self.commands.values() {
            output.push_str(&format!(
                "\n  {:<24} {}",
                entry.spec.display_usage(),
                entry.spec.summary
            ));
        }
        output
    }

    /// 根据规范名称或别名返回规范名称。
    pub(super) fn resolve_name<'a>(&'a self, name: &'a str) -> Option<&'a str> {
        if self.commands.contains_key(name) {
            Some(name)
        } else {
            self.aliases.get(name).map(String::as_str)
        }
    }

    /// 插入已经通过校验且不存在冲突的命令。
    pub(super) fn insert_unchecked(&mut self, entry: RegisteredCommand) {
        let canonical = entry.spec.name.clone();
        for alias in &entry.spec.aliases {
            self.aliases.insert(alias.clone(), canonical.clone());
        }
        self.commands.insert(canonical, entry);
    }

    /// 移除命令及其全部别名，不修改 generation。
    pub(super) fn remove_unchecked(&mut self, canonical: &str) {
        if let Some(entry) = self.commands.remove(canonical) {
            for alias in entry.spec.aliases {
                self.aliases.remove(&alias);
            }
        }
    }
}

/// Provider 在原始输入中识别出的当前参数游标。
struct CompletionCursorState {
    command: String,
    argument_index: usize,
    prefix: String,
    replacement_start: usize,
    replacement_end: usize,
}

/// 解析光标前的宽松命令行状态，并计算当前原始 token 的完整替换范围。
///
/// 与执行解析不同，补全允许光标处存在尚未闭合的引号或转义。
fn parse_completion_cursor(
    input: &str,
    cursor: usize,
) -> std::result::Result<Option<CompletionCursorState>, String> {
    if cursor > input.len() || !input.is_char_boundary(cursor) {
        return Err("补全光标不是有效的 UTF-8 字节位置".into());
    }
    let trimmed = input.trim_start();
    let input_start = input.len().saturating_sub(trimmed.len());
    if !trimmed.starts_with('/') {
        return Err("命令必须以 `/` 开头".into());
    }
    let command_start = input_start.saturating_add(1);
    if cursor <= command_start {
        return Ok(None);
    }
    let before_cursor = &input[command_start..cursor];
    let Some((separator_offset, _)) = before_cursor
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
    else {
        return Ok(None);
    };
    let command_end = command_start + separator_offset;
    let command = &input[command_start..command_end];
    if command.is_empty() {
        return Ok(None);
    }

    let mut argument_index = 0usize;
    let mut token_start = None;
    let mut prefix = String::new();
    let mut quote = None;
    let mut escaped = false;
    for (offset, character) in input[command_end..cursor].char_indices() {
        let absolute = command_end + offset;
        if token_start.is_none() {
            if character.is_whitespace() {
                continue;
            }
            token_start = Some(absolute);
        }
        if escaped {
            prefix.push(character);
            escaped = false;
            continue;
        }
        match (quote, character) {
            (Some('\''), '\'') => quote = None,
            (Some('\''), _) => prefix.push(character),
            (Some('"'), '"') => quote = None,
            (Some('"'), '\\') => escaped = true,
            (Some('"'), _) => prefix.push(character),
            (None, '\'') => quote = Some('\''),
            (None, '"') => quote = Some('"'),
            (None, '\\') => escaped = true,
            (None, value) if value.is_whitespace() => {
                argument_index = argument_index.saturating_add(1);
                token_start = None;
                prefix.clear();
            }
            (None, value) => prefix.push(value),
            (Some(_), _) => unreachable!("补全解析只支持单双引号"),
        }
    }

    let replacement_start = token_start.unwrap_or(cursor);
    let replacement_end = if token_start.is_some() {
        completion_token_end(input, cursor, quote, escaped)
    } else {
        cursor
    };
    Ok(Some(CompletionCursorState {
        command: command.into(),
        argument_index,
        prefix,
        replacement_start,
        replacement_end,
    }))
}

/// 从光标向后找到当前 token 的边界，保留完整 token 替换所需的引号状态。
pub(super) fn completion_token_end(
    input: &str,
    cursor: usize,
    mut quote: Option<char>,
    mut escaped: bool,
) -> usize {
    for (offset, character) in input[cursor..].char_indices() {
        let absolute = cursor + offset;
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, character) {
            (Some('\''), '\'') => quote = None,
            (Some('\''), _) => {}
            (Some('"'), '"') => quote = None,
            (Some('"'), '\\') => escaped = true,
            (Some('"'), _) => {}
            (None, '\'') => quote = Some('\''),
            (None, '"') => quote = Some('"'),
            (None, '\\') => escaped = true,
            (None, value) if value.is_whitespace() => return absolute,
            (None, _) => {}
            (Some(_), _) => unreachable!("补全解析只支持单双引号"),
        }
    }
    input.len()
}

/// 将输入中的参数位置映射到定义；可变参数会持续匹配最后一个定义。
pub(super) fn completion_argument(
    spec: &CommandSpec,
    input_index: usize,
) -> Option<(usize, &ArgumentSpec)> {
    if let Some(argument) = spec.arguments.get(input_index) {
        return Some((input_index, argument));
    }
    let index = spec.arguments.len().checked_sub(1)?;
    let argument = &spec.arguments[index];
    argument.variadic.then_some((index, argument))
}

/// 把调用方上限归一到协议默认值和硬上限之间。
pub(super) fn normalize_completion_limit(limit: u16) -> u16 {
    if limit == 0 {
        DEFAULT_COMPLETION_LIMIT
    } else {
        limit.min(MAX_COMPLETION_LIMIT)
    }
}

/// 按标签或参数值前缀过滤候选，并编码插入文本、应用数量上限。
pub(super) fn filter_completion_items(
    items: impl IntoIterator<Item = CompletionItem>,
    prefix: &str,
    limit: u16,
) -> Vec<CompletionItem> {
    let prefix = prefix.to_lowercase();
    items
        .into_iter()
        .filter(|item| {
            prefix.is_empty()
                || item.label.to_lowercase().starts_with(&prefix)
                || item.insert_text.to_lowercase().starts_with(&prefix)
        })
        .take(usize::from(limit))
        .map(|mut item| {
            item.insert_text = encode_command_token(&item.insert_text);
            item
        })
        .collect()
}

/// 返回命令规范名称与别名的集合。
pub(super) fn command_names(spec: &CommandSpec) -> BTreeSet<String> {
    std::iter::once(spec.name.clone())
        .chain(spec.aliases.iter().cloned())
        .collect()
}

/// 校验外部命令定义，拒绝模糊名称、不可执行命令和歧义参数。
pub(super) fn validate_spec(spec: &CommandSpec, builtin: bool) -> Result<()> {
    let canonical =
        canonical_command_name(&spec.name).ok_or_else(|| anyhow!("无效命令名：{}", spec.name))?;
    if canonical != spec.name {
        return Err(anyhow!("命令名必须是规范小写形式：{canonical}"));
    }
    if spec.summary.trim().is_empty() || spec.description.trim().is_empty() {
        return Err(anyhow!("命令摘要和描述不能为空"));
    }
    if !builtin {
        let handler = spec
            .handler
            .as_ref()
            .ok_or_else(|| anyhow!("第三方命令必须声明 handler"))?;
        if handler.service.trim().is_empty() || handler.handler_id.trim().is_empty() {
            return Err(anyhow!("命令 handler service 和 handler_id 不能为空"));
        }
    }

    let mut names = BTreeSet::from([spec.name.clone()]);
    for alias in &spec.aliases {
        let canonical_alias =
            canonical_command_name(alias).ok_or_else(|| anyhow!("无效命令别名：{alias}"))?;
        if canonical_alias != *alias {
            return Err(anyhow!("命令别名必须是规范小写形式：{canonical_alias}"));
        }
        if !names.insert(alias.clone()) {
            return Err(anyhow!("命令名称或别名重复：{alias}"));
        }
    }

    let mut argument_names = BTreeSet::new();
    let mut optional_seen = false;
    for (index, argument) in spec.arguments.iter().enumerate() {
        if canonical_command_name(&argument.name).as_deref() != Some(argument.name.as_str()) {
            return Err(anyhow!("无效参数名：{}", argument.name));
        }
        if argument.description.trim().is_empty() {
            return Err(anyhow!("参数 `{}` 的描述不能为空", argument.name));
        }
        if !argument_names.insert(argument.name.clone()) {
            return Err(anyhow!("参数名重复：{}", argument.name));
        }
        if optional_seen && argument.required {
            return Err(anyhow!("必填参数不能出现在可选参数之后"));
        }
        optional_seen |= !argument.required;
        if argument.variadic && index + 1 != spec.arguments.len() {
            return Err(anyhow!("可变参数 `{}` 必须位于最后", argument.name));
        }
        if let ArgumentKind::Choice { values } = &argument.kind {
            if values.is_empty() || values.iter().any(|value| value.is_empty()) {
                return Err(anyhow!("Choice 参数 `{}` 必须提供非空候选", argument.name));
            }
        }
        match &argument.completion {
            CompletionSource::Static { items }
                if items.iter().any(|item| item.insert_text.is_empty()) =>
            {
                return Err(anyhow!(
                    "Static 参数 `{}` 的插入文本不能为空",
                    argument.name
                ));
            }
            CompletionSource::Surface { source } if source.trim().is_empty() => {
                return Err(anyhow!("Surface 参数 `{}` 的数据源不能为空", argument.name));
            }
            _ => {}
        }
    }
    Ok(())
}

/// 按命令定义绑定并校验位置参数。
pub(super) fn bind_arguments(
    spec: &CommandSpec,
    values: Vec<String>,
    input: &str,
) -> std::result::Result<CommandInvocation, String> {
    let mut cursor = 0;
    let mut arguments = BTreeMap::new();
    for argument in &spec.arguments {
        let selected = if argument.variadic {
            let selected = values[cursor..].to_vec();
            cursor = values.len();
            selected
        } else if let Some(value) = values.get(cursor) {
            cursor += 1;
            vec![value.clone()]
        } else {
            Vec::new()
        };
        if selected.is_empty() {
            if argument.required {
                return Err(format!("缺少必填参数：{}", argument.name));
            }
            continue;
        }
        for value in &selected {
            validate_argument_value(argument, value)?;
        }
        arguments.insert(argument.name.clone(), selected);
    }
    if cursor < values.len() {
        return Err(format!("命令 `/{}` 收到多余参数", spec.name));
    }
    Ok(CommandInvocation {
        command: spec.name.clone(),
        input: input.into(),
        arguments,
    })
}

/// 根据参数类型校验单个原始值，不改变第三方插件最终收到的文本。
pub(super) fn validate_argument_value(
    argument: &ArgumentSpec,
    value: &str,
) -> std::result::Result<(), String> {
    if value.is_empty() {
        return Err(format!("参数 `{}` 不能为空", argument.name));
    }
    let valid = match &argument.kind {
        ArgumentKind::String | ArgumentKind::Session => true,
        ArgumentKind::Integer => value.parse::<i64>().is_ok(),
        ArgumentKind::Boolean => matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "false" | "yes" | "no" | "1" | "0"
        ),
        ArgumentKind::Choice { values } => values.iter().any(|candidate| candidate == value),
    };
    if valid {
        Ok(())
    } else {
        Err(format!("参数 `{}` 的值无效：{value}", argument.name))
    }
}

/// 构造官方内置命令及其处理器路由。
pub(super) fn builtin_specs() -> Vec<(CommandSpec, BuiltinCommand)> {
    vec![
        (
            CommandSpec::new("help", "查看命令帮助", "显示全部命令或指定命令的详细用法。")
                .with_argument(ArgumentSpec::optional(
                    "command",
                    "不含前导斜杠的命令名",
                    ArgumentKind::String,
                )),
            BuiltinCommand::Help,
        ),
        (
            idle_command(
                "resume",
                "恢复历史会话",
                "打开当前工作目录的会话列表，选择后恢复会话。",
            ),
            BuiltinCommand::Resume,
        ),
        (
            idle_command(
                "new",
                "新建空白会话",
                "结束当前会话并进入不会立即落盘的空白草稿。",
            ),
            BuiltinCommand::New,
        ),
        (
            idle_command(
                "sessions",
                "浏览项目会话",
                "打开当前工作目录的只读会话列表。",
            ),
            BuiltinCommand::Sessions,
        ),
        (
            idle_command(
                "clear",
                "清空当前上下文",
                "清空当前会话上下文并进入新的空白草稿。",
            ),
            BuiltinCommand::Clear,
        ),
        (
            idle_command(
                "compact",
                "主动压缩当前上下文",
                "立即压缩当前会话的较旧历史，并持久化压缩后的上下文。",
            ),
            BuiltinCommand::Compact,
        ),
        (
            idle_command("exit", "退出 Lucia", "请求 TUI 保存状态并正常退出 Lucia。")
                .with_alias("quit"),
            BuiltinCommand::Exit,
        ),
    ]
}

/// 创建只允许 Agent 空闲时执行的内置命令。
pub(super) fn idle_command(name: &str, summary: &str, description: &str) -> CommandSpec {
    let mut spec = CommandSpec::new(name, summary, description);
    spec.availability = CommandAvailability::IdleOnly;
    spec
}
