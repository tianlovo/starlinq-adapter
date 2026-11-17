//! 富媒体与原生桌面交互发送计划构造器：严格区分纯文本图文、原生桌面分步输入与严格不支持拦截

use std::path::PathBuf;

use common_types::constants::IMAGE_PLACEHOLDER_TOKEN;
use common_types::{
    face_id_to_name, parse_cq_string_to_segments, segments_to_plain_summary, AdapterError,
    ChatTargetKind, GroupMemberInfo, MessageSegment, Result,
};
use storage::Storage;
use uia_core::{decode_image_bytes_to_bgra, DecodedImageBgra};

use crate::composer::file_staging;
use crate::constants::{AT_ALL_DISPLAY_NAME, ONEBOT_AT_ALL_QQ};
use crate::driver::outbound_media;
use crate::parser;

/// 需要按顺序在聊天窗口及输入栏中以真实 QQNT 桌面交互逐项插入的内联步骤
pub(crate) enum InlineStep {
    /// 右键点击历史消息气泡并点击“回复”菜单项挂载原生引用卡片（固定前置到第 0 步）
    AttachReply {
        sender_name: String,
        excerpt: String,
        raw_text: String,
    },
    /// 点击“表情”工具栏按钮并在 QQNT 表情弹窗中真实点选对应经典表情
    InsertFace { face_name: String },
    /// 在编辑框键入 `@` 触发成员选择器弹窗并真实点选群成员或全体成员
    InsertAt { display_name: String, is_all: bool },
    /// 在当前活跃编辑框尾部输入原始正文文本
    TypeText { text: String },
    /// 通过剪贴板快照写入 `CF_DIB` 并执行 `Ctrl+V` 粘贴真实图片（不立即发送）
    PasteImage { bitmap: DecodedImageBgra },
}

/// 独占整条消息生命周期的独立原生桌面交互动作（不与文本或内联段混发）
pub(crate) enum StandaloneAction {
    /// 通过剪贴板 `CF_HDROP` 向聊天编辑框粘贴本地文件并点击确认发送真实文件气泡
    File { path: PathBuf },
    /// 通过剪贴板 `CF_HDROP` 向聊天编辑框粘贴本地或暂存视频文件并点击确认发送真实视频气泡
    Video { path: PathBuf },
    /// 点击表情/魔法面板中的真实“猜拳”入口（若客户端未提供则严格返回不支持）
    Rps,
    /// 点击表情/魔法面板中的真实“掷骰子”入口（若客户端未提供则严格返回不支持）
    Dice,
    /// 私聊窗口抖动原生工具栏入口（若当前桌面客户端无该控件则严格返回不支持）
    PrivateShake,
}

/// 根据 `[onebot].allow_degraded_media_summary` 与请求段列表推导出的发送路由决策
pub(crate) enum ComposerExecutionPlan {
    /// 仅含纯文本与常规图片：交由既有无焦点/图文混合发送管线处理
    PurePlainTextOrImage,
    /// 已显式启用降级文本摘要模式且包含复杂段：保留既有降级管线供兼容测试使用
    DegradedLegacy,
    /// 通过真实鼠标点击、弹窗点选与快捷键组合完成的原生富消息内联发送计划
    RichInline {
        steps: Vec<InlineStep>,
        canonical_segments: Vec<MessageSegment>,
        summary_text: String,
        verify_tokens: Vec<String>,
        has_image: bool,
    },
    /// 通过独占原生桌面交互动作投递文件、魔法表情或窗口抖动
    Standalone {
        action: StandaloneAction,
        canonical_segments: Vec<MessageSegment>,
        summary_text: String,
        verify_tokens: Vec<String>,
    },
}

fn is_flash_image(image_type: Option<&str>) -> bool {
    image_type
        .map(str::trim)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("flash"))
}

fn action_name_for_target(target_kind: ChatTargetKind) -> &'static str {
    match target_kind {
        ChatTargetKind::Group => "send_group_msg",
        ChatTargetKind::Private => "send_private_msg",
    }
}

fn unsupported(target_kind: ChatTargetKind, reason: impl Into<String>) -> AdapterError {
    AdapterError::UnsupportedAction {
        action: action_name_for_target(target_kind).to_string(),
        reason: reason.into(),
    }
}

fn is_standalone_segment(seg: &MessageSegment) -> bool {
    matches!(
        seg,
        MessageSegment::File { .. }
            | MessageSegment::Video { .. }
            | MessageSegment::Rps {}
            | MessageSegment::Dice {}
            | MessageSegment::Shake {}
    )
}

fn resolve_staged_video(
    file: &str,
    url: Option<&str>,
    target_kind: ChatTargetKind,
) -> Result<file_staging::StagedFile> {
    let action_name = action_name_for_target(target_kind);
    let file_trimmed = file.trim();
    let url_trimmed = url.map(str::trim).unwrap_or("");
    if !file_trimmed.is_empty() {
        match file_staging::stage_outbound_file(
            file_trimmed,
            None,
            target_kind,
            action_name,
            "mp4",
            None,
        ) {
            Ok(staged) => return Ok(staged),
            Err(err) => {
                if !url_trimmed.is_empty() && file_staging::is_http_or_https_url(url_trimmed) {
                    return file_staging::stage_outbound_file(
                        url_trimmed,
                        None,
                        target_kind,
                        action_name,
                        "mp4",
                        None,
                    );
                }
                return Err(err);
            }
        }
    }
    file_staging::stage_outbound_file(url_trimmed, None, target_kind, action_name, "mp4", None)
}

fn validate_strict_segments(
    target_kind: ChatTargetKind,
    segments: &[MessageSegment],
) -> Result<()> {
    let has_standalone = segments.iter().any(is_standalone_segment);
    if has_standalone && segments.len() > 1 {
        return Err(unsupported(
            target_kind,
            "standalone native actions (file/video/rps/dice/shake) cannot be mixed with other segments in strict mode",
        ));
    }

    let mut reply_count = 0usize;
    for seg in segments {
        match seg {
            MessageSegment::Text { .. } => {}
            MessageSegment::Image { image_type, .. } => {
                if is_flash_image(image_type.as_deref()) {
                    return Err(unsupported(
                        target_kind,
                        "QQNT 9.9.31 desktop UIA does not support sending flash images",
                    ));
                }
            }
            MessageSegment::Face { id, .. } => {
                if face_id_to_name(id).is_none() {
                    return Err(unsupported(
                        target_kind,
                        format!(
                            "unknown classic QQ face id '{id}'; cannot convert unsupported face to plain text"
                        ),
                    ));
                }
            }
            MessageSegment::At { .. } if target_kind == ChatTargetKind::Private => {
                return Err(unsupported(
                    target_kind,
                    "private chat does not support native @mention",
                ));
            }
            MessageSegment::At { .. } => {}
            MessageSegment::Reply { .. } => {
                reply_count += 1;
                if reply_count > 1 {
                    return Err(AdapterError::InputRejected(
                        "at most one reply segment is supported per native message".to_string(),
                    ));
                }
            }
            MessageSegment::File { .. }
            | MessageSegment::Video { .. }
            | MessageSegment::Rps {}
            | MessageSegment::Dice {} => {}
            MessageSegment::Poke { .. } => {
                return Err(unsupported(
                    target_kind,
                    "QQNT 9.9.31 desktop UIA does not support native outbound poke",
                ));
            }
            MessageSegment::Shake {} => {
                if target_kind == ChatTargetKind::Group {
                    return Err(unsupported(
                        target_kind,
                        "group chat does not support private window shake",
                    ));
                }
            }
            MessageSegment::Record { .. }
            | MessageSegment::Share { .. }
            | MessageSegment::Contact { .. }
            | MessageSegment::Location { .. }
            | MessageSegment::Music { .. }
            | MessageSegment::Xml { .. }
            | MessageSegment::Json { .. }
            | MessageSegment::Anonymous { .. }
            | MessageSegment::Forward { .. }
            | MessageSegment::Node { .. } => {
                return Err(unsupported(
                    target_kind,
                    "desktop UIA cannot natively construct record/card/forward/anonymous segments in strict mode",
                ));
            }
        }
    }
    Ok(())
}

/// 将标准化后的 OneBot 消息段编译为真实桌面执行计划，禁止任何静默伪造纯文本行为
pub(crate) fn build_composer_plan(
    storage: &Storage,
    members: &[GroupMemberInfo],
    target_kind: ChatTargetKind,
    target_session: (&str, ChatTargetKind),
    segments: &[MessageSegment],
    allow_degraded: bool,
) -> Result<ComposerExecutionPlan> {
    let is_pure = segments.iter().all(|seg| {
        matches!(seg, MessageSegment::Text { .. })
            || matches!(seg, MessageSegment::Image { image_type, .. } if !is_flash_image(image_type.as_deref()))
    });
    if is_pure {
        return Ok(ComposerExecutionPlan::PurePlainTextOrImage);
    }
    if allow_degraded {
        return Ok(ComposerExecutionPlan::DegradedLegacy);
    }

    validate_strict_segments(target_kind, segments)?;

    if segments.len() == 1 && is_standalone_segment(&segments[0]) {
        return build_standalone_plan(target_kind, &segments[0]);
    }

    build_rich_inline_plan(storage, members, target_session, segments)
}

fn build_standalone_plan(
    target_kind: ChatTargetKind,
    seg: &MessageSegment,
) -> Result<ComposerExecutionPlan> {
    match seg {
        MessageSegment::File {
            file,
            name,
            size: _,
        } => {
            let staged = file_staging::stage_outbound_file(
                file,
                name.as_deref(),
                target_kind,
                action_name_for_target(target_kind),
                "bin",
                None,
            )?;
            let display_name = staged.display_name;
            let canonical = MessageSegment::File {
                file: staged.path.display().to_string(),
                name: Some(display_name.clone()),
                size: Some(staged.size),
            };
            let canonical_segments = vec![canonical];
            let summary_text = segments_to_plain_summary(&canonical_segments);
            Ok(ComposerExecutionPlan::Standalone {
                action: StandaloneAction::File { path: staged.path },
                canonical_segments,
                summary_text,
                verify_tokens: vec![display_name],
            })
        }
        MessageSegment::Video { file, url } => {
            let staged = resolve_staged_video(file, url.as_deref(), target_kind)?;
            let display_name = staged.display_name;
            let canonical = MessageSegment::Video {
                file: staged.path.display().to_string(),
                url: None,
            };
            let canonical_segments = vec![canonical];
            let summary_text = segments_to_plain_summary(&canonical_segments);
            Ok(ComposerExecutionPlan::Standalone {
                action: StandaloneAction::Video { path: staged.path },
                canonical_segments,
                summary_text,
                verify_tokens: vec![display_name],
            })
        }
        MessageSegment::Rps {} => {
            let canonical_segments = vec![MessageSegment::Rps {}];
            Ok(ComposerExecutionPlan::Standalone {
                action: StandaloneAction::Rps,
                summary_text: segments_to_plain_summary(&canonical_segments),
                canonical_segments,
                verify_tokens: vec!["猜拳".to_string()],
            })
        }
        MessageSegment::Dice {} => {
            let canonical_segments = vec![MessageSegment::Dice {}];
            Ok(ComposerExecutionPlan::Standalone {
                action: StandaloneAction::Dice,
                summary_text: segments_to_plain_summary(&canonical_segments),
                canonical_segments,
                verify_tokens: vec!["骰子".to_string()],
            })
        }
        MessageSegment::Shake {} => {
            let canonical_segments = vec![MessageSegment::Shake {}];
            Ok(ComposerExecutionPlan::Standalone {
                action: StandaloneAction::PrivateShake,
                summary_text: segments_to_plain_summary(&canonical_segments),
                canonical_segments,
                verify_tokens: vec!["抖动".to_string()],
            })
        }
        _ => Err(AdapterError::InputRejected(
            "unexpected segment in standalone builder".to_string(),
        )),
    }
}

fn build_rich_inline_plan(
    storage: &Storage,
    members: &[GroupMemberInfo],
    target_session: (&str, ChatTargetKind),
    segments: &[MessageSegment],
) -> Result<ComposerExecutionPlan> {
    let mut reply_step: Option<InlineStep> = None;
    let mut body_steps: Vec<InlineStep> = Vec::with_capacity(segments.len());
    let mut canonical_segments: Vec<MessageSegment> = Vec::with_capacity(segments.len());
    let mut verify_tokens: Vec<String> = Vec::new();
    let mut has_image = false;

    for seg in segments {
        match seg {
            MessageSegment::Reply { id } => {
                let (parsed_id, record) =
                    outbound_media::load_verified_reply_record(storage, id, target_session)?;
                let target_segments = if !record.segments.is_empty() {
                    record.segments.clone()
                } else {
                    parse_cq_string_to_segments(&record.raw_message, true)
                };
                let plain = segments_to_plain_summary(&target_segments);
                let excerpt = outbound_media::compact_reply_excerpt(&plain, 32);
                reply_step = Some(InlineStep::AttachReply {
                    sender_name: record.sender_name.trim().to_string(),
                    excerpt,
                    raw_text: plain,
                });
                canonical_segments.push(MessageSegment::Reply {
                    id: parsed_id.to_string(),
                });
            }
            MessageSegment::Face { id, .. } => {
                let face_name = face_id_to_name(id).expect("validated face id");
                body_steps.push(InlineStep::InsertFace {
                    face_name: face_name.to_string(),
                });
                canonical_segments.push(MessageSegment::Face {
                    id: id.clone(),
                    text: Some(face_name.to_string()),
                });
            }
            MessageSegment::At { qq, name } => {
                let is_all = qq.trim().eq_ignore_ascii_case(ONEBOT_AT_ALL_QQ);
                let display_name = if is_all {
                    AT_ALL_DISPLAY_NAME.to_string()
                } else {
                    outbound_media::resolve_at_display_name(qq, name.as_deref(), members)
                };
                verify_tokens.push(display_name.clone());
                body_steps.push(InlineStep::InsertAt {
                    display_name: display_name.clone(),
                    is_all,
                });
                canonical_segments.push(MessageSegment::At {
                    qq: qq.clone(),
                    name: Some(display_name),
                });
            }
            MessageSegment::Text { text } => {
                if !text.is_empty() {
                    let norm = parser::normalize_message_text(text);
                    if !norm.is_empty() {
                        verify_tokens.push(norm);
                    }
                    body_steps.push(InlineStep::TypeText { text: text.clone() });
                }
                canonical_segments.push(MessageSegment::Text { text: text.clone() });
            }
            MessageSegment::Image {
                file,
                url,
                summary,
                image_type,
            } => {
                let raw_source = outbound_media::select_image_source(file, url.as_deref())?;
                let bytes = outbound_media::load_image_bytes_sync(&raw_source)?;
                let bitmap = decode_image_bytes_to_bgra(&bytes)?;
                has_image = true;
                verify_tokens.push(IMAGE_PLACEHOLDER_TOKEN.to_string());
                body_steps.push(InlineStep::PasteImage { bitmap });
                canonical_segments.push(MessageSegment::Image {
                    file: file.clone(),
                    url: url.clone(),
                    summary: summary
                        .clone()
                        .or_else(|| Some(IMAGE_PLACEHOLDER_TOKEN.to_string())),
                    image_type: image_type.clone(),
                });
            }
            _ => {
                return Err(AdapterError::InputRejected(
                    "unexpected segment in rich inline plan builder".to_string(),
                ));
            }
        }
    }

    let mut steps = Vec::with_capacity(body_steps.len() + usize::from(reply_step.is_some()));
    if let Some(reply) = reply_step {
        steps.push(reply);
    }
    steps.extend(body_steps);

    let summary_text = segments_to_plain_summary(&canonical_segments);
    Ok(ComposerExecutionPlan::RichInline {
        steps,
        canonical_segments,
        summary_text,
        verify_tokens,
        has_image,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage::Storage;

    #[test]
    fn strict_plan_rejects_outbound_poke_without_plain_text_fallback() {
        let storage = Storage::open(":memory:").expect("in memory storage");
        let poke_seg = [MessageSegment::Poke {
            poke_type: "poke".to_string(),
            id: "200000002".to_string(),
            name: None,
        }];

        // 严格模式下群聊出站戳一戳必须返回 UnsupportedAction，绝不生成纯文本降级计划
        let group_res = build_composer_plan(
            &storage,
            &[],
            ChatTargetKind::Group,
            ("", ChatTargetKind::Group),
            &poke_seg,
            false,
        );
        assert!(matches!(
            group_res,
            Err(AdapterError::UnsupportedAction { .. })
        ));

        // 严格模式下私聊出站戳一戳同样返回 UnsupportedAction
        let priv_res = build_composer_plan(
            &storage,
            &[],
            ChatTargetKind::Private,
            ("", ChatTargetKind::Private),
            &poke_seg,
            false,
        );
        assert!(matches!(
            priv_res,
            Err(AdapterError::UnsupportedAction { .. })
        ));
    }

    #[test]
    fn strict_plan_stages_video_and_populates_file_size_and_rejects_invalid_combos() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;

        let storage = Storage::open(":memory:").expect("in memory storage");
        let mp4 = file_staging::build_minimal_valid_mp4_bytes();
        let b64_video = format!("base64://{}", STANDARD.encode(&mp4));
        let video_plan = build_composer_plan(
            &storage,
            &[],
            ChatTargetKind::Group,
            ("", ChatTargetKind::Group),
            &[MessageSegment::Video {
                file: b64_video,
                url: None,
            }],
            false,
        )
        .expect("stage base64 video");
        match video_plan {
            ComposerExecutionPlan::Standalone {
                action: StandaloneAction::Video { path },
                canonical_segments,
                ..
            } => {
                assert!(path.is_file());
                assert_eq!(path.extension().and_then(|s| s.to_str()), Some("mp4"));
                assert!(matches!(
                    canonical_segments[0],
                    MessageSegment::Video { .. }
                ));
                let _ = std::fs::remove_file(&path);
            }
            _ => panic!("expected standalone video plan"),
        }

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let src_path = std::env::temp_dir().join(format!("qq_plan_src_{nanos}.txt"));
        std::fs::write(&src_path, b"hello-plan-file-123").expect("write temp file");
        let file_plan = build_composer_plan(
            &storage,
            &[],
            ChatTargetKind::Private,
            ("", ChatTargetKind::Private),
            &[MessageSegment::File {
                file: src_path.display().to_string(),
                name: Some("custom_echo_name.txt".to_string()),
                size: None,
            }],
            false,
        )
        .expect("stage renamed file");
        match file_plan {
            ComposerExecutionPlan::Standalone {
                action: StandaloneAction::File { path },
                canonical_segments,
                ..
            } => {
                assert_eq!(
                    path.file_name().and_then(|s| s.to_str()),
                    Some("custom_echo_name.txt")
                );
                assert!(matches!(
                    &canonical_segments[0],
                    MessageSegment::File {
                        name: Some(n),
                        size: Some(19),
                        ..
                    } if n == "custom_echo_name.txt"
                ));
                if let Some(parent) = path.parent() {
                    let _ = std::fs::remove_file(&path);
                    let _ = std::fs::remove_dir(parent);
                }
            }
            _ => panic!("expected standalone file plan"),
        }
        let _ = std::fs::remove_file(&src_path);

        let rejected_cases: &[&[MessageSegment]] = &[
            &[MessageSegment::Video {
                file: "https://example.com/clip.mp4".to_string(),
                url: None,
            }],
            &[MessageSegment::File {
                file: "http://example.com/archive.zip".to_string(),
                name: None,
                size: None,
            }],
            &[
                MessageSegment::Video {
                    file: format!("data:video/mp4;base64,{}", STANDARD.encode(&mp4)),
                    url: None,
                },
                MessageSegment::Text {
                    text: "mixed".to_string(),
                },
            ],
            &[MessageSegment::Record {
                file: "voice.amr".to_string(),
                magic: None,
                url: None,
            }],
            &[MessageSegment::Image {
                file: "pic.png".to_string(),
                url: None,
                summary: None,
                image_type: Some("flash".to_string()),
            }],
        ];
        for segs in rejected_cases {
            let err = build_composer_plan(
                &storage,
                &[],
                ChatTargetKind::Group,
                ("", ChatTargetKind::Group),
                segs,
                false,
            )
            .err()
            .expect("strict unsupported segment must fail");
            assert!(
                matches!(err, AdapterError::UnsupportedAction { .. }),
                "expected UnsupportedAction for {segs:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn build_composer_plan_rejects_cross_session_reply_targets() {
        let storage = Storage::open(":memory:").expect("open memory storage");
        let group_msg_id = storage
            .record_message(&storage::RecordMessageInput {
                message_uid: "uid-plan-group-a".to_string(),
                group_name: "群甲".to_string(),
                group_id: 111222,
                user_id: 333444,
                sender_name: "群友甲".to_string(),
                raw_message: "秘密内容A".to_string(),
                is_self: false,
                timestamp_hint: Some("10:00".to_string()),
                captured_at_ms: 1_700_000_001_000,
                chat_type: ChatTargetKind::Group,
                segments: Vec::new(),
            })
            .expect("record group message");
        let priv_same_name_id = storage
            .record_message(&storage::RecordMessageInput {
                message_uid: "uid-plan-priv-same".to_string(),
                group_name: "同名会话".to_string(),
                group_id: 0,
                user_id: 555666,
                sender_name: "同名会话".to_string(),
                raw_message: "私聊秘密B".to_string(),
                is_self: false,
                timestamp_hint: Some("10:01".to_string()),
                captured_at_ms: 1_700_000_002_000,
                chat_type: ChatTargetKind::Private,
                segments: Vec::new(),
            })
            .expect("record private message");

        // 跨群引用及同名不同聊天类型引用均须被拒绝，防止越权泄漏他群摘要
        let err_cross_group = match build_composer_plan(
            &storage,
            &[],
            ChatTargetKind::Group,
            ("群乙", ChatTargetKind::Group),
            &[
                MessageSegment::Reply {
                    id: group_msg_id.to_string(),
                },
                MessageSegment::Text {
                    text: "ping".to_string(),
                },
            ],
            false,
        ) {
            Err(err) => err,
            Ok(_) => panic!("cross-group reply must fail"),
        };
        assert!(matches!(
            err_cross_group,
            AdapterError::InputRejected(ref msg) if msg.contains("belongs to another session")
        ));

        let err_cross_kind = match build_composer_plan(
            &storage,
            &[],
            ChatTargetKind::Group,
            ("同名会话", ChatTargetKind::Group),
            &[
                MessageSegment::Reply {
                    id: priv_same_name_id.to_string(),
                },
                MessageSegment::Text {
                    text: "ping".to_string(),
                },
            ],
            false,
        ) {
            Err(err) => err,
            Ok(_) => panic!("cross-kind reply must fail"),
        };
        assert!(matches!(
            err_cross_kind,
            AdapterError::InputRejected(ref msg) if msg.contains("belongs to another session")
        ));

        let err_rich = match build_rich_inline_plan(
            &storage,
            &[],
            ("群乙", ChatTargetKind::Group),
            &[
                MessageSegment::Reply {
                    id: group_msg_id.to_string(),
                },
                MessageSegment::Image {
                    file: "base64://iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=".to_string(),
                    url: None,
                    summary: None,
                    image_type: None,
                },
            ],
        ) {
            Err(err) => err,
            Ok(_) => panic!("rich inline cross-session reply must fail"),
        };
        assert!(matches!(
            err_rich,
            AdapterError::InputRejected(ref msg) if msg.contains("belongs to another session")
        ));

        // 匹配同会话同聊天类型的回复可正常构建计划
        let ok_plan = build_composer_plan(
            &storage,
            &[],
            ChatTargetKind::Private,
            ("同名会话", ChatTargetKind::Private),
            &[
                MessageSegment::Reply {
                    id: priv_same_name_id.to_string(),
                },
                MessageSegment::Text {
                    text: "收到".to_string(),
                },
            ],
            false,
        )
        .expect("matching session reply must succeed");
        assert!(matches!(
            ok_plan,
            ComposerExecutionPlan::RichInline { ref steps, .. }
                if steps.iter().any(|step| matches!(step, InlineStep::AttachReply { .. }))
        ));
    }
}
