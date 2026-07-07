//! 长链路任务的分段返回排序（R13.1 / R13.4）。
//!
//! 含「数据文本结果」与「附件（截图/文件）」两类产出的任务，应先返回文本
//! 结果，再返回附件产出（R13.1）。本模块提供一个**纯函数** `order_segments`，
//! 对任意产出分段序列进行稳定分区：所有 `Text` 段排在所有 `Attachment` 段
//! 之前，且各组内部保持原有相对顺序（stable partition）。该函数无副作用、
//! 不读取时钟或全局状态，便于属性测试确定性验证（Property 33）。
//!
//! 另外提供 `take_completed` 辅助：当某子步骤超过其步骤超时被终止时，调用方
//! 可据此截取已完成的分段前缀并返回（R13.4 的纯逻辑部分；具体超时计时由集成层负责）。

/// 单个产出附件（截图 / 文件等）。
///
/// 这是一个轻量描述，承载附件的标识信息；真正的字节内容由发送层按 `name`/`ref`
/// 解析。`kind` 用于区分截图、文件等类别（可选语义，默认 `File`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// 附件名称 / 标识（如文件名或截图标题）。
    pub name: String,
    /// 附件引用（如沙盒内路径或 pending file id）。
    pub reference: String,
    /// 附件类别。
    pub kind: AttachmentKind,
}

impl Attachment {
    /// 构造一个文件类附件。
    pub fn file(name: impl Into<String>, reference: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            reference: reference.into(),
            kind: AttachmentKind::File,
        }
    }

    /// 构造一个截图类附件。
    pub fn screenshot(name: impl Into<String>, reference: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            reference: reference.into(),
            kind: AttachmentKind::Screenshot,
        }
    }
}

/// 附件类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    /// 普通文件（如 Excel、CSV）。
    File,
    /// 截图。
    Screenshot,
}

/// 任务的单个产出分段。
///
/// 一个长链路任务可能产出若干文本片段与若干附件，按产生顺序排列。
/// `order_segments` 负责把它们重排为「文本先于附件」的发送顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultSegment {
    /// 文本结果。
    Text(String),
    /// 附件产出（截图 / 文件）。
    Attachment(Attachment),
}

impl ResultSegment {
    /// 是否为文本分段。
    pub fn is_text(&self) -> bool {
        matches!(self, ResultSegment::Text(_))
    }

    /// 是否为附件分段。
    pub fn is_attachment(&self) -> bool {
        matches!(self, ResultSegment::Attachment(_))
    }
}

/// 对产出分段做稳定分区：所有 `Text` 段排在所有 `Attachment` 段之前，
/// 且两组内部各自保持输入中的相对顺序（stable partition）。
///
/// 该函数是 R13.1「先返回文本结果，再返回附件产出」的纯逻辑实现：
/// - 不丢弃任何分段（输出是输入的一个排列）；
/// - 文本组与附件组各自内部顺序与输入一致；
/// - 对仅含文本或仅含附件的输入是恒等的稳定排序。
pub fn order_segments(segments: Vec<ResultSegment>) -> Vec<ResultSegment> {
    let mut texts = Vec::new();
    let mut attachments = Vec::new();

    for seg in segments {
        match seg {
            ResultSegment::Text(_) => texts.push(seg),
            ResultSegment::Attachment(_) => attachments.push(seg),
        }
    }

    texts.extend(attachments);
    texts
}

/// 截取已完成的分段前缀（R13.4）。
///
/// 当某子步骤超过其步骤超时被终止时，调用方传入「已完成分段数」
/// `completed`，返回输入中前 `completed` 个分段（已完成部分）。
/// `completed` 超过分段总数时返回全部分段。
///
/// 注意：本函数仅做纯粹的前缀截取，不涉及计时；是否超时由集成层判断。
pub fn take_completed(segments: Vec<ResultSegment>, completed: usize) -> Vec<ResultSegment> {
    let mut segments = segments;
    let keep = completed.min(segments.len());
    segments.truncate(keep);
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sample_segments() -> Vec<ResultSegment> {
        vec![
            ResultSegment::Attachment(Attachment::screenshot("shot1", "/tmp/shot1.png")),
            ResultSegment::Text("持仓概览".to_string()),
            ResultSegment::Attachment(Attachment::file("holdings.xlsx", "/tmp/holdings.xlsx")),
            ResultSegment::Text("汇总完成".to_string()),
        ]
    }

    #[test]
    fn order_segments_puts_all_text_before_attachments() {
        let ordered = order_segments(sample_segments());
        // 找到第一个附件的位置，其后不应再出现文本。
        let first_attachment = ordered.iter().position(|s| s.is_attachment());
        if let Some(idx) = first_attachment {
            assert!(
                ordered[idx..].iter().all(|s| s.is_attachment()),
                "附件之后不应再出现文本分段"
            );
        }
    }

    #[test]
    fn order_segments_preserves_relative_order_within_groups() {
        let ordered = order_segments(sample_segments());
        let texts: Vec<_> = ordered
            .iter()
            .filter_map(|s| match s {
                ResultSegment::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["持仓概览".to_string(), "汇总完成".to_string()]);

        let attachment_names: Vec<_> = ordered
            .iter()
            .filter_map(|s| match s {
                ResultSegment::Attachment(a) => Some(a.name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            attachment_names,
            vec!["shot1".to_string(), "holdings.xlsx".to_string()]
        );
    }

    #[test]
    fn order_segments_is_identity_for_text_only() {
        let input = vec![
            ResultSegment::Text("a".to_string()),
            ResultSegment::Text("b".to_string()),
        ];
        assert_eq!(order_segments(input.clone()), input);
    }

    #[test]
    fn order_segments_is_identity_for_attachment_only() {
        let input = vec![
            ResultSegment::Attachment(Attachment::file("f1", "/r1")),
            ResultSegment::Attachment(Attachment::file("f2", "/r2")),
        ];
        assert_eq!(order_segments(input.clone()), input);
    }

    #[test]
    fn order_segments_handles_empty() {
        assert_eq!(order_segments(Vec::new()), Vec::<ResultSegment>::new());
    }

    #[test]
    fn take_completed_returns_prefix() {
        let segs = sample_segments();
        let kept = take_completed(segs.clone(), 2);
        assert_eq!(kept, segs[..2].to_vec());
    }

    #[test]
    fn take_completed_clamps_to_len() {
        let segs = sample_segments();
        let kept = take_completed(segs.clone(), 99);
        assert_eq!(kept, segs);
    }

    #[test]
    fn take_completed_zero_returns_empty() {
        let kept = take_completed(sample_segments(), 0);
        assert!(kept.is_empty());
    }

    // 集成测试（R13.4）：子步骤超时终止并返回已完成部分。
    // 场景：一个长链路任务按顺序产出多个分段（文本 + 附件步骤）。管线在
    // 完成前 K 个子步骤后，第 K+1 个子步骤超过其步骤超时被终止；此时系统
    // 应返回已完成的前 K 个分段，且超时点之后的分段不应包含在结果中。
    //
    // 这里以确定性方式模拟：集成层判定「已完成子步骤数 = completed」后调用
    // take_completed 截取已完成前缀（segment.rs 提供的纯前缀截断原语）。
    // Validates: Requirements 13.4
    #[test]
    fn substep_timeout_returns_completed_prefix() {
        // 长链路任务的全部分段（含若干文本步骤与附件步骤）。
        let all_segments = vec![
            ResultSegment::Text("步骤1：拉取昨日持仓".to_string()),
            ResultSegment::Attachment(Attachment::file("holdings.xlsx", "/tmp/holdings.xlsx")),
            ResultSegment::Text("步骤2：计算收益".to_string()),
            // —— 第 4 个子步骤（截图）在执行中超时被终止 ——
            ResultSegment::Attachment(Attachment::screenshot("chart.png", "/tmp/chart.png")),
            ResultSegment::Text("步骤3：生成结论".to_string()),
        ];

        // 集成层判定：前 3 个子步骤已完成，第 4 个子步骤超过步骤超时被终止。
        let completed_before_timeout = 3usize;

        let returned = take_completed(all_segments.clone(), completed_before_timeout);

        // 1) 返回的前缀恰为已完成部分（前 3 个分段）。
        assert_eq!(returned, all_segments[..completed_before_timeout].to_vec());

        // 2) 超时点及其之后的分段（chart.png 截图、步骤3 结论）不应出现在结果中。
        let timed_out_attachment =
            ResultSegment::Attachment(Attachment::screenshot("chart.png", "/tmp/chart.png"));
        let after_timeout_text = ResultSegment::Text("步骤3：生成结论".to_string());
        assert!(
            !returned.contains(&timed_out_attachment),
            "超时被终止的子步骤产出不应包含在已完成结果中"
        );
        assert!(
            !returned.contains(&after_timeout_text),
            "超时点之后的子步骤产出不应包含在已完成结果中"
        );

        // 3) 已完成部分内容完整、顺序保持。
        assert_eq!(returned.len(), completed_before_timeout);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // order_segments 保持稳定分区：输出是输入的排列，且文本组/附件组内部顺序不变。
        #[test]
        fn prop_order_segments_stable_partition(
            flags in proptest::collection::vec(any::<bool>(), 0..32),
        ) {
            // 用 flags 生成带唯一序号的分段：true=Text，false=Attachment。
            let input: Vec<ResultSegment> = flags
                .iter()
                .enumerate()
                .map(|(i, &is_text)| {
                    if is_text {
                        ResultSegment::Text(format!("t{i}"))
                    } else {
                        ResultSegment::Attachment(Attachment::file(format!("a{i}"), format!("/r{i}")))
                    }
                })
                .collect();

            let ordered = order_segments(input.clone());

            // 1) 数量守恒。
            prop_assert_eq!(ordered.len(), input.len());

            // 2) 文本全部排在附件之前。
            let first_attachment = ordered.iter().position(|s| s.is_attachment());
            if let Some(idx) = first_attachment {
                prop_assert!(ordered[..idx].iter().all(|s| s.is_text()));
                prop_assert!(ordered[idx..].iter().all(|s| s.is_attachment()));
            }

            // 3) 各组内部相对顺序保持不变。
            let in_texts: Vec<_> = input.iter().filter(|s| s.is_text()).cloned().collect();
            let out_texts: Vec<_> = ordered.iter().filter(|s| s.is_text()).cloned().collect();
            prop_assert_eq!(in_texts, out_texts);

            let in_atts: Vec<_> = input.iter().filter(|s| s.is_attachment()).cloned().collect();
            let out_atts: Vec<_> = ordered.iter().filter(|s| s.is_attachment()).cloned().collect();
            prop_assert_eq!(in_atts, out_atts);
        }

        // Feature: execution-performance-optimization, Property 33: 文本结果先于附件返回
        // Validates: Requirements 13.1
        // 对任意同时含文本结果与附件产出的任务分段，order_segments 的输出中
        // 每个 Text 都排在每个 Attachment 之前（文本结果先于附件返回）。
        #[test]
        fn prop_text_before_attachments(
            texts in proptest::collection::vec("[a-z]{1,8}", 1..16),
            attachments in proptest::collection::vec("[a-z]{1,8}", 1..16),
            interleave_seed in proptest::collection::vec(any::<bool>(), 0..32),
        ) {
            // 交织文本与附件，保证输入同时包含两类分段。
            let mut input: Vec<ResultSegment> = Vec::new();
            let mut ti = 0usize;
            let mut ai = 0usize;
            for &take_text in &interleave_seed {
                if take_text && ti < texts.len() {
                    input.push(ResultSegment::Text(texts[ti].clone()));
                    ti += 1;
                } else if ai < attachments.len() {
                    input.push(ResultSegment::Attachment(Attachment::file(
                        attachments[ai].clone(),
                        format!("/r{ai}"),
                    )));
                    ai += 1;
                }
            }
            // 追加剩余分段，确保两类都至少出现一次。
            while ti < texts.len() {
                input.push(ResultSegment::Text(texts[ti].clone()));
                ti += 1;
            }
            while ai < attachments.len() {
                input.push(ResultSegment::Attachment(Attachment::file(
                    attachments[ai].clone(),
                    format!("/r{ai}"),
                )));
                ai += 1;
            }

            prop_assume!(input.iter().any(|s| s.is_text()));
            prop_assume!(input.iter().any(|s| s.is_attachment()));

            let ordered = order_segments(input);

            // 每个 Text 的下标都小于每个 Attachment 的下标。
            let last_text = ordered.iter().rposition(|s| s.is_text());
            let first_attachment = ordered.iter().position(|s| s.is_attachment());
            if let (Some(last_text), Some(first_attachment)) = (last_text, first_attachment) {
                prop_assert!(
                    last_text < first_attachment,
                    "存在 Text 出现在 Attachment 之后，违反文本先于附件"
                );
            }
        }
    }
}
