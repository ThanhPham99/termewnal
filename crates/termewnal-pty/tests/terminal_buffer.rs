use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use termewnal_pty::{NamedColor, PtyEvent, TermColor, TerminalBuffer};

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
