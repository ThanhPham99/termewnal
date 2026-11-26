use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;
use termewnal_protocols::zmodem::{
    HeaderEncoding, ZBIN, ZBIN32, ZCRCW, ZDATA, ZFILE, ZHEX, ZRINIT, ZRQINIT, ZmodemDirection,
    ZmodemEngine, ZmodemEvent, ZmodemHeader, crc16_xmodem, crc32_ieee, encode_subpacket,
    resolve_safe_download_path, sanitize_remote_filename,
};

#[test]
fn zmodem_crc_headers_subpackets_sanitization_and_idle_passthrough() {
    assert_ne!(crc16_xmodem(b"123456789"), 0);
    assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);

    let sub = encode_subpacket(&[0x18, 0x11, 0x13, b'@', b'\r', b'A'], ZCRCW, true);
    assert!(sub.contains(&0x18));
    let _ = (ZBIN, ZBIN32, ZHEX, ZFILE, ZDATA);

    let hdr = ZmodemHeader::with_pos(HeaderEncoding::ZBin32, ZRINIT, 0x1234_5678);
    assert_eq!(hdr.pos(), 0x1234_5678);

    assert_eq!(sanitize_remote_filename("../../etc/passwd"), "passwd");
    assert_eq!(sanitize_remote_filename(r"C:\tmp\fw.bin"), "fw.bin");
    assert_eq!(sanitize_remote_filename(".."), "zmodem_download.bin");

    let temp = TempDir::new().unwrap();
    let p1 = resolve_safe_download_path(temp.path(), "log.txt");
    assert_eq!(p1, temp.path().join("log.txt"));
    fs::write(&p1, b"existing").unwrap();
    let p2 = resolve_safe_download_path(temp.path(), "log.txt");
    assert_eq!(p2, temp.path().join("log (1).txt"));
    fs::write(&p2, b"second").unwrap();
    let p3 = resolve_safe_download_path(temp.path(), "log.txt");
    assert_eq!(p3, temp.path().join("log (2).txt"));

    // Literal `*` in normal shell output must not stall terminal passthrough.
    let mut idle = ZmodemEngine::new(temp.path());
    let s1 = idle.feed_wire(b"ls *foo* bar\r\n").unwrap();
    assert_eq!(s1.terminal_output, b"ls *foo* bar\r\n");
    assert!(!idle.is_active());

    // Remote `rz` emitting `ZRINIT` notifies UI cleanly.
    let zrinit = ZmodemHeader::with_pos(HeaderEncoding::ZHex, ZRINIT, 0).encode();
    let s2 = idle.feed_wire(&zrinit).unwrap();
    assert!(s2.events.contains(&ZmodemEvent::RemoteReadyToReceive));
}

#[test]
fn zmodem_peer_to_peer_transfers_multiple_files_across_fragmented_wire() {
    let send_dir = TempDir::new().unwrap();
    let recv_dir = TempDir::new().unwrap();

    // Include binary control characters (ZDLE/XON/XOFF/@CR) across multi-chunk payload > SEND_CHUNK_SIZE.
    let mut payload_a = Vec::with_capacity(9500);
    for i in 0..9500_usize {
        let byte = match i % 7 {
            0 => 0x18,
            1 => 0x11,
            2 => 0x13,
            3 => b'@',
            4 => b'\r',
            5 => 0x90,
            _ => (i & 0xff) as u8,
        };
        payload_a.push(byte);
    }
    let src_a = send_dir.path().join("firmware.bin");
    fs::write(&src_a, &payload_a).unwrap();

    let src_b = send_dir.path().join("empty.txt");
    fs::write(&src_b, b"").unwrap();

    let mut sender = ZmodemEngine::new(send_dir.path());
    let mut receiver = ZmodemEngine::new(recv_dir.path());

    let mut sender_events = Vec::new();
    let mut receiver_events = Vec::new();

    // Start `sz` transfer -> feeds into receiver -> loops wire exchanges in irregular fragment sizes.
    let init = sender.start_send(vec![src_a.clone(), src_b.clone()]).unwrap();
    sender_events.extend(init.events);
    let mut to_receiver = init.wire_output;
    let mut to_sender = Vec::new();

    // Prepend normal terminal prompt before `ZRQINIT` to prove clean prompt passthrough + transition!
    let mut initial_wire = b"remote$ sz firmware.bin empty.txt\r\n".to_vec();
    initial_wire.extend_from_slice(&to_receiver);
    to_receiver = initial_wire;
    let mut receiver_term_out = Vec::new();

    for round in 0..64 {
        if to_receiver.is_empty() && to_sender.is_empty() {
            break;
        }

        if !to_receiver.is_empty() {
            let wire = std::mem::take(&mut to_receiver);
            for chunk in wire.chunks(13) {
                let step = receiver.feed_wire(chunk).unwrap();
                receiver_term_out.extend_from_slice(&step.terminal_output);
                to_sender.extend_from_slice(&step.wire_output);
                receiver_events.extend(step.events);
            }
        }

        if !to_sender.is_empty() {
            let wire = std::mem::take(&mut to_sender);
            for chunk in wire.chunks(7) {
                let step = sender.feed_wire(chunk).unwrap();
                to_receiver.extend_from_slice(&step.wire_output);
                sender_events.extend(step.events);
            }
        }

        assert!(round < 63, "Zmodem handshake did not converge");
    }

    assert!(!sender.is_active());
    assert!(!receiver.is_active());
    assert_eq!(receiver_term_out, b"remote$ sz firmware.bin empty.txt\r\n");

    assert_eq!(fs::read(recv_dir.path().join("firmware.bin")).unwrap(), payload_a);
    assert_eq!(fs::read(recv_dir.path().join("empty.txt")).unwrap(), b"");
    assert!(sender_events.contains(&ZmodemEvent::SessionFinished));
    assert!(receiver_events.contains(&ZmodemEvent::SessionFinished));

    let completed_count = receiver_events
        .iter()
        .filter(|e| matches!(e, ZmodemEvent::TransferCompleted { .. }))
        .count();
    assert_eq!(completed_count, 2);

    // Send firmware.bin a second time to verify conflict-safe auto-renaming (`firmware (1).bin`).
    let init2 = sender.start_send(vec![src_a]).unwrap();
    let mut to_recv2 = init2.wire_output;
    let mut to_send2 = Vec::new();
    while !to_recv2.is_empty() || !to_send2.is_empty() {
        if !to_recv2.is_empty() {
            let w = std::mem::take(&mut to_recv2);
            let st = receiver.feed_wire(&w).unwrap();
            to_send2.extend(st.wire_output);
        }
        if !to_send2.is_empty() {
            let w = std::mem::take(&mut to_send2);
            let st = sender.feed_wire(&w).unwrap();
            to_recv2.extend(st.wire_output);
        }
    }
    assert_eq!(
        fs::read(recv_dir.path().join("firmware (1).bin")).unwrap(),
        payload_a
    );

    // Verify abort cancels active receiver back to Idle cleanly.
    let zrq = ZmodemHeader::with_pos(HeaderEncoding::ZHex, ZRQINIT, 0).encode();
    let _ = receiver.feed_wire(&zrq).unwrap();
    assert!(receiver.is_active());
    let cancel_step = sender.cancel();
    let abort_step = receiver.feed_wire(&cancel_step.wire_output).unwrap();
    assert!(!receiver.is_active());
    assert!(abort_step
        .events
        .iter()
        .any(|e| matches!(e, ZmodemEvent::Error(_))));
    let _ = (ZmodemDirection::Send, PathBuf::new());
}
