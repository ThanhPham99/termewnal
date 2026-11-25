use tempfile::TempDir;
use termewnal_core::{
    AppConfig, BuiltinTheme, ConfigPaths, CursorStyle, RgbColor, ThemePalette, load_config,
    save_config,
};

#[test]
fn builtin_themes_resolve_all_six_palettes_with_distinct_colors() {
    assert_eq!(BuiltinTheme::ALL.len(), 6);

    let cases = [
        ("Catppuccin", BuiltinTheme::Catppuccin, false),
        ("catppuccin mocha", BuiltinTheme::Catppuccin, false),
        ("Tokyo Night", BuiltinTheme::TokyoNight, false),
        ("tokyonight", BuiltinTheme::TokyoNight, false),
        ("Dracula", BuiltinTheme::Dracula, false),
        ("Nord", BuiltinTheme::Nord, false),
        ("Dark", BuiltinTheme::Dark, false),
        ("Light", BuiltinTheme::Light, true),
    ];

    for (input, expected_theme, expected_light) in cases {
        let resolved = BuiltinTheme::from_name(input).expect("known theme name must parse");
        assert_eq!(resolved, expected_theme);
        let palette = ThemePalette::for_name(input);
        assert_eq!(palette.theme, expected_theme);
        assert_eq!(palette.is_light, expected_light);
        assert_ne!(palette.background, palette.foreground);
        assert_ne!(palette.match_bg, palette.active_match_bg);
    }

    assert_eq!(BuiltinTheme::resolve("nonexistent"), BuiltinTheme::Dark);
    assert_eq!(BuiltinTheme::Catppuccin.palette().background, RgbColor::new(0x1e, 0x1e, 0x2e));
    assert_eq!(BuiltinTheme::TokyoNight.palette().background, RgbColor::new(0x1a, 0x1b, 0x26));
    assert_eq!(BuiltinTheme::Dracula.palette().background, RgbColor::new(0x28, 0x2a, 0x36));
    assert_eq!(BuiltinTheme::Nord.palette().background, RgbColor::new(0x2e, 0x34, 0x40));
    assert_eq!(BuiltinTheme::Light.palette().is_light, true);
}

#[test]
fn terminal_config_defaults_and_roundtrips_ergonomics_fields_and_backward_compat() {
    let temp = TempDir::new().expect("create temp XDG root");
    let paths = ConfigPaths::from_root(temp.path());

    // Backward-compatible load of older config.toml omitting new ergonomics fields.
    std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
    std::fs::write(
        paths.config_file(),
        r#"
schema_version = 1

[terminal]
theme = "Nord"
font_size = 15.5
"#,
    )
    .expect("write partial config");

    let compat = load_config(&paths).expect("load partial config");
    assert_eq!(compat.terminal.scrollback_lines, 10_000);
    assert_eq!(compat.terminal.theme, "Nord");
    assert_eq!(compat.terminal.font_size, 15.5);
    assert!(!compat.terminal.font_ligatures);
    assert!(compat.terminal.confirm_multiline_paste);
    assert!(!compat.terminal.copy_on_select);
    assert_eq!(compat.terminal.cursor_style, CursorStyle::Block);

    // Explicit ergonomics round-trip across save and load.
    let mut cfg = AppConfig::default();
    cfg.terminal.scrollback_lines = 15_000;
    cfg.terminal.font_family = "JetBrains Mono".to_string();
    cfg.terminal.font_size = 16.0;
    cfg.terminal.font_ligatures = true;
    cfg.terminal.cursor_style = CursorStyle::Beam;
    cfg.terminal.theme = "Catppuccin".to_string();
    cfg.terminal.copy_on_select = true;
    cfg.terminal.confirm_multiline_paste = false;

    save_config(&paths, &cfg).expect("save ergonomics config");
    let loaded = load_config(&paths).expect("reload ergonomics config");
    assert_eq!(loaded, cfg);
}
