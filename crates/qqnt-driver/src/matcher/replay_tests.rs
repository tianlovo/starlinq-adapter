//! Round-2 真实客户端 UIA 树证据回放单测，校验群标题误读、空白页误判与搜索态识别

use std::path::Path;

use common_types::UiNodeSnapshot;

use super::{
    extract_sessions_from_tree, is_search_mode_active_in_tree, read_chat_header_from_tree,
};

fn load_reverify_tree(file_name: &str) -> UiNodeSnapshot {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/public")
        .join(file_name);
    let raw_text = std::fs::read_to_string(&path).expect("read reverify tree json");
    let raw_json: serde_json::Value = serde_json::from_str(&raw_text).expect("parse wrapper json");
    serde_json::from_value(raw_json["response"].clone()).expect("parse UiNodeSnapshot response")
}

#[test]
fn reverify_07_user_recovered_tree_reads_real_header_and_blocks_tall_message_bubble() {
    let tree = load_reverify_tree("07-user-recovered-tree.json");
    let (hdr, cnt) = read_chat_header_from_tree(&tree, tree.rect, false, None);
    assert_eq!(hdr.as_deref(), Some("Demo测试群"));
    assert_eq!(cnt, Some(9));
    // 该现场快照内左上角搜索框仍残留 'Demo测试群' 与 '清空' 按钮，搜索态检测应准确返回 true
    assert!(is_search_mode_active_in_tree(&tree));
}

#[test]
fn reverify_07_recover_search_result_tree_detects_search_mode_and_excludes_chrome_sessions() {
    let tree = load_reverify_tree("07-recover-search-result-tree.json");
    assert!(is_search_mode_active_in_tree(&tree));
    let sessions = extract_sessions_from_tree(&tree);
    for session in &sessions {
        assert!(
            !session.title.contains('\n'),
            "unexpected multiline session title: {:?}",
            session.title
        );
        for field in [session.title.as_str(), session.raw_name.as_str()] {
            assert!(
                !matches!(
                    field,
                    "清空"
                        | "切换为经典模式"
                        | "进入综合搜索"
                        | "综合搜索"
                        | "查找用户、群聊等"
                        | "群聊"
                ),
                "chrome pollution leaked into session list: {:?}",
                field
            );
        }
    }
}
