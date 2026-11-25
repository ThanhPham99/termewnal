use termewnal_ui::AppState;

fn main() -> iced::Result {
    iced::daemon(AppState::boot_daemon, AppState::update, AppState::view)
        .title(AppState::title)
        .theme(AppState::theme)
        .subscription(AppState::subscription)
        .run()
}
