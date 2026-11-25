use termewnal_ui::{
    ClosePaneOutcome, CycleDirection, FocusDirection, MAX_SPLIT_RATIO, MIN_SPLIT_RATIO,
    SessionId, SplitAxis, Workspace,
};

#[test]
fn tabs_can_be_created_switched_reordered_and_closed_cleanly() {
    let mut ws = Workspace::new();
    assert_eq!(ws.tab_count(), 0);
    assert_eq!(ws.active_tab_id(), None);

    let (t1, _p1) = ws.create_tab(SessionId(1), Some("Shell 1".to_string()));
    let (t2, _p2) = ws.create_tab(SessionId(2), Some("Shell 2".to_string()));
    let (t3, _p3) = ws.create_tab(SessionId(3), None);

    assert_eq!(ws.tab_count(), 3);
    assert_eq!(ws.active_tab_id(), Some(t3));
    assert_eq!(ws.tab_ids(), vec![t1, t2, t3]);

    assert!(ws.select_tab(t1));
    assert_eq!(ws.active_tab_id(), Some(t1));

    assert_eq!(ws.cycle_tab(CycleDirection::Next), Some(t2));
    assert_eq!(ws.cycle_tab(CycleDirection::Next), Some(t3));
    assert_eq!(ws.cycle_tab(CycleDirection::Next), Some(t1));
    assert_eq!(ws.cycle_tab(CycleDirection::Previous), Some(t3));

    assert!(ws.reorder_tab(t3, 0));
    assert_eq!(ws.tab_ids(), vec![t3, t1, t2]);
    assert_eq!(ws.active_tab_id(), Some(t3));

    assert!(ws.move_tab_by(t1, 1));
    assert_eq!(ws.tab_ids(), vec![t3, t2, t1]);

    // Closing the active tab selects its adjacent neighbor and returns its sessions.
    let closed_sessions = ws.close_tab(t3).expect("t3 existed");
    assert_eq!(closed_sessions, vec![SessionId(3)]);
    assert_eq!(ws.tab_ids(), vec![t2, t1]);
    assert_eq!(ws.active_tab_id(), Some(t2));

    assert_eq!(ws.close_tab(t1), Some(vec![SessionId(1)]));
    assert_eq!(ws.active_tab_id(), Some(t2));

    assert_eq!(ws.close_tab(t2), Some(vec![SessionId(2)]));
    assert_eq!(ws.tab_count(), 0);
    assert_eq!(ws.active_tab_id(), None);
}

#[test]
fn binary_split_tree_inserts_resizes_and_collapses_cleanly_on_pane_close() {
    let mut ws = Workspace::new();
    let (t1, p1) = ws.create_tab(SessionId(10), None);

    let (split_v, p2) = ws
        .split_pane(t1, p1, SplitAxis::Vertical, SessionId(11))
        .expect("split p1 vertically");
    assert_eq!(ws.tab(t1).unwrap().focused_pane, p2);
    assert_eq!(ws.tab(t1).unwrap().root.pane_count(), 2);
    assert_eq!(ws.tab(t1).unwrap().root.pane_ids(), vec![p1, p2]);
    assert_eq!(
        ws.tab(t1).unwrap().root.find_split(split_v),
        Some((SplitAxis::Vertical, 0.5))
    );

    let (split_h, p3) = ws
        .split_pane(t1, p2, SplitAxis::Horizontal, SessionId(12))
        .expect("split p2 horizontally");
    assert_eq!(ws.tab(t1).unwrap().focused_pane, p3);
    assert_eq!(ws.tab(t1).unwrap().root.pane_count(), 3);
    assert_eq!(ws.tab(t1).unwrap().root.pane_ids(), vec![p1, p2, p3]);
    assert_eq!(
        ws.tab(t1).unwrap().root.find_split(split_h),
        Some((SplitAxis::Horizontal, 0.5))
    );

    assert!(ws.resize_split(t1, split_v, 0.32));
    assert!((ws.tab(t1).unwrap().root.find_split(split_v).unwrap().1 - 0.32).abs() < 1e-6);

    assert!(ws.resize_split(t1, split_v, 0.01));
    assert_eq!(
        ws.tab(t1).unwrap().root.find_split(split_v).unwrap().1,
        MIN_SPLIT_RATIO
    );

    assert!(ws.resize_split(t1, split_v, 0.99));
    assert_eq!(
        ws.tab(t1).unwrap().root.find_split(split_v).unwrap().1,
        MAX_SPLIT_RATIO
    );

    assert!(!ws.resize_split(t1, split_v, f32::NAN));
    assert_eq!(
        ws.tab(t1).unwrap().root.find_split(split_v).unwrap().1,
        MAX_SPLIT_RATIO
    );

    // Close p2 (inside split_h); split_h collapses into p3 while split_v remains intact.
    assert_eq!(
        ws.close_pane(t1, p2),
        Some(ClosePaneOutcome::ClosedPane {
            session_id: SessionId(11),
            focused_pane: p3,
        })
    );
    let tab = ws.tab(t1).unwrap();
    assert_eq!(tab.root.pane_count(), 2);
    assert_eq!(tab.root.pane_ids(), vec![p1, p3]);
    assert_eq!(tab.focused_pane, p3);
    assert_eq!(tab.root.find_split(split_h), None);
    assert_eq!(
        tab.root.find_split(split_v),
        Some((SplitAxis::Vertical, MAX_SPLIT_RATIO))
    );

    // Close p3; split_v collapses into single leaf p1.
    assert_eq!(
        ws.close_pane(t1, p3),
        Some(ClosePaneOutcome::ClosedPane {
            session_id: SessionId(12),
            focused_pane: p1,
        })
    );
    assert_eq!(ws.tab(t1).unwrap().root.pane_count(), 1);
    assert_eq!(ws.tab(t1).unwrap().focused_pane, p1);

    // Closing the final pane closes the tab.
    assert_eq!(
        ws.close_pane(t1, p1),
        Some(ClosePaneOutcome::ClosedTab {
            session_id: SessionId(10),
        })
    );
    assert_eq!(ws.tab_count(), 0);
}

#[test]
fn focus_moves_directionally_and_cyclically_across_nested_splits() {
    let mut ws = Workspace::new();
    let (t1, p1) = ws.create_tab(SessionId(1), None);
    let (_s1, p2) = ws
        .split_pane(t1, p1, SplitAxis::Vertical, SessionId(2))
        .unwrap();
    let (_s2, p3) = ws
        .split_pane(t1, p2, SplitAxis::Horizontal, SessionId(3))
        .unwrap();
    let (_s3, p4) = ws
        .split_pane(t1, p1, SplitAxis::Horizontal, SessionId(4))
        .unwrap();

    // Spatial layout:
    //  p1 (top-left)  | p2 (top-right)
    //  p4 (bot-left)  | p3 (bot-right)
    assert!(ws.focus_pane(t1, p1));
    assert_eq!(ws.move_focus(t1, FocusDirection::Left), None);
    assert_eq!(ws.move_focus(t1, FocusDirection::Up), None);

    assert_eq!(ws.move_focus(t1, FocusDirection::Right), Some(p2));
    assert_eq!(ws.move_focus(t1, FocusDirection::Down), Some(p3));
    assert_eq!(ws.move_focus(t1, FocusDirection::Left), Some(p4));
    assert_eq!(ws.move_focus(t1, FocusDirection::Up), Some(p1));

    // Cyclic navigation visits [p1, p4, p2, p3] in DFS order and wraps.
    assert_eq!(ws.cycle_focus(t1, CycleDirection::Next), Some(p4));
    assert_eq!(ws.cycle_focus(t1, CycleDirection::Next), Some(p2));
    assert_eq!(ws.cycle_focus(t1, CycleDirection::Next), Some(p3));
    assert_eq!(ws.cycle_focus(t1, CycleDirection::Next), Some(p1));
    assert_eq!(ws.cycle_focus(t1, CycleDirection::Previous), Some(p3));

    // Focusing by SessionId works even across tabs.
    let (t2, p5) = ws.create_tab(SessionId(5), None);
    assert_eq!(ws.active_tab_id(), Some(t2));
    assert_eq!(ws.focus_session(SessionId(2)), Some((t1, p2)));
    assert_eq!(ws.active_tab_id(), Some(t1));
    assert_eq!(ws.tab(t1).unwrap().focused_pane, p2);
    assert_eq!(ws.focus_session(SessionId(5)), Some((t2, p5)));
}

#[test]
fn pane_zoom_preserves_split_geometry_and_broadcast_input_scopes_to_tab() {
    let mut ws = Workspace::new();
    let (t1, p1) = ws.create_tab(SessionId(1), None);
    let (s1, p2) = ws
        .split_pane(t1, p1, SplitAxis::Vertical, SessionId(2))
        .unwrap();
    let (s2, _p3) = ws
        .split_pane(t1, p2, SplitAxis::Horizontal, SessionId(3))
        .unwrap();

    assert!(ws.resize_split(t1, s1, 0.33));
    assert!(ws.resize_split(t1, s2, 0.72));
    assert!(ws.focus_pane(t1, p2));

    let expected_tree = ws.tab(t1).unwrap().root.clone();

    assert_eq!(ws.toggle_zoom(t1), Some(Some(p2)));
    assert_eq!(ws.tab(t1).unwrap().zoomed_pane, Some(p2));
    assert_eq!(ws.tab(t1).unwrap().root, expected_tree);

    assert_eq!(ws.toggle_zoom(t1), Some(None));
    assert_eq!(ws.tab(t1).unwrap().zoomed_pane, None);
    assert_eq!(ws.tab(t1).unwrap().root, expected_tree);

    // Splitting clears an active zoom so the newly split pane is visible.
    assert_eq!(ws.toggle_zoom(t1), Some(Some(p2)));
    let (_s3, _p4) = ws
        .split_pane(t1, p2, SplitAxis::Vertical, SessionId(4))
        .unwrap();
    assert_eq!(ws.tab(t1).unwrap().zoomed_pane, None);

    // Broadcast Input is scoped per-tab.
    let (t2, _p5) = ws.create_tab(SessionId(5), None);
    assert_eq!(ws.toggle_broadcast_input(t1), Some(true));
    assert!(ws.tab(t1).unwrap().broadcast_input);
    assert!(!ws.tab(t2).unwrap().broadcast_input);

    assert_eq!(
        ws.input_targets_for_session(SessionId(2)),
        vec![SessionId(1), SessionId(2), SessionId(4), SessionId(3)]
    );
    assert_eq!(
        ws.input_targets_for_session(SessionId(5)),
        vec![SessionId(5)]
    );

    assert!(ws.set_broadcast_input(t1, false));
    assert!(!ws.tab(t1).unwrap().broadcast_input);
    assert_eq!(
        ws.input_targets_for_session(SessionId(2)),
        vec![SessionId(2)]
    );
}
