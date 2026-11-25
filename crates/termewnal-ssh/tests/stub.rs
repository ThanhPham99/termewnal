use termewnal_ssh::{SshError, TransportStatus};

#[test]
fn transport_status_defaults_to_disconnected_and_ssh_error_formats_cleanly() {
    let status = TransportStatus::default();
    assert_eq!(status, TransportStatus::Disconnected);
    assert_eq!(status.label(), "Disconnected");

    let err = SshError::NotImplemented("Ticket 07");
    assert!(err.to_string().contains("Ticket 07"));
}
