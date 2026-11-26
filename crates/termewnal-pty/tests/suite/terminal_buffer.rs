use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use termewnal_pty::{NamedColor, PtyEvent, TermColor, TerminalBuffer, TerminalProgressState};

struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn terminal_buffer_feeds_ansi_bytes_resizes_tracks_events_and_routes_pty_write() {
    let buffer = TerminalBuffer::new(80, 24, 10_000);
    let mut rx = buffer
        .take_event_receiver()
        .expect("event receiver should be available");
    assert!(buffer.take_event_receiver().is_none());

    let sink_bytes = Arc::new(Mutex::new(Vec::new()));
    buffer.set_write_sink(Some(Box::new(SharedSink(Arc::clone(&sink_bytes)))));

    // Feed ANSI color + OSC title + DSR cursor position query (ESC[6n).
    buffer.feed_bytes(b"\x1b]2;ssh-host\x07hello \x1b[31mworld\x1b[0m\r\nline2\x1b[6n");

    let ready = buffer.wait_for_output(Duration::from_secs(2), |snap| {
        snap.visible_text().contains("hello world")
    });
    assert!(ready);

    let snap = buffer.snapshot();
    assert_eq!(snap.visible_text(), "hello world\nline2");
    let red_cell = snap.cell(6, 0).expect("cell inside viewport");
    assert_eq!(red_cell.c, 'w');
    assert_eq!(red_cell.fg, TermColor::Named(NamedColor::Red));

    // Verify DSR response written back to sink (`\x1b[2;6R` for row 2, col 6).
    assert_eq!(sink_bytes.lock().unwrap().as_slice(), b"\x1b[2;6R");

    // Verify resize updates grid dimensions.
    buffer.resize(40, 10);
    assert_eq!(buffer.dimensions(), (40, 10));
    let resized_snap = buffer.snapshot();
    assert_eq!(resized_snap.cols, 40);
    assert_eq!(resized_snap.rows, 10);

    // Verify exit marks buffer exited and delivers PtyEvent::Title + PtyEvent::Exited.
    buffer.mark_exited(Some(0));

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events.contains(&PtyEvent::Title("ssh-host".to_string())));
    assert!(events.contains(&PtyEvent::Output));
    assert!(events.contains(&PtyEvent::Exited(Some(0))));
}

#[test]
fn osc9_4_progress_clear_export_and_session_logging_work() {
    let temp = TempDir::new().expect("temp dir");
    let log_file = temp.path().join("session.log");
    let buffer = TerminalBuffer::new(40, 4, 200);
    let mut rx = buffer.take_event_receiver().expect("event receiver");

    buffer.start_logging(&log_file, true).expect("start logging");
    buffer.feed_bytes(b"\x1b]9;4;1;45\x07\x1b[32mline1\x1b[0m\r\nline2\r\nline3\r\nline4\r\nline5");
    assert_eq!(buffer.progress(), TerminalProgressState::Normal(45));

    buffer.feed_bytes(b"\x1b]9;4;3\x1b\\");
    assert_eq!(buffer.progress(), TerminalProgressState::Indeterminate);

    buffer.feed_bytes(b"\x1b]9;4;0\x07");
    assert_eq!(buffer.progress(), TerminalProgressState::Hidden);

    let exported = buffer.export_scrollback_text();
    assert!(exported.contains("line1"));
    assert!(exported.contains("line5"));

    let stopped = buffer.stop_logging().expect("stop logging");
    assert_eq!(stopped, log_file);
    let logged = std::fs::read_to_string(&log_file).expect("read log file");
    assert!(logged.contains("line1"));
    assert!(!logged.contains("\x1b[32m"));

    buffer.clear_screen_and_scrollback();
    assert_eq!(buffer.history_size(), 0);
    assert!(buffer.snapshot().visible_text().trim().is_empty());

    let mut progress_events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let PtyEvent::Progress(p) = ev {
            progress_events.push(p);
        }
    }
    assert_eq!(
        progress_events,
        vec![
            TerminalProgressState::Normal(45),
            TerminalProgressState::Indeterminate,
            TerminalProgressState::Hidden,
        ]
    );
}
