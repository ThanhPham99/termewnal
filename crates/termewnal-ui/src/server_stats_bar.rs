use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::Widget;
use iced::widget::{column, container, progress_bar, row, space, text};
use iced::{
    Alignment, Background, Border, Color, Element, Length, Padding, Rectangle, Shadow, Size, Theme,
    mouse,
};
use termewnal_core::ThemePalette;
use termewnal_ssh::{ServerStatsSnapshot, TransportStatus};

use crate::app::Message;
use crate::ui_theme::{
    ChromePalette, colored_progress_style, hairline_h, mono_font, ui_font, ui_font_semibold,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatGlyph {
    Cpu,
    Ram,
    Disk,
    Network,
}

struct ServerStatIcon {
    glyph: StatGlyph,
    color: Color,
}

impl ServerStatIcon {
    fn new<'a>(glyph: StatGlyph, color: Color) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self { glyph, color })
    }
}

impl Widget<Message, Theme, iced::Renderer> for ServerStatIcon {
    fn children(&self) -> Vec<Tree> {
        Vec::new()
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::stateless()
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(13.0), Length::Fixed(13.0))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.resolve(Length::Fixed(13.0), Length::Fixed(13.0), Size::ZERO);
        layout::Node::new(size)
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if !bounds.intersects(viewport) {
            return;
        }
        let x0 = bounds.x.round() + 1.0;
        let y0 = bounds.y.round() + 1.0;
        let c = self.color;

        match self.glyph {
            StatGlyph::Cpu => {
                stroke_rect(renderer, Rectangle::new([x0 + 2.0, y0 + 2.0].into(), [7.0, 7.0].into()), c);
                fill_quad(renderer, Rectangle::new([x0 + 4.0, y0 + 4.0].into(), [3.0, 3.0].into()), c);
                for offset in [3.5, 6.5] {
                    fill_quad(renderer, Rectangle::new([x0 + offset, y0].into(), [1.0, 2.0].into()), c);
                    fill_quad(renderer, Rectangle::new([x0 + offset, y0 + 9.0].into(), [1.0, 2.0].into()), c);
                    fill_quad(renderer, Rectangle::new([x0, y0 + offset].into(), [2.0, 1.0].into()), c);
                    fill_quad(renderer, Rectangle::new([x0 + 9.0, y0 + offset].into(), [2.0, 1.0].into()), c);
                }
            }
            StatGlyph::Ram => {
                stroke_rect(renderer, Rectangle::new([x0, y0 + 2.0].into(), [11.0, 6.0].into()), c);
                for px in [2.0, 5.0, 8.0] {
                    fill_quad(renderer, Rectangle::new([x0 + px, y0 + 8.0].into(), [1.0, 2.5].into()), c);
                    fill_quad(renderer, Rectangle::new([x0 + px, y0 + 4.0].into(), [1.5, 2.0].into()), c);
                }
            }
            StatGlyph::Disk => {
                stroke_rect(renderer, Rectangle::new([x0 + 1.0, y0 + 1.0].into(), [9.0, 9.0].into()), c);
                fill_quad(renderer, Rectangle::new([x0 + 1.0, y0 + 6.0].into(), [9.0, 1.0].into()), c);
                fill_quad(renderer, Rectangle::new([x0 + 3.0, y0 + 8.0].into(), [1.5, 1.0].into()), c);
            }
            StatGlyph::Network => {
                fill_quad(renderer, Rectangle::new([x0 + 4.0, y0 + 1.0].into(), [3.0, 2.5].into()), c);
                fill_quad(renderer, Rectangle::new([x0 + 5.0, y0 + 3.5].into(), [1.0, 2.0].into()), c);
                fill_quad(renderer, Rectangle::new([x0 + 1.5, y0 + 5.5].into(), [8.0, 1.0].into()), c);
                fill_quad(renderer, Rectangle::new([x0 + 2.0, y0 + 6.5].into(), [1.0, 1.5].into()), c);
                fill_quad(renderer, Rectangle::new([x0 + 8.0, y0 + 6.5].into(), [1.0, 1.5].into()), c);
                fill_quad(renderer, Rectangle::new([x0 + 1.0, y0 + 8.0].into(), [3.0, 2.5].into()), c);
                fill_quad(renderer, Rectangle::new([x0 + 7.0, y0 + 8.0].into(), [3.0, 2.5].into()), c);
            }
        }
    }
}

fn fill_quad(renderer: &mut iced::Renderer, bounds: Rectangle, color: Color) {
    use iced::advanced::renderer::Renderer as _;
    renderer.fill_quad(
        Quad {
            bounds,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(color),
    );
}

fn stroke_rect(renderer: &mut iced::Renderer, bounds: Rectangle, color: Color) {
    use iced::advanced::renderer::Renderer as _;
    renderer.fill_quad(
        Quad {
            bounds,
            border: Border {
                color,
                width: 1.0,
                radius: 1.0.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(Color::TRANSPARENT),
    );
}

fn vertical_divider<'a>(chrome: ChromePalette) -> Element<'a, Message, Theme, iced::Renderer> {
    container(space::vertical().width(1).height(12))
        .width(1)
        .height(12)
        .style(move |_| container::Style {
            background: Some(Background::Color(chrome.hairline)),
            ..Default::default()
        })
        .into()
}

/// Formats a utilization percentage (`18%`, or `--%` while awaiting the first sample).
pub fn format_stat_percent(pct: Option<f32>) -> String {
    match pct {
        Some(val) if val.is_finite() => format!("{:.0}%", val.clamp(0.0, 100.0)),
        _ => "--%".to_string(),
    }
}

/// Formats network throughput in compact units matching `414 K/s`, `12.4 M/s`, or `--`.
pub fn format_net_speed(bytes_per_sec: Option<u64>) -> String {
    let Some(bps) = bytes_per_sec else {
        return "--".to_string();
    };
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bps as f64;
    if bps < 1024 {
        format!("{bps} B/s")
    } else if b < MIB {
        format!("{:.0} K/s", (b / KIB).round())
    } else if b < GIB {
        format!("{:.1} M/s", b / MIB)
    } else {
        format!("{:.2} G/s", b / GIB)
    }
}

fn meter_block<'a>(
    glyph: StatGlyph,
    label: &'static str,
    percent: Option<f32>,
    fill_color: Color,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let val = percent.unwrap_or(0.0).clamp(0.0, 100.0);
    let active_color = if val >= 92.0 {
        chrome.danger
    } else {
        fill_color
    };

    row![
        ServerStatIcon::new(glyph, chrome.text_muted),
        text(label)
            .size(11.0)
            .font(ui_font())
            .color(chrome.text_secondary),
        progress_bar(0.0..=100.0, val)
            .girth(5.0)
            .length(52.0)
            .style(colored_progress_style(chrome, active_color)),
        container(
            text(format_stat_percent(percent))
                .size(11.0)
                .font(ui_font_semibold())
                .color(if percent.is_some() {
                    chrome.text_primary
                } else {
                    chrome.text_muted
                }),
        )
        .width(Length::Fixed(32.0))
        .align_x(Alignment::End),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

/// Renders the compact live server resource stats strip (`CPU`, `RAM`, `DISK`, `NET`)
/// docked at the bottom of an SSH session pane.
pub fn render_server_stats_bar<'a>(
    stats: Option<&ServerStatsSnapshot>,
    transport_status: &TransportStatus,
    palette: &ThemePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let chrome = ChromePalette::from_palette(palette);
    let stale_or_connecting = !matches!(transport_status, TransportStatus::Connected { .. });

    let cpu_pct = stats.and_then(|s| s.cpu_percent);
    let ram_pct = stats.and_then(|s| s.ram_percent);
    let disk_pct = stats.and_then(|s| s.disk_percent);
    let rx_bps = stats.and_then(|s| s.net_rx_bytes_per_sec);
    let tx_bps = stats.and_then(|s| s.net_tx_bytes_per_sec);

    let disk_blue = Color::from_rgb8(0x34, 0x98, 0xdb);

    let left_metrics = row![
        meter_block(StatGlyph::Cpu, "CPU", cpu_pct, chrome.success, chrome),
        vertical_divider(chrome),
        meter_block(StatGlyph::Ram, "RAM", ram_pct, chrome.warning, chrome),
        vertical_divider(chrome),
        meter_block(StatGlyph::Disk, "DISK", disk_pct, disk_blue, chrome),
        vertical_divider(chrome),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .wrap()
    .vertical_spacing(4);

    let rx_color = if rx_bps.is_some() && !stale_or_connecting {
        chrome.success
    } else if rx_bps.is_some() {
        Color {
            a: 0.70,
            ..chrome.success
        }
    } else {
        chrome.text_muted
    };
    let tx_color = if tx_bps.is_some() && !stale_or_connecting {
        chrome.danger
    } else if tx_bps.is_some() {
        Color {
            a: 0.70,
            ..chrome.danger
        }
    } else {
        chrome.text_muted
    };

    let net_rates = column![
        text(format!("↓ {}", format_net_speed(rx_bps)))
            .size(10.0)
            .font(mono_font())
            .color(rx_color),
        text(format!("↑ {}", format_net_speed(tx_bps)))
            .size(10.0)
            .font(mono_font())
            .color(tx_color),
    ]
    .spacing(0)
    .align_x(Alignment::End);

    let net_block = row![
        ServerStatIcon::new(StatGlyph::Network, chrome.text_muted),
        text("NET")
            .size(11.0)
            .font(ui_font())
            .color(chrome.text_secondary),
        net_rates,
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let bar_body = container(
        row![
            container(left_metrics).width(Length::Fill),
            net_block,
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .padding(Padding {
            top: 3.0,
            right: 12.0,
            bottom: 3.0,
            left: 12.0,
        }),
    )
    .width(Length::Fill)
    .clip(true)
    .style(move |_| container::Style {
        background: Some(Background::Color(chrome.bg_more)),
        ..Default::default()
    });

    column![hairline_h(chrome), bar_body]
        .width(Length::Fill)
        .into()
}
