use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use dbus::blocking::stdintf::org_freedesktop_dbus::{ObjectManager, Properties};
use dbus::blocking::Connection;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};

const POLL_MS: u64 = 100;
const REFRESH_SECS: u64 = 2;
const MAX_SSID: usize = 20;
const MAX_BLOCKS: usize = 10;

const ASCII_BORDER: border::Set = border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

const IWD_BUS: &str = "net.connman.iwd";
const IWD_STATION: &str = "net.connman.iwd.Station";
const IWD_NETWORK: &str = "net.connman.iwd.Network";
const IWD_DEVICE: &str = "net.connman.iwd.Device";
const DBUS_TIMEOUT: Duration = Duration::from_secs(5);

struct Network {
    name: String,
    net_type: String,
    signal_dbm: i16,
    connected: bool,
    known: bool,
}

struct App {
    networks: Vec<Network>,
    selected: usize,
    station_path: String,
    interface_name: String,
    state: String,
    conn: Connection,
    should_quit: bool,
}

impl App {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let conn =
            Connection::new_system().map_err(|_| "Failed to connect to system D-Bus")?;
        let (station_path, interface_name) = find_station(&conn)?;
        let proxy = conn.with_proxy(IWD_BUS, &*station_path, DBUS_TIMEOUT);
        let state: String = proxy.get(IWD_STATION, "State")?;

        let mut app = App {
            networks: Vec::new(),
            selected: 0,
            station_path,
            interface_name,
            state: state.to_uppercase(),
            conn,
            should_quit: false,
        };
        app.refresh_networks()?;
        Ok(app)
    }

    fn refresh_networks(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let proxy = self.conn.with_proxy(IWD_BUS, &*self.station_path, DBUS_TIMEOUT);
        self.state = proxy
            .get::<String>(IWD_STATION, "State")?
            .to_uppercase();

        let (ordered,): (Vec<(dbus::Path<'static>, i16)>,) =
            proxy.method_call(IWD_STATION, "GetOrderedNetworks", ())?;

        let mut networks = Vec::with_capacity(ordered.len());
        for (path, signal) in ordered {
            let np = self.conn.with_proxy(IWD_BUS, &*path, DBUS_TIMEOUT);
            let name: String = np.get(IWD_NETWORK, "Name")?;
            let net_type: String = np.get(IWD_NETWORK, "Type")?;
            let connected: bool = np.get(IWD_NETWORK, "Connected")?;
            let known = np
                .get::<dbus::Path<'static>>(IWD_NETWORK, "KnownNetwork")
                .is_ok();

            networks.push(Network {
                name,
                net_type,
                signal_dbm: signal / 100,
                connected,
                known,
            });
        }

        // Sort: connected first, then known by signal desc, then unknown by signal desc
        networks.sort_by(|a, b| {
            a.connected
                .cmp(&b.connected)
                .reverse()
                .then(a.known.cmp(&b.known).reverse())
                .then(b.signal_dbm.cmp(&a.signal_dbm))
        });

        self.networks = networks;
        if !self.networks.is_empty() {
            self.selected = self.selected.min(self.networks.len() - 1);
        }
        Ok(())
    }

    fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected < self.networks.len().saturating_sub(1) {
                    self.selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
            }
            _ => {}
        }
    }
}

fn find_station(conn: &Connection) -> Result<(String, String), Box<dyn std::error::Error>> {
    let proxy = conn.with_proxy(IWD_BUS, "/", DBUS_TIMEOUT);
    let objects = proxy
        .get_managed_objects()
        .map_err(|_| "iwd service not found on D-Bus. Is iwd running?")?;

    let mut stations: Vec<String> = objects
        .iter()
        .filter(|(_, ifaces)| ifaces.contains_key(IWD_STATION))
        .map(|(path, _)| path.to_string())
        .collect();
    stations.sort();

    if stations.is_empty() {
        return Err("No wireless adapters found. Check rfkill?".into());
    }

    // Prefer connected station, else first
    let mut selected = stations[0].clone();
    for path in &stations {
        let p = conn.with_proxy(IWD_BUS, path.as_str(), DBUS_TIMEOUT);
        if let Ok(state) = p.get::<String>(IWD_STATION, "State") {
            if state == "connected" {
                selected = path.clone();
                break;
            }
        }
    }

    let p = conn.with_proxy(IWD_BUS, selected.as_str(), DBUS_TIMEOUT);
    let name: String = p
        .get(IWD_DEVICE, "Name")
        .unwrap_or_else(|_| "unknown".into());

    Ok((selected, name))
}

fn signal_bar(dbm: i16) -> String {
    let clamped = dbm.max(-90).min(-30) as f32;
    let blocks = ((clamped + 90.0) / 6.0).round() as usize;
    let blocks = blocks.min(MAX_BLOCKS);
    let width = blocks + 3;
    let padding = (MAX_BLOCKS + 3).saturating_sub(width);
    format!("{}▓▒░{}", "█".repeat(blocks), " ".repeat(padding))
}

fn dbm_color(dbm: i16) -> Color {
    if dbm >= -55 {
        Color::Green
    } else if dbm >= -70 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(f.area());

    render_header(f, chunks[0], app);
    render_list(f, chunks[1], app);

    f.render_widget(
        Paragraph::new(
            " j/k:move  enter:connect  d:disconnect  f:forget  s:scan  a:autoconnect",
        ),
        chunks[2],
    );
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let left = format!(" iwd -- {} -- {}", app.interface_name, app.state);
    let right = "esc:quit ";
    let gap = (area.width as usize).saturating_sub(left.len() + right.len());
    let header = Line::from(vec![
        Span::raw(left),
        Span::raw(" ".repeat(gap)),
        Span::raw(right),
    ]);
    f.render_widget(Paragraph::new(header), area);
}

fn render_list(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().border_set(ASCII_BORDER);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines: Vec<Line> = app
        .networks
        .iter()
        .enumerate()
        .map(|(i, net)| {
            let cursor = if i == app.selected { ">" } else { " " };
            let ssid: String = net.name.chars().take(MAX_SSID).collect();
            let bar = signal_bar(net.signal_dbm);
            let status = if net.connected {
                "CONNECTED"
            } else if net.known {
                "known"
            } else {
                ""
            };

            let line = Line::from(vec![
                Span::raw(format!("{cursor}  {ssid:<MAX_SSID$}  ")),
                Span::raw(format!("{bar} ")),
                Span::styled(
                    format!("{:>4} dBm", net.signal_dbm),
                    Style::default().fg(dbm_color(net.signal_dbm)),
                ),
                Span::raw(format!("   {:<5} {}", net.net_type, status)),
            ]);

            if i == app.selected {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            }
        })
        .collect();

    f.render_widget(Paragraph::new(lines), inner);
}

fn main() {
    let mut app = match App::new() {
        Ok(app) => app,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app);
    ratatui::restore();

    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut last_refresh = Instant::now();

    loop {
        terminal.draw(|f| ui(f, app))?;

        if event::poll(Duration::from_millis(POLL_MS))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code);
                }
            }
        }

        if app.should_quit {
            return Ok(());
        }

        if last_refresh.elapsed() >= Duration::from_secs(REFRESH_SECS) {
            let _ = app.refresh_networks();
            last_refresh = Instant::now();
        }
    }
}
