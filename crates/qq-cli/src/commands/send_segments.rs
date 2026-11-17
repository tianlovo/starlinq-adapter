//! CLI `send` 与 `messages` 参数结构体及多段结构化消息段构建器

use std::path::Path;

use anyhow::{bail, Result};
use common_types::{face_id_to_name, face_name_to_id, InputMode, MessageSegment};

/// `send` 子命令组装后的参数载体（用于控制函数入参个数并统一多段消息构建）
#[derive(Debug, Clone, Default)]
pub struct SendCommandArgs {
    pub group: Option<String>,
    pub private: bool,
    pub text: Option<String>,
    pub image: Option<String>,
    pub file: Option<String>,
    pub video: Option<String>,
    pub face: Vec<String>,
    pub at: Vec<String>,
    pub reply: Option<i64>,
    pub mode: Option<InputMode>,
    pub response_window_ms: Option<u64>,
}

/// `messages` 子命令参数载体（统一群聊拉取、好友私聊切换与离线历史查询）
#[derive(Debug, Clone, Default)]
pub struct MessagesCommandArgs {
    pub group: Option<String>,
    pub private: bool,
    pub history: bool,
    pub limit: usize,
}

fn parse_at_segment(raw: &str) -> Result<MessageSegment> {
    let trimmed = raw.trim().trim_start_matches('@').trim();
    if trimmed.is_empty() {
        bail!("--at 目标不能为空");
    }
    if trimmed.eq_ignore_ascii_case("all") || trimmed == "全体成员" {
        return Ok(MessageSegment::At {
            qq: "all".to_string(),
            name: None,
        });
    }
    if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return Ok(MessageSegment::At {
            qq: trimmed.to_string(),
            name: None,
        });
    }
    Ok(MessageSegment::At {
        qq: trimmed.to_string(),
        name: Some(trimmed.to_string()),
    })
}

fn resolve_face_segment(raw: &str) -> Result<MessageSegment> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("--face 参数不能为空");
    }
    if let Some(name) = face_id_to_name(trimmed) {
        return Ok(MessageSegment::Face {
            id: trimmed.to_string(),
            text: Some(name.to_string()),
        });
    }
    if let Some(id) = face_name_to_id(trimmed) {
        return Ok(MessageSegment::Face {
            id: id.to_string(),
            text: Some(face_id_to_name(id).unwrap_or(trimmed).to_string()),
        });
    }
    bail!("未知 QQ 系统表情 '{trimmed}'（请输入数字编号如 14 或中文名称如 微笑）");
}

fn build_file_segment(raw: &str) -> MessageSegment {
    let file = raw.trim().to_string();
    let name = Path::new(&file)
        .file_name()
        .and_then(|val| val.to_str())
        .map(str::trim)
        .filter(|val| !val.is_empty())
        .map(ToOwned::to_owned);
    MessageSegment::File {
        file,
        name,
        size: None,
    }
}

/// 按规范顺序将 CLI 发送参数组装为 `Vec<MessageSegment>`，空消息或无效表情直接报错
pub fn build_send_segments(args: &SendCommandArgs) -> Result<Vec<MessageSegment>> {
    let mut segments = Vec::new();
    if let Some(reply_id) = args.reply {
        if reply_id <= 0 {
            bail!("--reply 引用消息 ID 必须为正整数");
        }
        segments.push(MessageSegment::Reply {
            id: reply_id.to_string(),
        });
    }
    for at_target in &args.at {
        segments.push(parse_at_segment(at_target)?);
    }
    for face_input in &args.face {
        segments.push(resolve_face_segment(face_input)?);
    }
    if let Some(text) = args.text.as_deref().filter(|val| !val.trim().is_empty()) {
        segments.push(MessageSegment::Text {
            text: text.to_string(),
        });
    }
    if let Some(image) = args
        .image
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty())
    {
        segments.push(MessageSegment::Image {
            file: image.to_string(),
            url: None,
            summary: None,
            image_type: None,
        });
    }
    if let Some(file) = args
        .file
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty())
    {
        segments.push(build_file_segment(file));
    }
    if let Some(video) = args
        .video
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty())
    {
        segments.push(MessageSegment::Video {
            file: video.to_string(),
            url: None,
        });
    }
    if segments.is_empty() {
        bail!(
            "send 命令至少需要提供 --text、--image、--file、--video、--face、--at 或 --reply 之一"
        );
    }
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_send_segments_assembles_reply_at_face_text_image_file_in_order() {
        let args = SendCommandArgs {
            group: Some("Demo测试群".to_string()),
            reply: Some(10_005),
            at: vec!["20001".to_string(), "@all".to_string(), "群友A".to_string()],
            face: vec!["14".to_string(), "微笑".to_string()],
            text: Some("hello world".to_string()),
            image: Some("demo.png".to_string()),
            file: Some(r"C:\tmp\report.pdf".to_string()),
            ..Default::default()
        };

        let segments = build_send_segments(&args).unwrap();
        assert_eq!(segments.len(), 9);
        assert_eq!(
            segments[0],
            MessageSegment::Reply {
                id: "10005".to_string()
            }
        );
        assert_eq!(
            segments[1],
            MessageSegment::At {
                qq: "20001".to_string(),
                name: None
            }
        );
        assert_eq!(
            segments[2],
            MessageSegment::At {
                qq: "all".to_string(),
                name: None
            }
        );
        assert_eq!(
            segments[3],
            MessageSegment::At {
                qq: "群友A".to_string(),
                name: Some("群友A".to_string())
            }
        );
        assert_eq!(
            segments[4],
            MessageSegment::Face {
                id: "14".to_string(),
                text: Some("微笑".to_string())
            }
        );
        assert_eq!(segments[5], segments[4]);
        assert_eq!(
            segments[6],
            MessageSegment::Text {
                text: "hello world".to_string()
            }
        );
        assert!(matches!(
            &segments[7],
            MessageSegment::Image { file, .. } if file == "demo.png"
        ));
        assert_eq!(
            segments[8],
            MessageSegment::File {
                file: r"C:\tmp\report.pdf".to_string(),
                name: Some("report.pdf".to_string()),
                size: None,
            }
        );

        let video_args = SendCommandArgs {
            group: Some("Demo测试群".to_string()),
            video: Some(r"C:\tmp\clip.mp4".to_string()),
            ..Default::default()
        };
        let video_segments = build_send_segments(&video_args).unwrap();
        assert_eq!(
            video_segments,
            vec![MessageSegment::Video {
                file: r"C:\tmp\clip.mp4".to_string(),
                url: None,
            }]
        );
    }

    #[test]
    fn build_send_segments_rejects_empty_invalid_reply_and_unknown_face() {
        let empty = SendCommandArgs::default();
        assert!(build_send_segments(&empty).is_err());

        let bad_reply = SendCommandArgs {
            reply: Some(0),
            text: Some("hi".to_string()),
            ..Default::default()
        };
        assert!(build_send_segments(&bad_reply).is_err());

        let bad_face_name = SendCommandArgs {
            face: vec!["不存在的神秘表情".to_string()],
            ..Default::default()
        };
        assert!(build_send_segments(&bad_face_name).is_err());

        let bad_face_id = SendCommandArgs {
            face: vec!["999999".to_string()],
            ..Default::default()
        };
        assert!(build_send_segments(&bad_face_id).is_err());

        let empty_at = SendCommandArgs {
            at: vec![" @ ".to_string()],
            ..Default::default()
        };
        assert!(build_send_segments(&empty_at).is_err());
    }
}
