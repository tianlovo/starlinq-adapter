//! Win32 底层探测、候选过滤、置顶护栏与不可变窗口身份绑定单元测试模块

use std::collections::HashSet;
use std::ffi::c_void;
use std::path::Path;
use std::time::{Duration, Instant};

use common_types::constants::paths::DEFAULT_QQ_EXECUTABLE_PATH;
use common_types::constants::processes::{
    CHROME_WIDGET_WINDOW_CLASS, MOCK_QQ_MAIN_CLASS, MOCK_QQ_SECURITY_CLASS, QQ_PROCESS_NAMES,
    QQ_WINDOW_TITLE, TX_GUI_FOUNDATION_CLASS,
};
use common_types::constants::ui_labels::SECURITY_KEYWORDS;
use common_types::{AdapterError, WindowInfo};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GetCursorPos, GetForegroundWindow, GetWindowLongW, MoveWindow,
    SetWindowTextW, ShowWindow, SystemParametersInfoW, GWL_EXSTYLE, SPI_GETSCREENREADER, SW_SHOWNA,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINDOW_EX_STYLE, WS_EX_TOPMOST, WS_POPUP,
};
use windows_core::w;

use super::process_lookup::is_exact_qq_process_name;
use super::window_enum::{
    build_explicit_extra_classes, is_security_window_candidate, is_security_window_candidate_ext,
    should_include_candidate_window,
};
use super::*;
use crate::constants::QQ_CHINESE_WINDOW_TITLE;

/// 测试专用 Win32 临时窗口句柄 RAII 销毁守护对象
struct TestWindowGuard(HWND);

impl Drop for TestWindowGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

#[test]
fn desktop_info_returns_non_empty_name() {
    let (name, _isolated) = current_desktop_info();
    assert!(!name.trim().is_empty());
}

#[test]
fn process_snapshot_finds_current_process_by_name() {
    let executable = std::env::current_exe().expect("获取当前测试可执行文件路径");
    let file_name = executable
        .file_name()
        .and_then(|segment| segment.to_str())
        .expect("解析当前测试可执行文件名")
        .to_string();
    let pids = find_pids_by_names(&[file_name]);
    assert!(pids.contains(&std::process::id()));
}

#[test]
fn resolve_qq_executable_prefers_existing_configured_and_validates_launch_path() {
    let temp_file =
        std::env::temp_dir().join(format!("qq-uia-resolve-test-{}.exe", std::process::id()));
    std::fs::write(&temp_file, b"stub").expect("写入临时模拟可执行文件");
    let temp_path_str = temp_file.to_string_lossy().to_string();

    let resolved = resolve_qq_executable(Some(&temp_path_str), &[]);
    assert_eq!(resolved.as_deref(), Some(temp_path_str.as_str()));

    let _ = std::fs::remove_file(&temp_file);

    let fallback_qq = DEFAULT_QQ_EXECUTABLE_PATH;
    let resolved_pid_or_fallback =
        resolve_qq_executable(Some(r"Z:\nonexistent\QQ.exe"), &[std::process::id()]);
    if Path::new(fallback_qq).is_file() {
        assert_eq!(resolved_pid_or_fallback.as_deref(), Some(fallback_qq));
    } else if let Some(ref found_path) = resolved_pid_or_fallback {
        assert!(
            Path::new(found_path)
                .file_name()
                .and_then(|segment| segment.to_str())
                .map(|segment| segment.eq_ignore_ascii_case(QQ_PROCESS_NAMES[0]))
                .unwrap_or(false),
            "解析到的可执行文件必须为 QQ.exe，实际得到 {found_path}"
        );
    }

    let launch_error = launch_or_wake_qq(r"Z:\nonexistent\missing_QQ.exe", &[]).unwrap_err();
    assert_eq!(launch_error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn chrome_widget_win_rejects_non_qq_processes_even_with_qq_title_and_extra_classes() {
    let pid_set: HashSet<u32> = HashSet::new();
    let explicit_extra = build_explicit_extra_classes(&[
        CHROME_WIDGET_WINDOW_CLASS.to_string(),
        TX_GUI_FOUNDATION_CLASS.to_string(),
        MOCK_QQ_MAIN_CLASS.to_string(),
    ]);

    // 通用 Chromium 与旧版 QQ 窗口类名必须被显式白名单剥离
    assert!(!explicit_extra.contains(CHROME_WIDGET_WINDOW_CLASS));
    assert!(!explicit_extra.contains(TX_GUI_FOUNDATION_CLASS));
    assert!(explicit_extra.contains(MOCK_QQ_MAIN_CLASS));

    // 非 QQ 进程即使窗口标题包含 QQ 且被传入扩展类名列表中也绝对不能被误匹配
    assert!(!should_include_candidate_window(
        4242,
        &pid_set,
        "Antigravity.exe",
        CHROME_WIDGET_WINDOW_CLASS,
        "QQ Bot GUI Automation Dev",
        &explicit_extra,
    ));
    assert!(!should_include_candidate_window(
        4243,
        &pid_set,
        "msedge.exe",
        CHROME_WIDGET_WINDOW_CLASS,
        QQ_WINDOW_TITLE,
        &explicit_extra,
    ));
    assert!(!should_include_candidate_window(
        4244,
        &pid_set,
        "qq-uia-adapter-daemon.exe",
        CHROME_WIDGET_WINDOW_CLASS,
        QQ_WINDOW_TITLE,
        &explicit_extra,
    ));
    assert!(!should_include_candidate_window(
        4245,
        &pid_set,
        "SomeLegacyApp.exe",
        TX_GUI_FOUNDATION_CLASS,
        QQ_CHINESE_WINDOW_TITLE,
        &explicit_extra,
    ));

    // 即使 pid_set 为空，真实的 QQ.exe / QQEX.exe 进程仍然应当被正确识别
    assert!(should_include_candidate_window(
        5001,
        &pid_set,
        "QQ.exe",
        CHROME_WIDGET_WINDOW_CLASS,
        QQ_WINDOW_TITLE,
        &explicit_extra,
    ));
    assert!(should_include_candidate_window(
        5002,
        &pid_set,
        "qqex.exe",
        TX_GUI_FOUNDATION_CLASS,
        QQ_WINDOW_TITLE,
        &explicit_extra,
    ));

    // 测试专用类名 MockQQNTWndClass 仅在调用方显式请求时才匹配
    assert!(should_include_candidate_window(
        6001,
        &pid_set,
        "mock_test.exe",
        MOCK_QQ_MAIN_CLASS,
        QQ_WINDOW_TITLE,
        &explicit_extra,
    ));
    let empty_extra = build_explicit_extra_classes(&[]);
    assert!(!should_include_candidate_window(
        6001,
        &pid_set,
        "mock_test.exe",
        MOCK_QQ_MAIN_CLASS,
        QQ_WINDOW_TITLE,
        &empty_extra,
    ));

    // 实盘枚举传入空 pids 与 Chrome_WidgetWin_1 时绝不能捕获任何非 QQ 进程窗口
    let live_windows = enumerate_candidate_windows(
        &[],
        &[
            CHROME_WIDGET_WINDOW_CLASS.to_string(),
            TX_GUI_FOUNDATION_CLASS.to_string(),
        ],
    );
    for window_item in &live_windows {
        assert!(
            is_exact_qq_process_name(&window_item.process_name),
            "不应匹配到非 QQ 候选窗口: {:?}",
            window_item
        );
    }
}

#[test]
fn find_windows_with_keywords_returns_empty_when_pids_is_empty() {
    let empty_pids: HashSet<u32> = HashSet::new();
    assert!(!is_security_window_candidate(
        &empty_pids,
        1234,
        0,
        CHROME_WIDGET_WINDOW_CLASS,
        "",
    ));
    assert!(is_security_window_candidate(
        &empty_pids,
        1234,
        0,
        MOCK_QQ_SECURITY_CLASS,
        "",
    ));
    assert!(is_security_window_candidate(
        &empty_pids,
        1234,
        999,
        "#32770",
        MOCK_QQ_MAIN_CLASS,
    ));
    // 验证 QA-BUG-010：生产守护关闭 allow_mock 时严禁匹配 Mock 安全弹窗类
    assert!(!is_security_window_candidate_ext(
        &empty_pids,
        1234,
        0,
        MOCK_QQ_SECURITY_CLASS,
        "",
        false,
    ));
    assert!(!is_security_window_candidate_ext(
        &empty_pids,
        1234,
        999,
        "#32770",
        MOCK_QQ_MAIN_CLASS,
        false,
    ));

    let hits = find_windows_with_keywords(&[], SECURITY_KEYWORDS);
    for (window_hwnd, _title) in &hits {
        let class_name = get_class_name(*window_hwnd);
        assert!(
            class_name == MOCK_QQ_SECURITY_CLASS,
            "空 pids 下不应误击中外部风控窗口: class={class_name}"
        );
    }
    assert!(
        hits.is_empty(),
        "空 pids 时桌面上不应命中风控窗口，实际命中: {hits:?}"
    );
}

#[test]
fn refresh_window_info_tracks_live_state_and_rejects_invalid_hwnd() {
    let mut invalid = WindowInfo {
        hwnd: 0,
        pid: 0,
        process_name: String::new(),
        class_name: String::new(),
        title: "stale".to_string(),
        is_visible: true,
        is_iconic: true,
        rect: [1, 2, 3, 4],
        is_standalone_chat: false,
    };
    assert!(!refresh_window_info(&mut invalid));

    let created_hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("InitialTitle"),
            WS_POPUP,
            10,
            20,
            300,
            200,
            None,
            None,
            None,
            None,
        )
        .expect("创建测试静态窗口")
    };
    let guard = TestWindowGuard(created_hwnd);

    let mut info = WindowInfo {
        hwnd: created_hwnd.0 as isize,
        pid: std::process::id(),
        process_name: String::new(),
        class_name: "STATIC".to_string(),
        title: "Outdated".to_string(),
        is_visible: true,
        is_iconic: true,
        rect: [0, 0, 0, 0],
        is_standalone_chat: false,
    };
    assert!(refresh_window_info(&mut info));
    assert_eq!(info.title, "InitialTitle");
    assert!(!info.is_visible);
    assert!(!info.is_iconic);
    assert_eq!(info.rect[2] - info.rect[0], 300);
    assert_eq!(info.rect[3] - info.rect[1], 200);

    unsafe {
        SetWindowTextW(created_hwnd, w!("UpdatedTitle")).expect("更新测试窗口标题");
        MoveWindow(created_hwnd, 40, 50, 420, 260, false).expect("调整测试窗口尺寸");
        let _ = ShowWindow(created_hwnd, SW_SHOWNA);
    }
    assert!(refresh_window_info(&mut info));
    assert_eq!(info.title, "UpdatedTitle");
    assert!(info.is_visible);
    assert!(!info.is_iconic);
    assert_eq!(info.rect[2] - info.rect[0], 420);
    assert_eq!(info.rect[3] - info.rect[1], 260);

    let raw_hwnd = created_hwnd.0 as isize;
    drop(guard);
    info.hwnd = raw_hwnd;
    assert!(!refresh_window_info(&mut info));
}

#[test]
fn topmost_unocclude_guard_sets_and_clears_topmost_without_stealing_mouse() {
    assert!(TopmostUnoccludeGuard::acquire(0).is_none());

    let handle = mock_qq_win::MockQqServer::spawn().expect("启动 MockQqServer");
    let window_hwnd = handle.hwnd();
    let raw_hwnd = HWND(window_hwnd as *mut c_void);

    let ex_style_before = unsafe { GetWindowLongW(raw_hwnd, GWL_EXSTYLE) } as u32;
    assert_eq!(
        ex_style_before & WS_EX_TOPMOST.0,
        0,
        "MockQqServer 初始不应带有 WS_EX_TOPMOST 样式"
    );

    let mut cursor_before = POINT::default();
    let has_cursor_before = unsafe { GetCursorPos(&mut cursor_before) }.is_ok();
    let foreground_before = unsafe { GetForegroundWindow() };

    {
        let _guard = TopmostUnoccludeGuard::acquire(window_hwnd)
            .expect("获取 TopmostUnoccludeGuard 必须成功");
        let ex_style_held = unsafe { GetWindowLongW(raw_hwnd, GWL_EXSTYLE) } as u32;
        assert_ne!(
            ex_style_held & WS_EX_TOPMOST.0,
            0,
            "持有 TopmostUnoccludeGuard 期间必须设置 WS_EX_TOPMOST"
        );
        assert_eq!(
            unsafe { GetForegroundWindow() },
            foreground_before,
            "TopmostUnoccludeGuard::acquire 严禁窃取前台焦点"
        );
    }

    let ex_style_after = unsafe { GetWindowLongW(raw_hwnd, GWL_EXSTYLE) } as u32;
    assert_eq!(
        ex_style_after & WS_EX_TOPMOST.0,
        0,
        "TopmostUnoccludeGuard 析构后必须清除 WS_EX_TOPMOST"
    );
    assert_eq!(
        unsafe { GetForegroundWindow() },
        foreground_before,
        "TopmostUnoccludeGuard::drop 严禁窃取前台焦点"
    );
    if has_cursor_before {
        let mut cursor_after = POINT::default();
        if unsafe { GetCursorPos(&mut cursor_after) }.is_ok() {
            assert_eq!(cursor_after.x, cursor_before.x);
            assert_eq!(cursor_after.y, cursor_before.y);
        }
    }

    handle.shutdown();
    assert!(TopmostUnoccludeGuard::acquire(window_hwnd).is_none());
}

#[test]
fn wake_ext_and_diag_enumeration_cover_mock_window_and_restore_screen_reader() {
    let handle = mock_qq_win::MockQqServer::spawn().expect("启动 MockQqServer");
    let window_hwnd = handle.hwnd();

    let all_windows = enumerate_all_windows_for_pids(&[std::process::id()]);
    assert!(
        all_windows.iter().any(|item| item.hwnd == window_hwnd),
        "enumerate_all_windows_for_pids 必须包含 MockQqServer"
    );
    let children = enumerate_all_child_windows(window_hwnd);
    assert!(
        children.len() >= 5,
        "MockQqServer 至少应包含 5 个子控件，实际数量: {}",
        children.len()
    );

    let report_no_pulse = wake_chromium_accessibility_ext(window_hwnd, false);
    assert!(report_no_pulse.target_hwnds_before.contains(&window_hwnd));
    assert!(report_no_pulse.target_hwnds_after.contains(&window_hwnd));
    assert!(!report_no_pulse.pulsed_screen_reader);
    assert!(report_no_pulse.visited_msaa_nodes > 0);
    assert!(report_no_pulse.acc_hit_test_ok > 0);
    assert!(!report_no_pulse.msaa_samples.is_empty());

    let report_pulse = wake_chromium_accessibility_ext(window_hwnd, true);
    assert!(report_pulse.visited_msaa_nodes > 0);
    assert!(report_pulse.acc_hit_test_ok > 0);

    // 验证 SPI_GETSCREENREADER 标志已可靠恢复至脉冲操作前状态
    let mut active_after: i32 = 0;
    let query_ok = unsafe {
        SystemParametersInfoW(
            SPI_GETSCREENREADER,
            0,
            Some(&mut active_after as *mut i32 as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok();
    if query_ok {
        assert_eq!(active_after != 0, report_pulse.screen_reader_flag_before);
    }

    handle.shutdown();
}

#[test]
fn verified_window_identity_binds_mock_rejects_invalid_and_verifies_fast_under_15ms() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<VerifiedWindowIdentity>();
    let _ = are_mouse_buttons_pressed();
    assert_eq!(get_root_ancestor(0), 0);

    assert!(matches!(
        VerifiedWindowIdentity::verify_and_bind(0, true),
        Err(AdapterError::WindowNotFound(_))
    ));
    assert!(matches!(
        VerifiedWindowIdentity::verify_and_bind(0, false),
        Err(AdapterError::WindowNotFound(_))
    ));

    let non_qq_hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("QQ"),
            WS_POPUP,
            0,
            0,
            100,
            100,
            None,
            None,
            None,
            None,
        )
    }
    .expect("创建非 QQ 静态对比窗口");
    let non_qq_guard = TestWindowGuard(non_qq_hwnd);
    assert!(matches!(
        VerifiedWindowIdentity::verify_and_bind(non_qq_guard.0 .0 as isize, false),
        Err(AdapterError::WindowNotFound(_))
    ));
    assert!(matches!(
        VerifiedWindowIdentity::verify_and_bind(non_qq_guard.0 .0 as isize, true),
        Err(AdapterError::WindowNotFound(_))
    ));
    drop(non_qq_guard);

    let mock_server = mock_qq_win::MockQqServer::spawn().expect("启动模拟 QQ 窗口");
    let mock_hwnd = mock_server.hwnd();

    assert!(matches!(
        VerifiedWindowIdentity::verify_and_bind(mock_hwnd, false),
        Err(AdapterError::WindowNotFound(_))
    ));

    let identity =
        VerifiedWindowIdentity::verify_and_bind(mock_hwnd, true).expect("绑定模拟 QQ 窗口");
    assert_eq!(identity.hwnd(), mock_hwnd);
    assert_eq!(identity.pid(), std::process::id());
    assert_eq!(identity.class_name(), MOCK_QQ_MAIN_CLASS);
    assert!(!identity.process_name().is_empty());
    assert!(identity.creation_time_100ns() > 0);
    assert_eq!(get_root_ancestor(mock_hwnd), mock_hwnd);
    if let Some(edit_hwnd) = find_child_window_by_class(mock_hwnd, "Edit") {
        assert_eq!(get_root_ancestor(edit_hwnd), mock_hwnd);
    }

    let start = Instant::now();
    for _ in 0..1000 {
        identity
            .verify_fast()
            .expect("模拟窗口存活期间 verify_fast 应持续通过");
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(15),
        "1000 次 verify_fast() 耗时 {elapsed:?}（超过 15ms 预算）"
    );

    mock_server.shutdown();
    assert!(matches!(
        identity.verify_fast(),
        Err(AdapterError::WindowNotFound(_))
    ));
}
