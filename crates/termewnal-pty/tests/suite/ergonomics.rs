use std::path::Path;
use std::time::Duration;

use termewnal_core::{HighlightRule, RgbColor};
use termewnal_pty::{
    CursorShape, DetectedLinkKind, GridSearchMatch, PtyMouseButton, PtyMouseEventKind, PtySession,
    PtySpawnConfig, Scroll, SelectionType, ShellSpec, Side, TerminalBuffer,
    compile_highlight_rules, compute_highlight_grid, detect_links_in_snapshot,
    detect_urls_in_snapshot, encode_mouse_event, link_at_cell, shell_escape_path,
    shell_escape_paths, url_at_cell,
};

fn test_pty_config(cols: u16, rows: u16, scrollback_lines: usize) -> PtySpawnConfig {
    let mut cfg = PtySpawnConfig {
        cols,
        rows,
        scrollback_lines,
        ..PtySpawnConfig::default()
    };
    #[cfg(unix)]
    if Path::new("/bin/sh").exists() {
        cfg.shell = ShellSpec::custom("/bin/sh", Vec::new());
    }
    cfg
}

#[test]
fn scrollback_defaults_to_10000_respects_custom_limit_and_scrolls_display() {
    let default_cfg = PtySpawnConfig::default();
    assert_eq!(default_cfg.scrollback_lines, 10_000);

    // Real PTY session retaining lines in scrollback and navigating via scroll_display.
    let session =
        PtySession::spawn(test_pty_config(80, 10, 10_000)).expect("spawn PTY with 10k scrollback");

    session
        .write_bytes(
            b"i=1; while [ \"$i\" -le 80 ]; do echo \"HIST_ROW_$i\"; i=$((i+1)); done; printf 'DONE_%s\\n' SCROLL\r",
        )
        .expect("emit 80 lines to PTY");

    assert!(
        session.wait_for_output(Duration::from_secs(5), |snap| {
            snap.visible_text().contains("DONE_SCROLL")
        }),
        "expected DONE_SCROLL, got:\n{}",
        session.snapshot().visible_text()
    );

    assert!(
        session.history_size() >= 70,
        "expected scrollback history >= 70 lines, got {}",
        session.history_size()
    );
    assert_eq!(session.display_offset(), 0);
    assert!(!session.snapshot().visible_text().contains("HIST_ROW_1\n"));

    let top_offset = session.scroll_display(Scroll::Top);
    assert_eq!(top_offset, session.history_size());
    let top_snap = session.snapshot();
    assert_eq!(top_snap.display_offset, top_offset);
    assert_eq!(top_snap.top_line, -(top_offset as i32));
    assert!(
        top_snap.visible_text().contains("HIST_ROW_1"),
        "expected top scrollback to contain HIST_ROW_1, got:\n{}",
        top_snap.visible_text()
    );

    assert_eq!(session.scroll_display(Scroll::Bottom), 0);
    assert!(session.snapshot().visible_text().contains("DONE_SCROLL"));

    // Custom small scrollback limit caps history retention and dynamic apply_options shrinks/updates cursor.
    let capped = PtySession::spawn(test_pty_config(80, 5, 15)).expect("spawn capped PTY");

    capped
        .write_bytes(
            b"i=1; while [ \"$i\" -le 45 ]; do echo \"CAP_ROW_$i\"; i=$((i+1)); done; printf 'CAP_%s\\n' DONE\r",
        )
        .expect("emit 45 lines");

    assert!(capped.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("CAP_DONE")
    }));
    assert_eq!(capped.history_size(), 15);

    capped.apply_options(10, CursorShape::Beam);
    assert_eq!(capped.history_size(), 10);
    let snap = capped.snapshot();
    if let Some(cursor) = snap.cursor {
        assert_eq!(cursor.shape, CursorShape::Beam);
    }
}

#[test]
fn search_scrollback_finds_history_and_screen_matches_and_scrolls_to_match() {
    let session =
        PtySession::spawn(test_pty_config(80, 8, 500)).expect("spawn PTY for scrollback search");

    session
        .write_bytes(
            b"printf '%s_%s_%s\\n' ALPHA HIT 1; i=1; while [ \"$i\" -le 25 ]; do echo \"spacer_$i\"; i=$((i+1)); done; printf '%s_%s_%s\\n' ALPHA HIT 2; printf 'SEARCH_%s\\n' READY\r",
        )
        .expect("write search corpus");

    assert!(session.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("SEARCH_READY")
    }));

    assert!(session.search_scrollback("").is_empty());

    // Smart-case lowercase query matches both history hit and visible hit in chronological order.
    let matches: Vec<GridSearchMatch> = session.search_scrollback("alpha_hit_");
    assert_eq!(matches.len(), 2, "expected 2 matches, got {matches:?}");
    assert!(matches[0].start.line < 0, "first match should be in scrollback: {:?}", matches[0]);
    assert!(matches[1].start.line >= 0, "second match should be on live screen: {:?}", matches[1]);
    assert_eq!(matches[0].end.col - matches[0].start.col + 1, "ALPHA_HIT_".len());

    // Uppercase triggers case-sensitive smart-case search.
    assert!(session.search_scrollback("alpha_HIT_").is_empty());

    // Unclosed regex syntax falls back gracefully without crashing.
    let _ = session.search_scrollback("ALPHA(");

    // Jumping to the off-screen scrollback match scrolls it into the visible viewport.
    let offset = session.scroll_to_match(matches[0]);
    assert!(offset > 0);
    let scrolled_snap = session.snapshot();
    assert!(
        scrolled_snap
            .grid_to_viewport(matches[0].start.line, matches[0].start.col)
            .is_some(),
        "expected match {:?} visible at top_line={}",
        matches[0],
        scrolled_snap.top_line
    );
    assert!(scrolled_snap.visible_text().contains("ALPHA_HIT_1"));
}

#[test]
fn url_detection_handles_punctuation_wrapping_cjk_offset_and_cell_hit_testing() {
    let session = PtySession::spawn(test_pty_config(40, 24, 200)).expect("spawn 40-col PTY");

    session
        .write_bytes(
            "printf 'See https://example.com/docs_(v1). and (http://127.0.0.1:8080/ok)!\\n'\r\
             printf 'https://example.org/alpha/beta/gamma/delta/epsilon/wrapped\\n'\r\
             printf '日本 https://rust-lang.org/learn\\n'\r\
             printf 'URL_%s\\n' READY\r"
                .as_bytes(),
        )
        .expect("emit URLs to PTY");

    assert!(session.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("URL_READY")
    }));

    let snap = session.snapshot();
    let urls = detect_urls_in_snapshot(&snap);
    let url_strings: Vec<&str> = urls.iter().map(|u| u.url.as_str()).collect();

    assert!(
        url_strings.contains(&"https://example.com/docs_(v1)"),
        "expected balanced paren URL without trailing period, got {url_strings:?}"
    );
    assert!(
        url_strings.contains(&"http://127.0.0.1:8080/ok"),
        "expected parenthesized URL without outer parens, got {url_strings:?}"
    );
    assert!(
        url_strings.contains(&"https://example.org/alpha/beta/gamma/delta/epsilon/wrapped"),
        "expected soft-wrapped multi-row URL, got {url_strings:?}"
    );
    assert!(
        url_strings.contains(&"https://rust-lang.org/learn"),
        "expected post-CJK URL, got {url_strings:?}"
    );

    // Hit-test wrapped URL on its second row (use rfind to hit printed output line).
    let wrapped = urls
        .iter()
        .rfind(|u| u.url == "https://example.org/alpha/beta/gamma/delta/epsilon/wrapped")
        .expect("wrapped URL present");
    assert_eq!(wrapped.spans.len(), 2, "expected 2 spans for wrapped URL");
    let second_span = wrapped.spans[1];
    assert_eq!(
        url_at_cell(&snap, second_span.start_col, second_span.row).map(|u| u.url),
        Some(wrapped.url.clone())
    );

    // Hit-test post-CJK output line where '$ 日本 ' occupies columns 0..7 before 'https://...'.
    let cjk_row = (0..snap.rows as usize)
        .rfind(|&r| {
            let line = snap.row_text(r);
            line.contains("日本 https://rust-lang.org/learn") && !line.contains("printf")
        })
        .unwrap_or_else(|| panic!("post-CJK output row missing in:\n{}", snap.visible_text()));
    let cjk_url = urls
        .iter()
        .find(|u| u.url == "https://rust-lang.org/learn" && u.spans.iter().any(|s| s.row == cjk_row))
        .expect("post-CJK URL present on printed row");
    let cjk_span = cjk_url.spans[0];
    let prefix_width: usize = snap
        .row_text(cjk_row)
        .split("https://")
        .next()
        .unwrap()
        .chars()
        .map(|ch| if ch == '日' || ch == '本' { 2 } else { 1 })
        .sum();
    assert_eq!(cjk_span.start_col, prefix_width, "wide CJK chars must offset URL cell col");
    assert!(cjk_span.start_col >= 5);
    assert_eq!(
        url_at_cell(&snap, cjk_span.start_col, cjk_span.row).map(|u| u.url),
        Some(cjk_url.url.clone())
    );
    assert!(url_at_cell(&snap, 0, cjk_span.row).is_none());
}

#[test]
fn selection_extracts_dragged_text_and_ignores_bare_clicks() {
    let session = PtySession::spawn(test_pty_config(80, 10, 200)).expect("spawn PTY for selection");

    session
        .write_bytes(b"printf 'PICK_%s_HERE\\n' TARGET\r")
        .expect("write selection line");
    assert!(session.wait_for_output(Duration::from_secs(5), |s| {
        s.visible_text().contains("PICK_TARGET_HERE")
    }));

    let snap = session.snapshot();
    let row = (0..snap.rows as usize)
        .find(|&r| snap.row_text(r).contains("PICK_TARGET_HERE"))
        .unwrap_or_else(|| panic!("find output row in:\n{}", snap.visible_text()));
    let start_col = snap
        .row_text(row)
        .find("PICK_TARGET_HERE")
        .expect("substring col")
        + "PICK_".len();

    // Bare click without drag produces no copied text.
    session.start_selection(start_col, row, Side::Left, SelectionType::Simple);
    assert_eq!(session.finish_selection(), None);

    // Drag across "TARGET" (6 chars).
    session.start_selection(start_col, row, Side::Left, SelectionType::Simple);
    session.update_selection(start_col + 5, row, Side::Right);
    assert_eq!(session.finish_selection().as_deref(), Some("TARGET"));

    let selected_snap = session.snapshot();
    assert!(selected_snap.is_cell_selected(start_col, row));
    assert!(selected_snap.is_cell_selected(start_col + 5, row));
    assert!(!selected_snap.is_cell_selected(start_col - 1, row));
    assert!(!selected_snap.is_cell_selected(start_col + 6, row));

    session.clear_selection();
    assert!(!session.snapshot().is_cell_selected(5, row));
}

#[test]
fn shell_escape_formats_plain_special_and_unicode_paths_safely() {
    assert_eq!(
        shell_escape_path(Path::new("/tmp/plain-file_01.txt")),
        "/tmp/plain-file_01.txt"
    );
    assert_eq!(
        shell_escape_path(Path::new("/tmp/My File (1).txt")),
        "'/tmp/My File (1).txt'"
    );
    assert_eq!(
        shell_escape_path(Path::new("/tmp/it's $HOME `cmd`.sh")),
        "'/tmp/it'\\''s $HOME `cmd`.sh'"
    );
    assert_eq!(
        shell_escape_path(Path::new("/tmp/tiếng việt.txt")),
        "'/tmp/tiếng việt.txt'"
    );
    assert_eq!(shell_escape_path(Path::new("")), "''");
    assert_eq!(
        shell_escape_paths([
            Path::new("/tmp/a.txt"),
            Path::new("/tmp/with space.txt"),
            Path::new("/tmp/quote's"),
        ]),
        "/tmp/a.txt '/tmp/with space.txt' '/tmp/quote'\\''s'"
    );
}

#[test]
fn link_detection_mouse_encoding_and_highlight_rules_work() {
    let buffer = TerminalBuffer::new(80, 6, 100);
    buffer.feed_bytes(
        b"ERROR at crates/termewnal-ui/src/app.rs:42:10 and https://example.com/a/b\r\nfile:///tmp/out.log:9",
    );
    let snap = buffer.snapshot();

    let links = detect_links_in_snapshot(&snap);
    assert!(links.iter().any(|l| {
        matches!(
            &l.kind,
            DetectedLinkKind::LocalFile {
                path,
                line: Some(42),
                col: Some(10),
            } if path == "crates/termewnal-ui/src/app.rs"
        )
    }));
    assert!(links.iter().any(|l| {
        matches!(
            &l.kind,
            DetectedLinkKind::LocalFile {
                path,
                line: Some(9),
                col: None,
            } if path == "/tmp/out.log"
        )
    }));
    assert!(links.iter().any(|l| {
        l.kind == DetectedLinkKind::WebUrl && l.target == "https://example.com/a/b"
    }));
    assert!(link_at_cell(&snap, 12, 0).is_some());

    // Mouse event encoding (SGR 1006 & normal X10).
    assert_eq!(
        encode_mouse_event(
            PtyMouseButton::Left,
            PtyMouseEventKind::Press,
            4,
            2,
            false,
            false,
            false,
            true
        ),
        b"\x1b[<0;5;3M"
    );
    assert_eq!(
        encode_mouse_event(
            PtyMouseButton::Left,
            PtyMouseEventKind::Release,
            4,
            2,
            false,
            false,
            false,
            true
        ),
        b"\x1b[<0;5;3m"
    );
    assert_eq!(
        encode_mouse_event(
            PtyMouseButton::WheelUp,
            PtyMouseEventKind::Press,
            0,
            0,
            false,
            false,
            true,
            true
        ),
        b"\x1b[<80;1;1M"
    );
    assert_eq!(
        encode_mouse_event(
            PtyMouseButton::Right,
            PtyMouseEventKind::Release,
            1,
            1,
            false,
            false,
            false,
            false
        ),
        vec![0x1b, b'[', b'M', 35, 34, 34]
    );

    // Highlight rule matching.
    let compiled = compile_highlight_rules(&[HighlightRule {
        id: "err".to_string(),
        name: "Errors".to_string(),
        pattern: "error".to_string(),
        foreground: Some("#ff4444".to_string()),
        background: None,
        bold: true,
        enabled: true,
        case_insensitive: true,
    }]);
    let grid = compute_highlight_grid(&snap, &compiled);
    let err_cell = grid[0].expect("col 0 of row 0 matched ERROR");
    assert_eq!(err_cell.fg, Some(RgbColor::new(0xff, 0x44, 0x44)));
    assert!(err_cell.bold);
    assert!(grid[6].is_none());
}
