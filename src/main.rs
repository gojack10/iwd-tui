use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use dbus::blocking::Connection;
use dbus::blocking::stdintf::org_freedesktop_dbus::{ObjectManager, Properties};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Paragraph},
};

const POLL_MS: u64 = 100;
const STATUS_POLL_MS: u64 = 300;
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

enum ActionResult {
    Scan(Result<(), String>),
    Connect {
        path: String,
        result: Result<(), String>,
    },
    Disconnect(Result<(), String>),
}

struct Network {
    path: String,
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
    pending_connect_path: Option<String>,
    scanning: bool,
    scan_started_at: Option<Instant>,
    header_error: Option<String>,
    action_tx: mpsc::Sender<ActionResult>,
    action_rx: mpsc::Receiver<ActionResult>,
    conn: Connection,
    should_quit: bool,
}

impl App {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let conn = Connection::new_system().map_err(|_| "Failed to connect to system D-Bus")?;
        let (station_path, interface_name) = find_station(&conn)?;
        let proxy = conn.with_proxy(IWD_BUS, &*station_path, DBUS_TIMEOUT);
        let state: String = proxy.get(IWD_STATION, "State")?;

        let (action_tx, action_rx) = mpsc::channel();
        let mut app = App {
            networks: Vec::new(),
            selected: 0,
            station_path,
            interface_name,
            state: state.to_uppercase(),
            pending_connect_path: None,
            scanning: false,
            scan_started_at: None,
            header_error: None,
            action_tx,
            action_rx,
            conn,
            should_quit: false,
        };
        app.refresh_runtime_state()?;
        app.refresh_networks()?;
        Ok(app)
    }

    fn refresh_networks(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let proxy = self
            .conn
            .with_proxy(IWD_BUS, &*self.station_path, DBUS_TIMEOUT);
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
                path: path.to_string(),
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
        self.reconcile_pending_connect();
        Ok(())
    }

    fn refresh_runtime_state(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let proxy = self
            .conn
            .with_proxy(IWD_BUS, &*self.station_path, DBUS_TIMEOUT);
        self.state = proxy.get::<String>(IWD_STATION, "State")?.to_uppercase();

        let scanning = proxy.get::<bool>(IWD_STATION, "Scanning")?;
        if scanning && !self.scanning {
            self.scan_started_at = Some(Instant::now());
        }

        let scan_finished = self.scanning && !scanning;
        self.scanning = scanning;

        if scan_finished {
            self.scan_started_at = None;
            self.refresh_networks()?;
        }

        self.reconcile_pending_connect();

        Ok(())
    }

    fn set_error(&mut self, error: impl ToString) {
        self.header_error = Some(condense_error(&error.to_string()));
    }

    fn clear_action_error(&mut self) {
        self.header_error = None;
    }

    fn selected_network(&self) -> Option<&Network> {
        self.networks.get(self.selected)
    }

    fn reconcile_pending_connect(&mut self) {
        let Some(path) = self.pending_connect_path.as_deref() else {
            return;
        };

        let target_connected = self
            .networks
            .iter()
            .any(|network| network.path == path && network.connected);

        if target_connected || self.state != "CONNECTING" {
            self.pending_connect_path = None;
        }
    }

    fn connect_is_idempotent_noop(&self, network: &Network) -> bool {
        network.connected || self.pending_connect_path.as_deref() == Some(network.path.as_str())
    }

    fn scan(&mut self) {
        let tx = self.action_tx.clone();
        let station_path = self.station_path.clone();
        thread::spawn(move || {
            let result = dbus_call(&station_path, IWD_STATION, "Scan");
            let _ = tx.send(ActionResult::Scan(result));
        });
    }

    fn connect_selected(&mut self) -> Result<(), String> {
        let Some(network) = self.selected_network() else {
            return Err("No network selected".into());
        };
        let network_path = network.path.clone();
        let network_type = network.net_type.clone();
        let network_known = network.known;

        if self.connect_is_idempotent_noop(network) {
            return Ok(());
        }

        if !network_known && network_type != "open" {
            return Err("Password required; T3 overlay not built yet".into());
        }

        self.pending_connect_path = Some(network_path.clone());
        let tx = self.action_tx.clone();
        thread::spawn(move || {
            let result = dbus_call(&network_path, IWD_NETWORK, "Connect");
            let _ = tx.send(ActionResult::Connect {
                path: network_path,
                result,
            });
        });
        Ok(())
    }

    fn disconnect(&mut self) {
        let tx = self.action_tx.clone();
        let station_path = self.station_path.clone();
        thread::spawn(move || {
            let result = dbus_call(&station_path, IWD_STATION, "Disconnect");
            let _ = tx.send(ActionResult::Disconnect(result));
        });
    }

    fn drain_action_results(&mut self) {
        while let Ok(result) = self.action_rx.try_recv() {
            match result {
                ActionResult::Scan(Err(e)) => self.set_error(e),
                ActionResult::Connect {
                    path,
                    result: Err(e),
                } => {
                    if self.pending_connect_path.as_deref() == Some(&path) {
                        self.pending_connect_path = None;
                    }
                    self.set_error(e);
                }
                ActionResult::Disconnect(Err(e)) => self.set_error(e),
                _ => {}
            }
        }
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
            KeyCode::Char('s') => {
                self.clear_action_error();
                self.scan();
            }
            KeyCode::Enter => {
                self.clear_action_error();
                if let Err(error) = self.connect_selected() {
                    self.set_error(error);
                }
            }
            KeyCode::Char('d') => {
                self.clear_action_error();
                self.disconnect();
            }
            _ => {}
        }
    }
}

fn dbus_call(path: &str, interface: &str, method: &str) -> Result<(), String> {
    let conn = Connection::new_system().map_err(|e| e.to_string())?;
    let proxy = conn.with_proxy(IWD_BUS, path, DBUS_TIMEOUT);
    let _: () = proxy
        .method_call(interface, method, ())
        .map_err(|e| e.to_string())?;
    Ok(())
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

fn condense_error(error: &str) -> String {
    error
        .rsplit(": ")
        .next()
        .unwrap_or(error)
        .trim()
        .trim_end_matches('.')
        .to_string()
}

fn header_state(app: &App) -> String {
    if let Some(error) = &app.header_error {
        return format!("FAILED: {error}");
    }

    if app.scanning {
        let start = app.scan_started_at.unwrap_or_else(Instant::now);
        let phase = ((start.elapsed().as_millis() / 300) % 3 + 1) as usize;
        return format!("SCANNING{}", ".".repeat(phase));
    }

    app.state.clone()
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
        Paragraph::new(" j/k:move  enter:connect  d:disconnect  f:forget  s:scan  a:autoconnect"),
        chunks[2],
    );
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let left = format!(" iwd -- {} -- {}", app.interface_name, header_state(app));
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

            line
        })
        .collect();

    let visible = inner.height as usize;
    let offset = if app.selected >= visible {
        app.selected - visible + 1
    } else {
        0
    };
    f.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), inner);
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
    terminal.clear().unwrap();
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
    let mut last_status_refresh = Instant::now();

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

        app.drain_action_results();

        if last_status_refresh.elapsed() >= Duration::from_millis(STATUS_POLL_MS) {
            let _ = app.refresh_runtime_state();
            last_status_refresh = Instant::now();
        }

        if last_refresh.elapsed() >= Duration::from_secs(REFRESH_SECS) {
            let _ = app.refresh_runtime_state();
            let _ = app.refresh_networks();
            last_refresh = Instant::now();
        }
    }
}
