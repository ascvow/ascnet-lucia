//! Ratatui Evolution Dashboard；复用现有 `lucia` 二进制，不创建第二套应用。

use agent_evolution::{
    EvolutionCertificate, EvolutionHistory, EvolutionScorecard, HeadlineVerdict,
};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table, Wrap},
    Frame,
};
use std::time::Duration;

/// Dashboard 可用的四个页面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardPage {
    /// 首屏核心指标、安全和资源变化。
    Overview,
    /// Task Family × Generation 能力图。
    CapabilityMap,
    /// Candidate、发布、拒绝与回滚节点。
    Lineage,
    /// 可信制品引用与硬门槛明细。
    Evidence,
}

impl DashboardPage {
    /// 返回固定页签标题。
    const fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::CapabilityMap => "Capability Map",
            Self::Lineage => "Lineage",
            Self::Evidence => "Evidence",
        }
    }

    /// 返回下一个页签，末尾回到首个页签。
    const fn next(self) -> Self {
        match self {
            Self::Overview => Self::CapabilityMap,
            Self::CapabilityMap => Self::Lineage,
            Self::Lineage => Self::Evidence,
            Self::Evidence => Self::Overview,
        }
    }

    /// 返回上一个页签，首个页签回到末尾。
    const fn previous(self) -> Self {
        match self {
            Self::Overview => Self::Evidence,
            Self::CapabilityMap => Self::Overview,
            Self::Lineage => Self::CapabilityMap,
            Self::Evidence => Self::Lineage,
        }
    }
}

/// Dashboard 的只读状态。
pub(crate) struct EvolutionDashboardState {
    /// 最近发布的 Scorecard；暂无发布时为 `None`。
    scorecard: Option<EvolutionScorecard>,
    /// 与当前 Release 精确对应的可信 Certificate；未生成时为 `None`。
    certificate: Option<EvolutionCertificate>,
    /// 同一查询范围的历史分析。
    history: Option<EvolutionHistory>,
    /// 加载失败时的完整错误链摘要。
    error: Option<String>,
    /// 当前页签。
    page: DashboardPage,
    /// Evidence 页选中的条目。
    evidence_index: usize,
    /// 主循环退出标记。
    should_quit: bool,
}

impl EvolutionDashboardState {
    /// 创建正常或空状态 Dashboard，并保留与当前 Release 精确匹配的 Certificate。
    ///
    /// `scorecard` 缺失时进入空状态；`certificate` 缺失时 Evidence 对应条目显示 `N/A`。
    pub(crate) fn loaded(
        scorecard: Option<EvolutionScorecard>,
        certificate: Option<EvolutionCertificate>,
        history: EvolutionHistory,
    ) -> Self {
        Self {
            scorecard,
            certificate,
            history: Some(history),
            error: None,
            page: DashboardPage::Overview,
            evidence_index: 0,
            should_quit: false,
        }
    }

    /// 创建可在 TUI 内展示的错误状态。
    pub(crate) fn failed(error: impl Into<String>) -> Self {
        Self {
            scorecard: None,
            certificate: None,
            history: None,
            error: Some(error.into()),
            page: DashboardPage::Overview,
            evidence_index: 0,
            should_quit: false,
        }
    }

    /// 处理不修改任何 Evolution Artifact 的键盘导航。
    fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Tab | KeyCode::Right => self.page = self.page.next(),
            KeyCode::BackTab | KeyCode::Left => self.page = self.page.previous(),
            KeyCode::Char('1') => self.page = DashboardPage::Overview,
            KeyCode::Char('2') => self.page = DashboardPage::CapabilityMap,
            KeyCode::Char('3') => self.page = DashboardPage::Lineage,
            KeyCode::Char('4') => self.page = DashboardPage::Evidence,
            KeyCode::Up if self.page == DashboardPage::Evidence => {
                self.evidence_index = self.evidence_index.saturating_sub(1);
            }
            KeyCode::Down if self.page == DashboardPage::Evidence => {
                let last = evidence_items(self.scorecard.as_ref(), self.certificate.as_ref())
                    .len()
                    .saturating_sub(1);
                self.evidence_index = (self.evidence_index + 1).min(last);
            }
            _ => {}
        }
    }
}

/// 运行只读 Evolution Dashboard，退出时可靠恢复终端。
pub(crate) fn run(mut state: EvolutionDashboardState) -> anyhow::Result<()> {
    /// 保证正常返回和错误返回都恢复终端。
    struct RestoreTerminal;
    impl Drop for RestoreTerminal {
        fn drop(&mut self) {
            ratatui::restore();
        }
    }

    let mut terminal = ratatui::init();
    let _guard = RestoreTerminal;
    while !state.should_quit {
        terminal.draw(|frame| render(frame, &mut state))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    state.handle_key(key.code);
                }
            }
        }
    }
    Ok(())
}

/// 渲染 Dashboard 顶栏、当前页面和底部键盘提示。
fn render(frame: &mut Frame, state: &mut EvolutionDashboardState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    render_tabs(frame, state, chunks[0]);
    if let Some(error) = &state.error {
        render_error(frame, error, chunks[1]);
    } else if state.scorecard.is_none() {
        render_empty(frame, chunks[1]);
    } else if area.width < 60 || area.height < 16 {
        render_compact(frame, state, chunks[1]);
    } else {
        match state.page {
            DashboardPage::Overview => render_overview(frame, state, chunks[1]),
            DashboardPage::CapabilityMap => render_capability_map(frame, state, chunks[1]),
            DashboardPage::Lineage => render_lineage(frame, state, chunks[1]),
            DashboardPage::Evidence => render_evidence(frame, state, chunks[1]),
        }
    }
    frame.render_widget(
        Paragraph::new("Tab/←/→ 切换页面  1-4 跳转  ↑/↓ 证据下钻  q/Esc 退出")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

/// 渲染无颜色环境也可识别的编号页签。
fn render_tabs(frame: &mut Frame, state: &EvolutionDashboardState, area: Rect) {
    let pages = [
        DashboardPage::Overview,
        DashboardPage::CapabilityMap,
        DashboardPage::Lineage,
        DashboardPage::Evidence,
    ];
    let line = Line::from(
        pages
            .iter()
            .enumerate()
            .flat_map(|(index, page)| {
                let style = if *page == state.page {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                [
                    Span::styled(format!(" {} {} ", index + 1, page.title()), style),
                    Span::raw(" "),
                ]
            })
            .collect::<Vec<_>>(),
    );
    frame.render_widget(
        Paragraph::new(line).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Lucia Evolution Dashboard"),
        ),
        area,
    );
}

/// 渲染 Overview 的核心数字、安全与资源区。
fn render_overview(frame: &mut Frame, state: &EvolutionDashboardState, area: Rect) {
    let scorecard = state.scorecard.as_ref().expect("非空状态必须有 Scorecard");
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(area);
    let primary = vec![
        Line::from(verdict_span(scorecard.headline_verdict)),
        Line::from(format!(
            "Net Capability Gain   {}",
            signed(scorecard.capability.net_gain, " points")
        )),
        Line::from(format!(
            "Hidden Gain           {}",
            pp(scorecard.datasets.hidden.delta_pp.map(|delta| delta.0))
        )),
        Line::from(format!(
            "Regression Retention  {}",
            rate(scorecard.datasets.regression.retention.retention)
        )),
        Line::from(format!(
            "Inheritance           {}",
            scorecard
                .inheritance
                .as_ref()
                .map(|metrics| rate(metrics.rate()))
                .unwrap_or_else(|| "N/A".into())
        )),
    ];
    frame.render_widget(
        Paragraph::new(primary)
            .block(Block::default().borders(Borders::ALL).title("Core"))
            .wrap(Wrap { trim: false }),
        columns[0],
    );
    let secondary = vec![
        Line::from(format!("Safety Gate    {}", safety_label(scorecard))),
        Line::from(format!("Confidence     {}", confidence_label(scorecard))),
        Line::from(format!(
            "Token Delta    {}",
            relative(scorecard.resources.tokens.relative.map(|delta| delta.0))
        )),
        Line::from(format!(
            "Cost Delta     {}",
            relative(scorecard.resources.cost.relative.map(|delta| delta.0))
        )),
        Line::from(format!(
            "Latency Delta  {}",
            relative(scorecard.resources.latency_ms.relative.map(|delta| delta.0))
        )),
        Line::from(format!("Gate Decision  {:?}", scorecard.gate.decision)),
    ];
    frame.render_widget(
        Paragraph::new(secondary).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Safety & Resource"),
        ),
        columns[1],
    );
}

/// 渲染 Task Family × Generation 表格，单元格始终显示数值。
fn render_capability_map(frame: &mut Frame, state: &EvolutionDashboardState, area: Rect) {
    let rows = state
        .history
        .as_ref()
        .map(|history| history.capability_map.as_slice())
        .unwrap_or_default();
    if rows.is_empty() {
        render_message(frame, "暂无可验证的 Task Family × Generation 数据", area);
        return;
    }
    let generations: BTreeSet<_> = rows
        .iter()
        .flat_map(|row| row.cells.iter().map(|cell| cell.generation))
        .collect();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(1)])
        .split(area);
    let boundaries = generations
        .iter()
        .map(|generation| {
            let versions: BTreeSet<_> = rows
                .iter()
                .flat_map(|row| row.cells.iter())
                .filter(|cell| cell.generation == *generation)
                .flat_map(|cell| cell.dataset_versions.iter())
                .map(ToString::to_string)
                .collect();
            format!(
                "G{generation}: {}",
                if versions.is_empty() {
                    "N/A".into()
                } else {
                    versions.into_iter().collect::<Vec<_>>().join(", ")
                }
            )
        })
        .collect::<Vec<_>>()
        .join("  ");
    frame.render_widget(
        Paragraph::new(boundaries)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Dataset 版本边界"),
            )
            .wrap(Wrap { trim: false }),
        layout[0],
    );
    let header = Row::new(
        std::iter::once(Cell::from("Task Family"))
            .chain(
                generations
                    .iter()
                    .map(|generation| Cell::from(format!("G{generation}"))),
            )
            .collect::<Vec<_>>(),
    )
    .style(Style::default().add_modifier(Modifier::BOLD));
    let table_rows = rows.iter().map(|row| {
        let cells: BTreeMap<_, _> = row
            .cells
            .iter()
            .map(|cell| (cell.generation, cell.score))
            .collect();
        Row::new(
            std::iter::once(Cell::from(row.task_family.clone()))
                .chain(generations.iter().map(|generation| {
                    Cell::from(
                        cells
                            .get(generation)
                            .copied()
                            .flatten()
                            .map(|score| format!("{:.1}%", score * 100.0))
                            .unwrap_or_else(|| "N/A".into()),
                    )
                }))
                .collect::<Vec<_>>(),
        )
    });
    let widths = std::iter::once(Constraint::Percentage(40))
        .chain((0..generations.len()).map(|_| Constraint::Length(9)))
        .collect::<Vec<_>>();
    frame.render_widget(
        Table::new(table_rows, widths)
            .header(header)
            .column_spacing(1)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Capability Map (数值不依赖颜色)"),
            ),
        layout[1],
    );
}

/// 渲染 Lineage 节点和拒绝、隔离、回滚状态。
fn render_lineage(frame: &mut Frame, state: &EvolutionDashboardState, area: Rect) {
    let nodes = state
        .history
        .as_ref()
        .map(|history| history.lineage_nodes.as_slice())
        .unwrap_or_default();
    if nodes.is_empty() {
        render_message(frame, "暂无可验证的 Lineage 节点", area);
        return;
    }
    let items = nodes.iter().map(|node| {
        ListItem::new(vec![
            Line::from(format!(
                "{} {} ← {}",
                node.generation
                    .map(|generation| format!("G{generation}"))
                    .unwrap_or_else(|| "G?".into()),
                node.revision,
                node.parent
            )),
            Line::from(format!(
                "  Mutation {} | {:?} | Gate {:?} | {:?}",
                mutation_surfaces(&node.mutation_surfaces),
                node.behavior_assessment,
                node.gate_decision,
                node.lifecycle,
            )),
            Line::from(format!(
                "  Capability {} | Hidden {} | Repair {} | Retention {} | Stability {}",
                number(node.capability_score),
                percent(node.hidden_score),
                percent(node.repair_score),
                rate(node.regression_retention),
                percent(node.stability),
            )),
            Line::from(format!(
                "  Token {} | Latency {} ms | Safety {} | Release {} | Rollback {}",
                number(node.average_tokens),
                number(node.average_latency_ms),
                node.safety_failures,
                node.release
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "N/A".into()),
                node.rollback_record
                    .as_ref()
                    .map(|record| format!("{:?}: {}", record.category, record.reason))
                    .unwrap_or_else(|| "N/A".into()),
            ))
            .style(Style::default().fg(Color::DarkGray)),
        ])
    });
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Lineage (Rejected / Quarantined / RolledBack 均保留)"),
        ),
        area,
    );
}

/// 渲染 Evidence 列表与选中条目详情，不暴露 Hidden TaskCase 内容。
fn render_evidence(frame: &mut Frame, state: &mut EvolutionDashboardState, area: Rect) {
    let items = evidence_items(state.scorecard.as_ref(), state.certificate.as_ref());
    if items.is_empty() {
        render_message(frame, "暂无 Evidence", area);
        return;
    }
    state.evidence_index = state.evidence_index.min(items.len() - 1);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
        .split(area);
    let list = items.iter().enumerate().map(|(index, item)| {
        let prefix = if index == state.evidence_index {
            "> "
        } else {
            "  "
        };
        ListItem::new(format!("{prefix}{}", item.0))
    });
    frame.render_widget(
        List::new(list).block(Block::default().borders(Borders::ALL).title("Evidence")),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(items[state.evidence_index].1.clone())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(items[state.evidence_index].0),
            )
            .wrap(Wrap { trim: false }),
        columns[1],
    );
}

/// 小终端降级为当前页关键文本，保留 Verdict 与 Safety。
fn render_compact(frame: &mut Frame, state: &EvolutionDashboardState, area: Rect) {
    let scorecard = state.scorecard.as_ref().expect("非空状态必须有 Scorecard");
    let text = format!(
        "{}\nVerdict: {}\nSafety: {}\nCapability: {} -> {}\nHidden: {}\nRetention: {}\nInheritance: {}\n\n终端过小，已显示紧凑视图。",
        state.page.title(),
        scorecard.headline_verdict.label(),
        safety_label(scorecard),
        number(scorecard.capability.parent_score),
        number(scorecard.capability.candidate_score),
        pp(scorecard.datasets.hidden.delta_pp.map(|delta| delta.0)),
        rate(scorecard.datasets.regression.retention.retention),
        scorecard
            .inheritance
            .as_ref()
            .map(|metrics| rate(metrics.rate()))
            .unwrap_or_else(|| "N/A".into()),
    );
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// 渲染无数据状态。
fn render_empty(frame: &mut Frame, area: Rect) {
    render_message(
        frame,
        "NO EVOLUTION DATA\n暂无已发布 Evolution 数据\n\n先生成可信 EvaluationReport、通过 Commit Gate 并完成 Promotion。",
        area,
    );
}

/// 渲染错误状态，避免损坏数据被静默当作空数据。
fn render_error(frame: &mut Frame, error: &str, area: Rect) {
    frame.render_widget(
        Paragraph::new(format!("Evolution Dashboard 加载失败\n\n{error}"))
            .style(Style::default().fg(Color::Red))
            .block(Block::default().borders(Borders::ALL).title("ERROR"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// 渲染普通居中语义消息。
fn render_message(frame: &mut Frame, message: &str, area: Rect) {
    frame.render_widget(
        Paragraph::new(message)
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// 构建不含 Hidden 正文、Secret 或未脱敏 ToolResult 的 Evidence 项。
fn evidence_items(
    scorecard: Option<&EvolutionScorecard>,
    certificate: Option<&EvolutionCertificate>,
) -> Vec<(&'static str, String)> {
    let Some(scorecard) = scorecard else {
        return Vec::new();
    };
    let items = vec![
        (
            "Source Episodes",
            certificate
                .map(|certificate| {
                    certificate
                        .source_episode_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| "N/A".into()),
        ),
        (
            "Evolution Issue",
            certificate
                .map(|certificate| format!("ID: {}", certificate.evolution_issue_id))
                .unwrap_or_else(|| "N/A".into()),
        ),
        (
            "Mutation Proposal",
            certificate
                .map(|certificate| {
                    format!(
                        "ID: {}\nChanged surfaces: {}",
                        certificate.mutation_id,
                        if certificate.allowed_diff.changed_surfaces.is_empty() {
                            "none".into()
                        } else {
                            certificate
                                .allowed_diff
                                .changed_surfaces
                                .iter()
                                .map(|surface| format!("{surface:?}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    )
                })
                .unwrap_or_else(|| "N/A".into()),
        ),
        (
            "Candidate Artifacts",
            certificate
                .map(|certificate| {
                    if certificate.candidate_artifacts.is_empty() {
                        "N/A".into()
                    } else {
                        certificate
                            .candidate_artifacts
                            .iter()
                            .map(|artifact| {
                                format!(
                                    "{}  {} bytes\n{}",
                                    artifact.media_type, artifact.size_bytes, artifact.digest
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    }
                })
                .unwrap_or_else(|| "N/A".into()),
        ),
        (
            "Evaluation Report",
            format!(
                "ID: {}\nSource digest: {}\nMetrics policy: {}\nVerdict policy: {}",
                scorecard.evaluation_report,
                scorecard.source_report_digest,
                scorecard.metrics_policy_version,
                scorecard.verdict_policy_version
            ),
        ),
        (
            "Genome Diff / Comparison",
            if !scorecard.comparison_validity.valid {
                format!(
                    "Comparable: NO\n{}",
                    scorecard
                        .comparison_validity
                        .violations
                        .iter()
                        .map(|violation| format!("{:?}: {}", violation.kind, violation.detail))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            } else if let Some(certificate) = certificate {
                format!(
                    "Comparable: YES\nChanged surfaces: {}\nDiff artifact: {}",
                    if certificate.allowed_diff.changed_surfaces.is_empty() {
                        "none".into()
                    } else {
                        certificate
                            .allowed_diff
                            .changed_surfaces
                            .iter()
                            .map(|surface| format!("{surface:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                    certificate
                        .allowed_diff
                        .artifact
                        .as_ref()
                        .map(|artifact| artifact.digest.to_string())
                        .unwrap_or_else(|| "N/A".into())
                )
            } else {
                "Comparable: YES\nEvolution Certificate: N/A".into()
            },
        ),
        (
            "Safety Failures",
            format!(
                "Critical: {}\nPermission expansions: {}\nHidden access attempts: {}\nSecret access attempts: {}\nArtifact integrity failures: {}\nAudit integrity failures: {}\nMissing safety attempts: {}",
                scorecard.safety.candidate.critical_failures,
                scorecard.safety.candidate.permission_expansions,
                scorecard.safety.candidate.hidden_dataset_access_attempts,
                scorecard.safety.candidate.secret_access_attempts,
                scorecard.safety.candidate.artifact_integrity_failures,
                scorecard.safety.candidate.audit_integrity_failures,
                scorecard.safety.candidate.missing_attempts,
            ),
        ),
        (
            "Gate / Release",
            format!(
                "Gate: {:?}\nHard failures: {}\nRelease: {}\nLifecycle: {:?}",
                scorecard.gate.decision,
                if scorecard.gate.hard_failures.is_empty() {
                    "none".into()
                } else {
                    scorecard.gate.hard_failures.join(", ")
                },
                scorecard
                    .release_record
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "N/A".into()),
                scorecard.lifecycle,
            ),
        ),
        (
            "Inheritance Verification",
            scorecard
                .inheritance
                .as_ref()
                .map(|metrics| {
                    format!(
                        "Expected: {}\nObserved after restart: {}\nRestart: {}\nNew session: {}\nOld session parent preserved: {}\nStable ref: {}\nDigest: {}\nVerified: {}",
                        metrics.expected_genome,
                        metrics
                            .observed_genome_after_restart
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| "N/A".into()),
                        rate(metrics.restart),
                        rate(metrics.new_session),
                        optional_bool(metrics.old_session_parent_preserved),
                        yes_no(metrics.stable_reference_verified),
                        yes_no(metrics.genome_digest_verified),
                        yes_no(metrics.verified),
                    )
                })
                .unwrap_or_else(|| "N/A".into()),
        ),
        (
            "Rollback Record",
            certificate
                .and_then(|certificate| certificate.rollback_record.as_ref())
                .map(|record| {
                    format!(
                        "Release: {}\nCategory: {:?}\nReason: {}\nCreated: {}\nEvidence: {}",
                        record.release_record,
                        record.category,
                        record.reason,
                        record.created_at_ms,
                        if record.evidence.is_empty() {
                            "N/A".into()
                        } else {
                            record
                                .evidence
                                .iter()
                                .map(|artifact| artifact.digest.to_string())
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                    )
                })
                .unwrap_or_else(|| "N/A".into()),
        ),
        (
            "Evolution Certificate",
            certificate
                .map(|certificate| {
                    format!(
                        "Schema: {}\nRevision: r{}\nPrevious: {}\nDigest: {}\nLifecycle: {:?}\nRepaired cases: {}\nPost-promotion runs: {}",
                        certificate.schema_version,
                        certificate.revision,
                        certificate
                            .previous_certificate_digest
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| "N/A".into()),
                        certificate.certificate_digest,
                        certificate.lifecycle,
                        certificate.repaired_task_case_ids.len(),
                        certificate.post_promotion_run_ids.len(),
                    )
                })
                .unwrap_or_else(|| "N/A".into()),
        ),
    ];
    items
}

/// Verdict 标签样式；标签文本本身始终存在。
fn verdict_span(verdict: HeadlineVerdict) -> Span<'static> {
    let color = match verdict {
        HeadlineVerdict::Evolved | HeadlineVerdict::Eligible => Color::Green,
        HeadlineVerdict::Patched | HeadlineVerdict::NoChange => Color::Yellow,
        HeadlineVerdict::Unsafe
        | HeadlineVerdict::Regressed
        | HeadlineVerdict::RolledBack
        | HeadlineVerdict::InvalidComparison => Color::Red,
        HeadlineVerdict::Inconclusive => Color::DarkGray,
    };
    Span::styled(
        format!("Headline Verdict      {}", verdict.label()),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

/// 返回安全 Gate 的显式文本标签。
fn safety_label(scorecard: &EvolutionScorecard) -> &'static str {
    if scorecard.safety.candidate.hard_gate_failed()
        || scorecard.gate.artifact_integrity_verified == Some(false)
        || scorecard.gate.audit_integrity_verified == Some(false)
        || scorecard.gate.hidden_dataset_isolated == Some(false)
    {
        "FAIL"
    } else if !scorecard.safety.candidate.is_complete()
        || scorecard.gate.artifact_integrity_verified != Some(true)
        || scorecard.gate.audit_integrity_verified != Some(true)
        || scorecard.gate.hidden_dataset_isolated != Some(true)
    {
        "UNKNOWN"
    } else {
        "PASS"
    }
}

/// 以文本列出全部变异表面，避免空集合或颜色造成歧义。
fn mutation_surfaces(surfaces: &BTreeSet<agent_evolution_protocol::MutationSurface>) -> String {
    if surfaces.is_empty() {
        return "none".into();
    }
    surfaces
        .iter()
        .map(|surface| format!("{surface:?}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// 返回置信度类型，不把 Deterministic 伪装为百分比。
fn confidence_label(scorecard: &EvolutionScorecard) -> &'static str {
    match scorecard.confidence {
        agent_evolution::EvaluationConfidence::Deterministic => "Deterministic",
        agent_evolution::EvaluationConfidence::PairedBootstrap { .. } => "Paired Bootstrap",
        agent_evolution::EvaluationConfidence::Insufficient { .. } => "Insufficient",
    }
}

/// 格式化可选分数。
fn number(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.1}"))
        .unwrap_or_else(|| "N/A".into())
}

/// 格式化可选有符号分数。
fn signed(value: Option<f64>, suffix: &str) -> String {
    value
        .map(|value| format!("{value:+.1}{suffix}"))
        .unwrap_or_else(|| "N/A".into())
}

/// 格式化 `[0,1]` 百分比。
fn percent(value: Option<f64>) -> String {
    value
        .map(|value| format!("{:.1}%", value * 100.0))
        .unwrap_or_else(|| "N/A".into())
}

/// 格式化百分点变化。
fn pp(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:+.1}pp"))
        .unwrap_or_else(|| "N/A".into())
}

/// 格式化相对百分比变化。
fn relative(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:+.1}%"))
        .unwrap_or_else(|| "N/A".into())
}

/// 格式化保留计数的 Rate。
fn rate(value: agent_evolution::Rate) -> String {
    value
        .percent()
        .map(|percent| format!("{percent:.1}% ({}/{})", value.numerator, value.denominator))
        .unwrap_or_else(|| "N/A".into())
}

/// 格式化布尔值且不依赖颜色。
const fn yes_no(value: bool) -> &'static str {
    if value {
        "YES"
    } else {
        "NO"
    }
}

/// 区分未知布尔值与 false。
const fn optional_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "YES",
        Some(false) => "NO",
        None => "N/A",
    }
}

use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    /// 把状态渲染到 TestBackend 并返回纯文本缓冲区。
    fn rendered(mut state: EvolutionDashboardState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("测试终端应创建");
        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("Dashboard 应渲染");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn dashboard_has_clear_empty_and_error_states() {
        let empty = EvolutionDashboardState {
            scorecard: None,
            certificate: None,
            history: None,
            error: None,
            page: DashboardPage::Overview,
            evidence_index: 0,
            should_quit: false,
        };
        assert!(rendered(empty, 80, 20).contains("NO EVOLUTION DATA"));
        assert!(rendered(EvolutionDashboardState::failed("损坏报告"), 80, 20).contains("ERROR"));
    }

    #[test]
    fn keyboard_navigation_reaches_all_four_pages() {
        let mut state = EvolutionDashboardState {
            scorecard: None,
            certificate: None,
            history: None,
            error: None,
            page: DashboardPage::Overview,
            evidence_index: 0,
            should_quit: false,
        };
        state.handle_key(KeyCode::Char('2'));
        assert_eq!(state.page, DashboardPage::CapabilityMap);
        state.handle_key(KeyCode::Char('3'));
        assert_eq!(state.page, DashboardPage::Lineage);
        state.handle_key(KeyCode::Char('4'));
        assert_eq!(state.page, DashboardPage::Evidence);
        state.handle_key(KeyCode::Char('1'));
        assert_eq!(state.page, DashboardPage::Overview);
    }
}
