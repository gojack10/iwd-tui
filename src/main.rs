macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("[{}] {}", chrono::Local::now().format("%H:%M:%S%.3f"), format!($($arg)*))
    };
}

use std::fs::OpenOptions;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use dbus::blocking::Connection;
use dbus::blocking::stdintf::org_freedesktop_dbus::{ObjectManager, Properties};
use dbus::channel::{MatchingReceiver, Sender};
use dbus::message::MatchRule;
use dbus::Message;
use dbus_crossroads::Crossroads;
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
const IWD_KNOWN_NETWORK: &str = "net.connman.iwd.KnownNetwork";
const IWD_AGENT_MANAGER: &str = "net.connman.iwd.AgentManager";
const AGENT_PATH: &str = "/iwdtui/agent";
const DBUS_TIMEOUT: Duration = Duration::from_secs(5);

enum ActionResult {
    Scan(Result<(), String>),
    Connect {
        path: String,
        result: Result<(), String>,
    },
    Disconnect(Result<(), String>),
    Forget(Result<(), String>),
    AutoConnect(Result<(), String>),
}

struct Network {
    path: String,
    name: String,
    net_type: String,
    signal_dbm: i16,
    connected: bool,
    known_path: Option<String>,
}

impl Network {
    fn is_known(&self) -> bool {
        self.known_path.is_some()
    }
}

enum Overlay {
    Password {
        input: String,
        visible: bool,
        network_path: String,
        network_name: String,
    },
    ForgetConfirm {
        known_path: String,
        network_name: String,
    },
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
    overlay: Option<Overlay>,
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
            overlay: None,
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
            let known_path = np
                .get::<dbus::Path<'static>>(IWD_NETWORK, "KnownNetwork")
                .ok()
                .map(|p| p.to_string());

            networks.push(Network {
                path: path.to_string(),
                name,
                net_type,
                signal_dbm: signal / 100,
                connected,
                known_path,
            });
        }

        // Sort by signal strength only (strongest first)
        networks.sort_by(|a, b| b.signal_dbm.cmp(&a.signal_dbm));

        let prev_path = self.networks.get(self.selected).map(|n| n.path.clone());
        self.networks = networks;
        if let Some(ref p) = prev_path {
            if let Some(idx) = self.networks.iter().position(|n| n.path == *p) {
                self.selected = idx;
            } else {
                self.selected = self.selected.min(self.networks.len().saturating_sub(1));
            }
        } else {
            self.selected = self.selected.min(self.networks.len().saturating_sub(1));
        }
        self.reconcile_pending_connect();
        Ok(())
    }

    fn refresh_runtime_state(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let state: String = {
            let proxy = self.conn.with_proxy(IWD_BUS, &*self.station_path, DBUS_TIMEOUT);
            match proxy.get(IWD_STATION, "State") {
                Ok(s) => s,
                Err(_) => {
                    // Station path stale — re-discover (IWD may have restarted)
                    let (new_path, new_name) = find_station(&self.conn)?;
                    self.station_path = new_path;
                    self.interface_name = new_name;
                    let proxy = self.conn.with_proxy(IWD_BUS, &*self.station_path, DBUS_TIMEOUT);
                    proxy.get(IWD_STATION, "State")?
                }
            }
        };
        self.state = state.to_uppercase();

        let proxy = self.conn.with_proxy(IWD_BUS, &*self.station_path, DBUS_TIMEOUT);
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
        network.connected || self.pending_connect_path.is_some()
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
        let network_name = network.name.clone();
        let network_type = network.net_type.clone();
        let network_known = network.is_known();

        if self.connect_is_idempotent_noop(network) {
            return Ok(());
        }

        if !network_known && network_type != "open" {
            log!("opening password overlay for {network_name}");
            self.overlay = Some(Overlay::Password {
                input: String::new(),
                visible: false,
                network_path,
                network_name,
            });
            return Ok(());
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

    fn agent_connect_selected(&mut self) {
        if self.pending_connect_path.is_some() {
            return;
        }
        let Some(Overlay::Password {
            ref input,
            ref network_path,
            ..
        }) = self.overlay
        else {
            return;
        };
        let password = input.clone();
        let network_path = network_path.clone();
        log!("agent_connect_selected: path={network_path} pw_len={}", password.len());
        self.pending_connect_path = Some(network_path.clone());
        self.overlay = None;
        let tx = self.action_tx.clone();
        thread::spawn(move || {
            let result = agent_connect(&network_path, &password);
            log!("agent_connect result: {result:?}");
            let _ = tx.send(ActionResult::Connect {
                path: network_path,
                result,
            });
        });
    }

    fn forget_network(&mut self) {
        let Some(Overlay::ForgetConfirm {
            ref known_path, ..
        }) = self.overlay
        else {
            return;
        };
        let known_path = known_path.clone();
        log!("forget_network: {known_path}");
        self.overlay = None;
        let tx = self.action_tx.clone();
        thread::spawn(move || {
            let result = dbus_call(&known_path, IWD_KNOWN_NETWORK, "Forget");
            log!("forget result: {result:?}");
            let _ = tx.send(ActionResult::Forget(result));
        });
    }

    fn toggle_autoconnect(&mut self) {
        let Some(network) = self.selected_network() else {
            return;
        };
        let Some(ref known_path) = network.known_path else {
            self.set_error("Not a known network");
            return;
        };
        let known_path = known_path.clone();
        log!("toggle_autoconnect: {known_path}");
        let tx = self.action_tx.clone();
        thread::spawn(move || {
            let result = (|| -> Result<(), String> {
                let conn = Connection::new_system().map_err(|e| e.to_string())?;
                let proxy = conn.with_proxy(IWD_BUS, &*known_path, DBUS_TIMEOUT);
                let current: bool = proxy
                    .get(IWD_KNOWN_NETWORK, "AutoConnect")
                    .map_err(|e| e.to_string())?;
                proxy
                    .set(IWD_KNOWN_NETWORK, "AutoConnect", !current)
                    .map_err(|e| e.to_string())?;
                Ok(())
            })();
            log!("autoconnect result: {result:?}");
            let _ = tx.send(ActionResult::AutoConnect(result));
        });
    }

    fn handle_overlay_key(&mut self, code: KeyCode) {
        match &self.overlay {
            Some(Overlay::Password { .. }) => match code {
                KeyCode::Esc => self.overlay = None,
                KeyCode::Enter => self.agent_connect_selected(),
                KeyCode::Backspace => {
                    if let Some(Overlay::Password { ref mut input, .. }) = self.overlay {
                        input.pop();
                    }
                }
                KeyCode::Char('v') => {
                    if let Some(Overlay::Password {
                        ref mut visible, ..
                    }) = self.overlay
                    {
                        *visible = !*visible;
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(Overlay::Password { ref mut input, .. }) = self.overlay {
                        input.push(c);
                    }
                }
                _ => {}
            },
            Some(Overlay::ForgetConfirm { .. }) => match code {
                KeyCode::Char('y') => self.forget_network(),
                KeyCode::Char('n') | KeyCode::Esc => self.overlay = None,
                _ => {}
            },
            None => {}
        }
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
                ActionResult::Forget(Ok(())) => {
                    let _ = self.refresh_networks();
                }
                ActionResult::Forget(Err(e)) => self.set_error(e),
                ActionResult::AutoConnect(Err(e)) => self.set_error(e),
                _ => {}
            }
        }
    }

    fn handle_key(&mut self, code: KeyCode) {
        if self.overlay.is_some() {
            self.handle_overlay_key(code);
            return;
        }
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
                if self.scanning {
                    return;
                }
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
            KeyCode::Char('f') => {
                self.clear_action_error();
                if let Some(network) = self.selected_network() {
                    if let Some(ref known_path) = network.known_path {
                        self.overlay = Some(Overlay::ForgetConfirm {
                            known_path: known_path.clone(),
                            network_name: network.name.clone(),
                        });
                    } else {
                        self.set_error("Not a known network");
                    }
                }
            }
            KeyCode::Char('a') => {
                self.clear_action_error();
                self.toggle_autoconnect();
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

fn agent_connect(network_path: &str, password: &str) -> Result<(), String> {
    match agent_connect_once(network_path, password) {
        Ok(()) => return Ok(()),
        Err(ref e) if e == "InProgress" => {}
        Err(e) => return Err(e),
    }

    // Self-heal: restart IWD to clear stale agent_request
    log!("agent_connect: InProgress — restarting iwd to clear stale state");
    restart_iwd_service()?;

    // Wait for IWD to rediscover the network after restart
    let suffix = network_path.rsplit('/').next()
        .ok_or_else(|| "Invalid network path".to_string())?;
    let new_path = wait_for_network(suffix)?;

    agent_connect_once(&new_path, password)
}

fn agent_connect_once(network_path: &str, password: &str) -> Result<(), String> {
    log!("agent_connect: opening system bus");
    let conn = Connection::new_system().map_err(|e| e.to_string())?;

    let connect_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Setup agent interface via crossroads
    let mut cr = Crossroads::new();
    let iface_token = cr.register("net.connman.iwd.Agent", |b| {
        b.method(
            "RequestPassphrase",
            ("network",),
            ("passphrase",),
            |_, password: &mut String, (_network,): (dbus::Path<'static>,)| {
                log!("agent: RequestPassphrase called");
                Ok((password.clone(),))
            },
        );
        b.method("Cancel", ("reason",), (), |_, _, (reason,): (String,)| {
            log!("agent: Cancel: {reason}");
            Ok(())
        });
        b.method("Release", (), (), |_, _, ()| {
            log!("agent: Release called");
            Ok(())
        });
    });
    cr.insert(AGENT_PATH, &[iface_token], password.to_string());

    let ce = connect_err.clone();
    conn.start_receive(
        MatchRule::new(),
        Box::new(move |mut msg, conn| {
            if msg.msg_type() == dbus::MessageType::Error {
                let err_name = msg.as_result().err()
                    .and_then(|e| e.name().map(String::from));
                log!("agent: recv Error name={:?} items={:?}", err_name, msg.get_items());
                *ce.lock().unwrap() = err_name;
            } else {
                log!("agent: recv {:?} member={:?} path={:?} items={:?}",
                    msg.msg_type(), msg.member(), msg.path(), msg.get_items());
                if msg.msg_type() == dbus::MessageType::MethodCall {
                    cr.handle_message(msg, conn).unwrap();
                }
            }
            true
        }),
    );

    // Find station and set up proxies
    let station_path = find_station_path(&conn).map_err(|e| e.to_string())?;
    let station_proxy = conn.with_proxy(IWD_BUS, &*station_path, DBUS_TIMEOUT);
    let mgr_proxy = conn.with_proxy(IWD_BUS, "/net/connman/iwd", DBUS_TIMEOUT);

    // Disconnect if currently connected
    let state: String = station_proxy.get(IWD_STATION, "State").unwrap_or_default();
    if state != "disconnected" {
        log!("agent_connect: disconnecting (state={state})");
        let _: Result<(), _> = station_proxy.method_call(IWD_STATION, "Disconnect", ());
        for i in 0..20 {
            thread::sleep(Duration::from_millis(50));
            let s: String = station_proxy.get(IWD_STATION, "State").unwrap_or_default();
            if s == "disconnected" {
                log!("agent_connect: disconnected after {i} polls");
                break;
            }
        }
    }

    // Clear stale agent from previous crash, then register fresh
    let _ = mgr_proxy.method_call::<(), _, _, _>(
        IWD_AGENT_MANAGER,
        "UnregisterAgent",
        (dbus::Path::from(AGENT_PATH),),
    );

    log!("agent_connect: registering agent at {AGENT_PATH}");
    let _: () = mgr_proxy
        .method_call(
            IWD_AGENT_MANAGER,
            "RegisterAgent",
            (dbus::Path::from(AGENT_PATH),),
        )
        .map_err(|e| e.to_string())?;

    // Non-blocking Connect — process() will dispatch RequestPassphrase to crossroads
    log!("agent_connect: sending Connect on {network_path}");
    let msg = Message::new_method_call(IWD_BUS, network_path, IWD_NETWORK, "Connect")
        .map_err(|e| e)?;
    conn.send(msg).map_err(|()| "Failed to send Connect")?;

    // Flush outgoing queue to ensure Connect is actually on the wire
    let _ = conn.channel().read_write(Some(Duration::from_millis(0)));
    log!("agent_connect: Connect flushed");

    // Pump messages until connected, failed, or timed out
    let target_proxy = conn.with_proxy(IWD_BUS, network_path, DBUS_TIMEOUT);
    let deadline = Instant::now() + Duration::from_secs(30);
    let started = Instant::now();
    let grace = Duration::from_secs(5);
    let mut seen_connecting = false;

    let result = loop {
        // Drain all pending messages — dispatches RequestPassphrase to crossroads
        // Must happen BEFORE blocking proxy.get() calls which could eat messages
        let mut drained = 0u32;
        loop {
            match conn.process(Duration::from_millis(0)) {
                Ok(true) => { drained += 1; }
                Ok(false) => break,
                Err(e) => {
                    let _ = mgr_proxy.method_call::<(), _, _, _>(
                        IWD_AGENT_MANAGER, "UnregisterAgent",
                        (dbus::Path::from(AGENT_PATH),),
                    );
                    return Err(format!("D-Bus error: {e}"));
                }
            }
        }
        if drained > 0 {
            log!("agent_connect: drained {drained} messages");
        }

        // Check if Connect returned an error (set by start_receive handler)
        if let Some(err) = connect_err.lock().unwrap().take() {
            if err.contains("InProgress") {
                log!("agent_connect: InProgress — stale agent_request on network");
                break Err("InProgress".into());
            }
            let label = err.rsplit('.').next().unwrap_or(&err).to_string();
            log!("agent_connect: Connect error: {err}");
            break Err(label);
        }

        let connected: bool = target_proxy
            .get(IWD_NETWORK, "Connected")
            .unwrap_or(false);
        if connected {
            log!("agent_connect: success");
            break Ok(());
        }

        let state: String = station_proxy
            .get(IWD_STATION, "State")
            .unwrap_or_default();

        if state == "connecting" && !seen_connecting {
            log!("agent_connect: state=connecting");
            seen_connecting = true;
        }

        if state == "disconnected" && (seen_connecting || started.elapsed() > grace) {
            log!("agent_connect: failed (seen_connecting={seen_connecting})");
            break Err("Connection failed".into());
        }

        if Instant::now() >= deadline {
            log!("agent_connect: timed out");
            break Err("Connection timed out".into());
        }

        // Wait for new messages (blocking read, up to 250ms)
        let _ = conn.process(Duration::from_millis(250));
    };

    // Always unregister agent
    let _ = mgr_proxy.method_call::<(), _, _, _>(
        IWD_AGENT_MANAGER,
        "UnregisterAgent",
        (dbus::Path::from(AGENT_PATH),),
    );

    result
}

fn restart_iwd_service() -> Result<(), String> {
    // Try without sudo, then with sudo -n (non-interactive)
    if let Ok(s) = std::process::Command::new("sv")
        .args(["restart", "iwd"])
        .stderr(std::process::Stdio::null())
        .status()
    {
        if s.success() {
            log!("restart_iwd: success");
            return Ok(());
        }
    }
    if let Ok(s) = std::process::Command::new("sudo")
        .args(["-n", "sv", "restart", "iwd"])
        .stderr(std::process::Stdio::null())
        .status()
    {
        if s.success() {
            log!("restart_iwd: success (sudo)");
            return Ok(());
        }
    }
    Err("Failed to restart iwd (need root)".into())
}

fn wait_for_network(path_suffix: &str) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut scanned = false;

    while Instant::now() < deadline {
        thread::sleep(Duration::from_secs(1));

        let conn = match Connection::new_system() {
            Ok(c) => c,
            Err(_) => continue,
        };
        let proxy = conn.with_proxy(IWD_BUS, "/", DBUS_TIMEOUT);
        let objects = match proxy.get_managed_objects() {
            Ok(o) => o,
            Err(_) => continue,
        };

        // Trigger a scan once a station appears
        if !scanned {
            for (path, ifaces) in &objects {
                if ifaces.contains_key(IWD_STATION) {
                    let sp = conn.with_proxy(IWD_BUS, path.clone(), DBUS_TIMEOUT);
                    let _: Result<(), _> = sp.method_call(IWD_STATION, "Scan", ());
                    scanned = true;
                    break;
                }
            }
        }

        // Check if our network reappeared
        for (path, ifaces) in &objects {
            if ifaces.contains_key(IWD_NETWORK) && path.to_string().ends_with(path_suffix) {
                log!("wait_for_network: found {path}");
                return Ok(path.to_string());
            }
        }
    }

    Err("Network not found after iwd restart".into())
}

fn find_station_path(conn: &Connection) -> Result<String, Box<dyn std::error::Error>> {
    let (path, _) = find_station(conn)?;
    Ok(path)
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
    if app.overlay.is_some() {
        render_overlay(f, f.area(), app);
    }

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
            } else if net.is_known() {
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

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

fn render_overlay(f: &mut Frame, area: Rect, app: &App) {
    match &app.overlay {
        Some(Overlay::Password {
            input,
            visible,
            network_name,
            ..
        }) => {
            let rect = centered_rect(42, 7, area);
            f.render_widget(ratatui::widgets::Clear, rect);

            let display_name: String = network_name.chars().take(30).collect();
            let masked: String = if *visible {
                input.clone()
            } else {
                "*".repeat(input.len())
            };
            let pw_display: String = masked.chars().rev().take(28).collect::<String>().chars().rev().collect();

            let block = Block::bordered().border_set(ASCII_BORDER);
            let inner = block.inner(rect);
            f.render_widget(block, rect);

            let lines = vec![
                Line::from(format!("  CONNECT: {display_name}")),
                Line::from(""),
                Line::from(format!("  Password: {pw_display}_")),
                Line::from(""),
                Line::from("  v:show  enter:connect  esc:cancel"),
            ];
            f.render_widget(Paragraph::new(lines), inner);
        }
        Some(Overlay::ForgetConfirm { network_name, .. }) => {
            let rect = centered_rect(42, 5, area);
            f.render_widget(ratatui::widgets::Clear, rect);

            let display_name: String = network_name.chars().take(28).collect();

            let block = Block::bordered().border_set(ASCII_BORDER);
            let inner = block.inner(rect);
            f.render_widget(block, rect);

            let lines = vec![
                Line::from(format!("  FORGET: {display_name}?")),
                Line::from(""),
                Line::from("  y:confirm  n:cancel"),
            ];
            f.render_widget(Paragraph::new(lines), inner);
        }
        None => {}
    }
}

fn main() {
    // Redirect stderr to log file
    use std::os::unix::io::AsRawFd;
    if let Ok(f) = OpenOptions::new().create(true).append(true).open("/tmp/iwd-tui.log") {
        unsafe { libc::dup2(f.as_raw_fd(), 2); }
    }
    log!("starting iwd-tui");

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
