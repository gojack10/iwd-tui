use super::*;
use ratatui::{Terminal, backend::TestBackend};
use std::sync::mpsc;

fn net(name: &str, dbm: i16, connected: bool, known: bool) -> Network {
    Network {
        path: format!("/net/connman/iwd/0/{name}"),
        name: name.into(),
        net_type: "psk".into(),
        signal_dbm: dbm,
        connected,
        known_path: if known {
            Some(format!("/kn/{name}"))
        } else {
            None
        },
    }
}

fn test_app(networks: Vec<Network>, state: &str) -> App {
    let (tx, rx) = mpsc::channel();
    App {
        networks,
        selected: 0,
        station_path: String::new(),
        interface_name: "wlan0".into(),
        state: state.to_uppercase(),
        pending_connect_path: None,
        scanning: false,
        scan_started_at: None,
        overlay: None,
        header_error: None,
        action_tx: tx,
        action_rx: rx,
        conn: Connection::new_system().unwrap(),
        should_quit: false,
        diagnostics: None,
        connected_since: None,
        detail_autoconnect: None,
        detail_ipv4: None,
        ethernet: None,
        wifi_rx_bytes: 0,
        wifi_tx_bytes: 0,
    }
}

fn render_to_string(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui(f, app)).unwrap();
    let buf = terminal.backend().buffer();
    let area = buf.area;
    let mut output = String::new();
    for y in area.y..area.y + area.height {
        let mut line = String::new();
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell((x, y)) {
                line.push_str(cell.symbol());
            }
        }
        let trimmed = line.trim_end();
        output.push_str(trimmed);
        output.push('\n');
    }
    output
}

// ===== Pure function tests =====

#[test]
fn test_freq_to_channel() {
    assert_eq!(freq_to_channel(2412), "1");
    assert_eq!(freq_to_channel(2437), "6");
    assert_eq!(freq_to_channel(2462), "11");
    assert_eq!(freq_to_channel(2484), "14");
    assert_eq!(freq_to_channel(5180), "36");
    assert_eq!(freq_to_channel(5745), "149");
    assert_eq!(freq_to_channel(5825), "165");
    assert_eq!(freq_to_channel(9999), "9999 MHz");
}

#[test]
fn test_format_bytes() {
    assert_eq!(format_bytes(0), "0 B");
    assert_eq!(format_bytes(500), "500 B");
    assert_eq!(format_bytes(1024), "1.0 KB");
    assert_eq!(format_bytes(1536), "1.5 KB");
    assert_eq!(format_bytes(1024 * 1024), "1 MB");
    assert_eq!(format_bytes(5 * 1024 * 1024), "5 MB");
    assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GB");
    assert_eq!(
        format_bytes(2 * 1024 * 1024 * 1024 + 512 * 1024 * 1024),
        "2.50 GB"
    );
}

#[test]
fn test_format_speed() {
    assert_eq!(format_speed(100), "100Mbps");
    assert_eq!(format_speed(999), "999Mbps");
    assert_eq!(format_speed(1000), "1Gbps");
    assert_eq!(format_speed(2500), "2Gbps");
}

#[test]
fn test_signal_bar() {
    let strong = signal_bar(-40);
    let weak = signal_bar(-80);
    assert!(strong.matches('█').count() > weak.matches('█').count());
    // Edge values don't panic
    let _ = signal_bar(-30);
    let _ = signal_bar(-90);
}

#[test]
fn test_dbm_color() {
    assert_eq!(dbm_color(-40), Color::Green);
    assert_eq!(dbm_color(-55), Color::Green);
    assert_eq!(dbm_color(-56), Color::Yellow);
    assert_eq!(dbm_color(-70), Color::Yellow);
    assert_eq!(dbm_color(-71), Color::Red);
    assert_eq!(dbm_color(-90), Color::Red);
}

#[test]
fn test_condense_error() {
    assert_eq!(
        condense_error("D-Bus error: org.freedesktop.DBus: Failed"),
        "Failed"
    );
    assert_eq!(condense_error("simple error"), "simple error");
    assert_eq!(
        condense_error("error with trailing."),
        "error with trailing"
    );
    assert_eq!(condense_error(""), "");
}

#[test]
fn test_format_uptime_secs() {
    assert_eq!(format_uptime_secs(0), "0m 0s");
    assert_eq!(format_uptime_secs(30), "0m 30s");
    assert_eq!(format_uptime_secs(90), "1m 30s");
    assert_eq!(format_uptime_secs(3600), "1h 0m");
    assert_eq!(format_uptime_secs(3661), "1h 1m");
    assert_eq!(format_uptime_secs(7200), "2h 0m");
}

#[test]
fn test_centered_rect() {
    let area = Rect::new(0, 0, 80, 20);
    let rect = centered_rect(42, 7, area);
    assert_eq!(rect.x, 19);
    assert_eq!(rect.y, 6);
    assert_eq!(rect.width, 42);
    assert_eq!(rect.height, 7);

    // Clamping when area is smaller than requested
    let small = Rect::new(0, 0, 30, 5);
    let rect = centered_rect(42, 7, small);
    assert_eq!(rect.width, 30);
    assert_eq!(rect.height, 5);
}

// ===== Snapshot tests =====

#[test]
fn snapshot_narrow_connected() {
    let app = test_app(
        vec![
            net("HomeWiFi", -45, true, true),
            net("Neighbor", -72, false, false),
        ],
        "connected",
    );
    insta::assert_snapshot!(render_to_string(&app, 80, 20));
}

#[test]
fn snapshot_wide_connected() {
    let mut app = test_app(
        vec![
            net("HomeWiFi", -45, true, true),
            net("CoffeeShop", -65, false, true),
            net("Neighbor", -72, false, false),
        ],
        "connected",
    );
    app.diagnostics = Some(Diagnostics {
        rssi: Some(-45),
        frequency: Some(5180),
        tx_bitrate: Some(866700),
        rx_bitrate: Some(650000),
        security: Some("WPA2-PSK".into()),
    });
    app.wifi_rx_bytes = 1024 * 1024 * 150;
    app.wifi_tx_bytes = 1024 * 1024 * 25;
    app.detail_autoconnect = Some(true);
    app.detail_ipv4 = Some("192.168.███.███".into());
    insta::assert_snapshot!(render_to_string(&app, 120, 24));
}

#[test]
fn snapshot_wide_disconnected() {
    let app = test_app(
        vec![
            net("OpenNet", -50, false, false),
            net("SecureNet", -68, false, true),
        ],
        "disconnected",
    );
    insta::assert_snapshot!(render_to_string(&app, 120, 24));
}

#[test]
fn snapshot_ethernet() {
    let mut app = test_app(vec![net("HomeWiFi", -45, true, true)], "connected");
    app.ethernet = Some(EthernetInfo {
        interface: "eth0".into(),
        speed_mbps: Some(1000),
        rx_bytes: 1024 * 1024 * 500,
        tx_bytes: 1024 * 1024 * 50,
        ipv4_masked: Some("10.0.███.███".into()),
    });
    insta::assert_snapshot!(render_to_string(&app, 120, 24));
}

#[test]
fn snapshot_password_overlay() {
    let mut app = test_app(vec![net("SecureNet", -55, false, false)], "disconnected");
    app.overlay = Some(Overlay::Password {
        input: "secret".into(),
        visible: false,
        network_path: "/net/connman/iwd/0/SecureNet".into(),
        network_name: "SecureNet".into(),
    });
    insta::assert_snapshot!(render_to_string(&app, 80, 20));
}

#[test]
fn snapshot_forget_overlay() {
    let mut app = test_app(vec![net("OldNetwork", -60, false, true)], "disconnected");
    app.overlay = Some(Overlay::ForgetConfirm {
        known_path: "/kn/OldNetwork".into(),
        network_name: "OldNetwork".into(),
    });
    insta::assert_snapshot!(render_to_string(&app, 80, 20));
}

#[test]
fn snapshot_scanning() {
    let mut app = test_app(vec![net("HomeWiFi", -45, false, true)], "disconnected");
    app.scanning = true;
    insta::assert_snapshot!(render_to_string(&app, 80, 20));
}

#[test]
fn snapshot_error() {
    let mut app = test_app(vec![net("HomeWiFi", -45, false, true)], "disconnected");
    app.header_error = Some("Connection failed".into());
    insta::assert_snapshot!(render_to_string(&app, 80, 20));
}
