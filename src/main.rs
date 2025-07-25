use std::{
    clone, collections::HashMap, fs::{
        self, File
    }, io::{
        self, 
        Write
    }, net::ToSocketAddrs, path::Path, sync::{
        mpsc::{self, Receiver, Sender}, Arc, Mutex
    }, thread::{self, sleep_ms, JoinHandle}, time::{
        Duration, Instant, SystemTime, UNIX_EPOCH
    }
};
use futures_util::{stream::{SplitSink, SplitStream}, SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async, tungstenite::{
        client::IntoClientRequest, Message, http::Response, Error
    }, MaybeTlsStream, WebSocketStream
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    layout::{Constraint, Layout}, prelude::{
        Buffer, Rect
    }, style::{Color, Stylize}, symbols::border, text::{
        Line, Text
    }, widgets::{
        Block, Paragraph, Widget
    }, DefaultTerminal, Frame
};
use sha2::{self, Sha256, Digest};
use serde::{Deserialize, Serialize};
use serde_json::{to_string, to_string_pretty, Value};

const CONFIG_PATH: &str = "./config.json";
const PORT: u16 = 2096;
const DARK_GRAY: Color = Color::Rgb(60, 60, 60);

#[derive(Clone, Debug, Default)]
pub struct Server {
    hostname: String
}

#[derive(Debug)]
enum Screen {
    Selection,
    Connected,
    New,
    Status,
    Message, // status but waits for user to press enter before continuing
    Login
}
#[derive(PartialEq)]
enum LoginField {
    Username,
    Password
}

#[derive(Default, Clone)]
struct Login {
    username: String,
    password: String,
}

impl Default for Screen {
    fn default() -> Self {
        Screen::Selection
    }
}

pub struct ConnectedSocket {
    write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>
}

impl ConnectedSocket {
    pub async fn send(&mut self, data: String) {
        self.write.send(data.into()).await;
    }

    pub async fn read(&mut self) -> Option<Result<Message, Error>> {
        self.read.next().await
    }
}

pub struct App {
    screen: Screen,
    servers: Vec<Server>,
    selected_server: usize,
    server_selection_scroll_size: u16,
    scroll_offset: i32,
    ping_times: Arc<Mutex<Vec<Option<u128>>>>,
    editing_server: Server, // the server that is in the new connection screen
    editing_existing_index: Option<usize>,
    exit: bool,
    status_text: String,
    terminal: Option<DefaultTerminal>,
    next_fn: Option<Box<dyn Fn(&mut Self)>>,
    connected_socket: Option<ConnectedSocket>,
    selected_login_field: LoginField,
    login: Login,
    session: String
}

#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    servers: Vec<String>
}

fn go_to_selection() -> Box<dyn Fn(&mut App)>{
    return Box::new(|app: &mut App| {app.go_to_selection()})
}

fn nop() -> Box<dyn Fn(&mut App)> {
    return Box::new(|_| {})
}

fn payload<T: Into<String>>(action: T, data: HashMap<String, Value>) -> String {
    let action_str = action.into();
    let mut cloned = data.clone();
    cloned.insert("action".to_string(), Value::String(action_str));
    return to_string(&cloned).unwrap();
}

impl App {
    pub async fn run(&mut self, sender: Sender<Vec<Server>>) -> io::Result<()> {
        while !self.exit {
            let _ = sender.send(self.servers.clone());
            self.redraw();
            let _ = self.event_handler().await;
           
        }
        // save config
        let config = Config {
            servers: self.servers.iter().map(|s| s.hostname.clone()).collect()
        };
        let json = to_string_pretty(&config)?;
        let mut file = File::create(CONFIG_PATH)?;
        file.write(json.as_bytes()).expect("Failed to write to file.");

        Ok(())
    }

    // render is a reserved function
    pub fn draw(&mut self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    pub fn new_server(&mut self, editing: bool) {
        match editing {
            true => {
                self.editing_server = self.servers[self.selected_server].clone();
                self.editing_existing_index = Some(self.selected_server)
            },
            false => {
                self.editing_server = Server::default();
                self.editing_existing_index = None
            }
        };
        self.screen = Screen::New;
    }

    pub fn redraw(&mut self) {
        let mut terminal = self.terminal.take().unwrap();
        let _ = terminal.draw(|frame| self.draw(frame));
        self.terminal = Some(terminal);
    }

    pub fn status<T: Into<String>>(&mut self, status_text: T) {
        self.status_text = status_text.into();
        self.screen = Screen::Status;
        self.redraw();
    }

    pub fn message<T: Into<String>>(&mut self, status_text: T, next: Box<dyn Fn(&mut Self)>) {
        self.status_text = status_text.into();
        self.screen = Screen::Message;
        self.next_fn = Some(next);
        self.redraw();
    }

    pub fn go_to_selection(&mut self) {
        self.connected_socket = None;
        self.login = Default::default();
        self.screen = Screen::Selection;
    }

    pub async fn connect(&mut self) {
        // todo: make a WSS for tls (and e2ee eventually)
        let url = format!("ws://{}:{PORT}", self.servers[self.selected_server].hostname).into_client_request().unwrap();
        self.status("Connecting...");
        let res: Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response<Option<Vec<u8>>>), Error> = connect_async(url).await;
        match res {
            Ok((ws_stream, response)) => {
                // ws_stream is a WebSocketStream<MaybeTlsStream<TcpStream>>
                self.status(
                    format!("Connected to the server. ({})", response.status())
                );
                // SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>
                // SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>
                let (mut write, mut read) = ws_stream.split();

                let _ = write.send(payload("session", HashMap::new()).into()).await;
                if let Some(msg) = read.next().await {
                    match msg {
                        Ok(v) => {
                            self.connected_socket = Some(ConnectedSocket { write: write, read: read });
                            let raw = v.into_text().unwrap().to_string();
                            let json: Value = serde_json::from_str(&raw).unwrap();
                            self.session = json["session"].to_string();
                            self.screen = Screen::Login;
                        },
                        Err(_) => {
                            self.message("Unable to get session.", go_to_selection());
                        }
                    }
                };
            },
            Err(e) => {
                self.message(
                    format!("Failed to connect to server\n{e}"),
                    go_to_selection()
                )
            }
        }  
    }

    pub async fn handle_login(&mut self, socket: &mut ConnectedSocket) {
        if let Some(msg) = socket.read().await {
            match msg {
                Ok(v) => {
                    let json: Value = serde_json::from_str(&v.into_text().unwrap().to_string()).unwrap();
                    match json["success"].as_bool().unwrap() {
                        true => {
                            self.status("Logged in successfully. [TODO] Loading clientside...")
                        },
                        false => self.message(format!("Login failed: {}", json["reason"]), go_to_selection())
                    }
                }
                Err(_) => {
                    self.message("Failed to log in.", go_to_selection())
                }
            }
        }
    }

    pub fn session_template(&mut self) -> HashMap<String, Value> {
        let mut template: HashMap<String, Value> = HashMap::new();
        template.insert("session".to_string(), Value::String(self.session.clone()));
        return template
    }

    pub async fn selection_keybinds(&mut self, key_event: KeyEvent) {
        let servers = &mut self.servers;
        let cursor_on_server = self.selected_server != servers.len();
        match key_event.code {
            KeyCode::Down => {
                if self.selected_server < servers.len() {
                    self.selected_server += 1;
                }
                if self.selected_server as i32 - self.scroll_offset > self.server_selection_scroll_size as i32 - 3 {
                    self.scroll_offset += 1;
                }
            },
            KeyCode::Up => {
                if self.selected_server > 0 {
                    self.selected_server -= 1;
                }
                if self.selected_server as i32 - self.scroll_offset < 0 {
                    self.scroll_offset -= 1;
                }
            }
            KeyCode::Enter => {
                match cursor_on_server {
                    false => {
                        self.new_server(false);
                    },
                    true => {
                        self.connect().await;
                    }
                }
            },
            KeyCode::Char('e') => {
                if cursor_on_server {
                    self.new_server(true);
                }
                self.screen = Screen::New;
            },
            KeyCode::Delete => {
                if cursor_on_server {
                    self.servers.remove(self.selected_server);
                }
            },
            KeyCode::Char('n') => {
                self.new_server(false);
            }
            KeyCode::Esc => {
                self.exit = true;
            },
            _ => {}
        }
    }

    pub fn connected_keybinds(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc => {
                self.exit = true;
            },
            _ => {}
        }
    }

    pub fn new_keybinds(&mut self, key_event: KeyEvent) {
        let servers = &mut self.servers;
        match key_event.code {
            KeyCode::Esc => {
                self.screen = Screen::Selection;
            },
            KeyCode::Enter => {
                let hostname = self.editing_server.hostname.trim();
                self.editing_server.hostname = hostname.to_string();
                match self.editing_existing_index {
                    Some(index) => {
                        servers[index] = self.editing_server.clone();
                        {
                            let mut pings = self.ping_times.lock().unwrap();
                            pings[index] = None;
                        }
                    },
                    None => {
                        servers.push(self.editing_server.clone());
                        {
                            let mut pings = self.ping_times.lock().unwrap();
                            pings.push(None);
                        }
                    }
                }
                self.screen = Screen::Selection;
            },
            KeyCode::Char(input) => {
                self.editing_server.hostname.push(input);
            },
            KeyCode::Backspace => {
                self.editing_server.hostname.pop();
            }
            _ => {}
        }
    }

    pub fn status_keybinds(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc => {
                self.exit = true;
            },
            _ => {}
        }
    }

    pub fn message_keybinds(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc => {
                self.exit = true;
            },
            KeyCode::Enter => {
                if let Some(next_fn) = self.next_fn.take() {
                    next_fn(self)
                }
            }
            _ => {}
        }
    }

    pub async fn login_keybinds (&mut self, key_event: KeyEvent) { 
        match key_event.code {
            KeyCode::Enter => {
                match self.selected_login_field {
                    LoginField::Username => self.selected_login_field = LoginField::Password,
                    LoginField::Password => { // login
                        let mut data = self.session_template();
                        data.insert("session".into(), self.session.clone().into());
                        data.insert("username".into(), self.login.username.clone().into());
                        data.insert(
                            "password".into(), 
                            Sha256::digest(self.login.password.clone())[..].into()
                        );

                        if let Some(socket) = self.connected_socket.as_mut() {
                            socket.send(payload("login", data)).await;
                        };
                        self.status("Logging in...");
                        if let Some(mut socket) = self.connected_socket.take() {
                            self.handle_login(&mut socket).await;
                            self.connected_socket = Some(socket);
                        }
                    }
                }
            },
            KeyCode::Tab => {
                self.selected_login_field = match self.selected_login_field {
                    LoginField::Username => LoginField::Password,
                    LoginField::Password => LoginField::Username
                }
            },
            KeyCode::Esc => {
                self.go_to_selection();
            }
            KeyCode::Char(char) => {
                match self.selected_login_field {
                    LoginField::Password => self.login.password.push(char),
                    LoginField::Username => self.login.username.push(char)
                }
            },
            KeyCode::Backspace => {
                match self.selected_login_field {
                    LoginField::Password => self.login.password.pop(),
                    LoginField::Username => self.login.username.pop()
                };
            }
            _ => {}
        }
    }

    pub async fn event_handler(&mut self) -> io::Result<()> {
        if event::poll(Duration::from_millis(0))? {
            if let Event::Key(key_event) = event::read()? && key_event.kind == KeyEventKind::Press {
                match self.screen {
                    Screen::Selection => self.selection_keybinds(key_event).await,
                    Screen::Connected => self.connected_keybinds(key_event),
                    Screen::New => self.new_keybinds(key_event),
                    Screen::Status => self.status_keybinds(key_event),
                    Screen::Message => self.message_keybinds(key_event),
                    Screen::Login => self.login_keybinds(key_event).await
                };
            }
        }
        Ok(())
    }
}

impl Widget for &mut App {
    // here is the drawing logic
    fn render(self, area: Rect, buf: &mut Buffer) {
        let ping_times = {
            let readonly = self.ping_times.lock().unwrap();
            readonly.clone()
        };
        let servers =  &self.servers;
        let mut status_text = |texts: Vec<&str>| {
            let status = self.status_text.clone();
            let layout = Layout::vertical(vec![
                Constraint::Min(1),
                Constraint::Length(status.chars().filter(|c| *c == '\n').count() as u16 + 3),
                Constraint::Min(1)
            ]).split(area);

            let block = Block::bordered()
                .border_set(border::DOUBLE)
                .title(Line::from(texts[0]))
                .title_bottom(Line::from(texts[1]).right_aligned());

            Paragraph::new(Text::from(
                status.split('\n').map(|line| Line::from(line).centered()).collect::<Vec<Line>>()
            ))
                .block(block)
                .render(layout[1], buf);
        };
        match self.screen {
            Screen::Selection => {
                let title = Line::from(format!(" Server Selection ").bold());

                let is_server = {
                    self.selected_server < servers.len()
                };

                self.server_selection_scroll_size = area.height;

                let mut lines_vec: Vec<Line> = vec![];
                for (i, server) in servers.iter().enumerate() {
                    if i < self.scroll_offset as usize {
                        continue
                    }
                    let line = Line::from(vec![
                        match match ping_times.get(i) {
                            Some(v) => v,
                            None => &None
                        } {
                            Some(v) => format!(" {:<7} ", v.to_string() + "ms").into(),
                            None => " Offline ".red()
                        },
                        server.hostname.clone().bold(),
                        format!(":{PORT} ").gray(),
                    ]);
                    lines_vec.push(
                        match i == self.selected_server {
                            true => line.bg(DARK_GRAY),
                            false => line
                        }
                    );
                }
                
                let add_server_line = Line::from(vec![
                    " + ".green().bold(),
                    "Add Server ".bold()
                ]);
                lines_vec.push(
                    match is_server {
                        true => add_server_line,
                        false => add_server_line.bg(DARK_GRAY),
                    }
                );


                let keybinds = match is_server {
                    true => Line::from(vec![
                        " Move Cursor ".bold(), "<Up/Down>  ".cyan().bold(),
                        "Connect ".bold(), "<Enter>  ".cyan().bold(),
                        "Edit ".bold(), "<E>  ".cyan().bold(),
                        "Remove ".bold(), "<DEL>  ".cyan().bold(),
                        "New connection ".bold(), "<N>  ".cyan().bold(),
                        "Exit ".bold(), "<Q> ".cyan().bold()
                    ]),
                    false => Line::from(vec![
                        " Move Cursor ".bold(), "<Up/Down>  ".cyan().bold(),
                        " New connection ".bold(), "<Enter> ".cyan().bold()
                    ])
                };

                let block = Block::bordered()
                    .border_set(border::DOUBLE)
                    .title(title.centered())
                    .title_bottom(keybinds.centered());

                let text = Text::from(lines_vec);
                Paragraph::new(text)
                    .block(block)
                    .render(area, buf);
            },
            Screen::Connected => {

            },
            Screen::New => { 
                let keybinds = Line::from(vec![
                    " Confirm ".bold(), "<Enter>  ".cyan().bold(),
                    "Cancel ".bold(), "<Esc>  ".cyan().bold()
                ]);

                let center = Layout::horizontal(vec![
                    Constraint::Min(1),
                    Constraint::Length(90),
                    Constraint::Min(1),
                ]).split(area);

                let vertical = Layout::vertical(vec![
                    Constraint::Min(1),
                    Constraint::Length(3),
                    Constraint::Min(1)
                ]).split(center[1]);

                let block = Block::bordered()
                    .border_set(border::DOUBLE)
                    .title(Line::from(" Hostname ").centered())
                    .title_bottom(keybinds.centered());

                let text = vec![
                    " ".into(), 
                    self.editing_server.hostname.clone().into(), 
                    if SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() % 1000 < 500 {
                        "_".cyan()
                    } else {
                        "".into()
                    }
                ];

                Paragraph::new(Line::from(text))
                    .block(block)
                    .render(vertical[1], buf);

            },
            Screen::Status => status_text(vec![" Stuff is happening... ", " Please wait "]),
            Screen::Message => status_text(vec![" Stuff has happened ", " Press Enter to continue "]),
            Screen::Login => {
                let keybinds = match self.selected_login_field {
                    LoginField::Password => {
                        Line::from(vec![
                            " Log in ".bold(), "<Enter>  ".cyan().bold(),
                            "Cancel ".bold(), "<Esc> ".cyan().bold()
                        ])
                    },
                    LoginField::Username => {
                        Line::from(vec![
                            " Go to password field ".bold(), "<Enter>  ".cyan().bold(),
                            "Cancel ".bold(), "<Esc> ".cyan().bold()
                        ])
                    }
                };
                
                let center = Layout::horizontal(vec![
                    Constraint::Min(1),
                    Constraint::Length(90),
                    Constraint::Min(1),
                ]).split(area);

                let vertical = Layout::vertical(vec![
                    Constraint::Min(1),
                    Constraint::Length(1),
                    Constraint::Length(3),
                    Constraint::Length(1),
                    Constraint::Length(3),
                    Constraint::Min(1)
                ]).split(center[1]);

                let focused = &self.selected_login_field;

                let username = vec![
                    " ".into(), 
                    self.login.username.clone().into(), 
                    if SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() % 1000 < 500 
                        && *focused == LoginField::Username {
                        "_".cyan()
                    } else {
                        "".into()
                    }
                ];

                let password = vec![
                    " ".into(), 
                    self.login.password.clone().chars().map(|_| "*").collect::<String>().into(), 
                    if SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() % 1000 < 500 
                        && *focused == LoginField::Password {
                        "_".cyan()
                    } else {
                        "".into()
                    }
                ];
                
                Paragraph::new(Line::from("Enter login credentials. Accounts that do not exist are automatically created.").centered())
                    .render(vertical[1], buf);

                Paragraph::new(Line::from(username))
                    .block(Block::bordered()
                        .border_set(border::DOUBLE)
                        .title(Line::from(" Username ")))
                    .render(vertical[2], buf);

                Paragraph::new(Line::from(password))
                    .block(Block::bordered()
                        .border_set(border::DOUBLE)
                        .title(Line::from(" Password "))
                        .title_bottom(keybinds.right_aligned()))
                    .render(vertical[4], buf);
            }
        }      
    }
}

fn ping(domain: String) -> Option<u128> {
    let addr = format!("{domain}:{PORT}");
    let start = Instant::now();

    if let Ok(mut addrs) = addr.to_socket_addrs() {
        if let Some(socket) = addrs.next() {
            if std::net::TcpStream::connect_timeout(&socket, Duration::from_secs(1)).is_ok() {
                return Some(start.elapsed().as_millis())
            }
        }
    };

    None
}

fn app_next_fn_placeholder(_: &mut App) {}

#[tokio::main]
async fn main () -> io::Result<()> {
    let config = match Path::new(CONFIG_PATH).exists() {
        true => {
            let file = fs::read_to_string(CONFIG_PATH)?;
            if let Ok(cfg) = serde_json::from_str(&file) {
                cfg
            } else {
                Config::default()
            }
        },
        false => {
            let mut file = File::create(CONFIG_PATH)?;
            file.write_all(b"{}")?;
            Config::default()
        }
    };

    let servers: Vec<Server> = config.servers
        .into_iter()
        .map(|s| Server {hostname: s})
        .collect();


    let ping_times_raw: Vec<Option<u128>> = vec![None; servers.len()];
    let ping_times = Arc::new(Mutex::new(ping_times_raw));
    let thread_ping_times = Arc::clone(&ping_times);
    
    let terminal = ratatui::init();
    let mut app = App {
        screen: Default::default(),
        servers: Default::default(),
        selected_server: Default::default(),
        server_selection_scroll_size: Default::default(),
        scroll_offset: Default::default(),
        ping_times: Default::default(),
        editing_server: Default::default(),
        editing_existing_index: Default::default(),
        exit: Default::default(),
        status_text: Default::default(),
        next_fn: Some(Box::new(app_next_fn_placeholder)),
        terminal: Some(terminal),
        connected_socket: Default::default(),
        selected_login_field: LoginField::Username,
        login: Default::default(),
        session: Default::default()
    };
    app.servers = servers;
    app.ping_times = ping_times;

    // channel stuff

    let (sender, receiver): (Sender<Vec<Server>>, Receiver<Vec<Server>>) = mpsc::channel();

    thread::spawn(move || {
        let mut hosts: Vec<Server> = vec![];
        loop {
            // get new data
            while let Ok(servers) = receiver.try_recv() {
                hosts = servers
            }

            // get the latencies of each server

            let ping_threads = hosts.clone()
                .into_iter()
                .map(|host| {
                    thread::spawn( move || {
                        ping(host.hostname)
                    })
                })
                .collect::<Vec<JoinHandle<Option<u128>>>>();

            let ping_results = ping_threads
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<Option<u128>>>();

            {
                let mut latencies = thread_ping_times.lock().unwrap();
                *latencies = ping_results;
                // todo: ping the server (and maek the backend)
            }

            thread::sleep(Duration::from_millis(500))
        }
    });
    
    let app_result = app.run(sender).await;
    ratatui::restore(); 
    app_result
}