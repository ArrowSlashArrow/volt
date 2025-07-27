use std::{
    any::type_name, collections::HashMap, fmt::Debug, fs::{
        self, File
    }, io::{
        self, 
        Write
    }, net::ToSocketAddrs, path::Path, process, sync::{
        mpsc::{self, Receiver, Sender}, Arc, Mutex
    }, thread::{self, JoinHandle}, time::{
        Duration, Instant, SystemTime, UNIX_EPOCH
    }
};
use futures_util::{stream::{SplitSink, SplitStream}, SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async, tungstenite::{
        client::IntoClientRequest, http::{Response}, Error, Message
    }, MaybeTlsStream, WebSocketStream
};

use crossterm::{event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers}};
use ratatui::{
    crossterm::terminal::disable_raw_mode, layout::{Constraint, Layout}, prelude::{
        Buffer, Rect
    }, style::{Color, Stylize}, symbols::border, text::{
        Line, Span, Text
    }, widgets::{
        Block, Paragraph, Widget
    }, DefaultTerminal, Frame
};
use sha2::{self, digest::crypto_common::Key, Digest, Sha256};
use serde::{Deserialize, Serialize};
use serde_json::{to_string, to_string_pretty, Value, from_value};
use x25519_dalek::{EphemeralSecret, PublicKey};
use chrono::prelude::*;
use copypasta::{ClipboardContext, ClipboardProvider};

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
        let _ = self.write.send(data.into()).await;
    }

    pub async fn read(&mut self) -> Option<Result<Message, Error>> {
        self.read.next().await
    }
}

#[derive(Default, Debug, Clone)]
pub struct Msg {
    msg: String,
    id: u64,
    user: String,
    time: u32,
    replying_to: Option<u64>
}

#[derive(Default)]
pub struct CurrentServer {
    channels: Vec<String>,
    msgs: HashMap<String, Vec<Msg>>
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
    session: String,
    current_user: String,
    typing: bool,
    replying_to: Option<u64>,
    current_server: CurrentServer,
    private: EphemeralSecret,
    public: PublicKey,
    clipboard: Box<dyn ClipboardProvider>,
    new_message_buf: String,
    sidebar: bool,
    selected_channel: Option<usize>,
    selected_msg_ids: HashMap<String, Option<u64>>,
    selected_msg_idcs: HashMap<String, Option<usize>> // index in the corresponding messages vec
}

#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    servers: Vec<String>
}

fn go_to_selection() -> Box<dyn Fn(&mut App)> {
    return Box::new(|app: &mut App| {app.go_to_selection()})
}

fn retry_login() -> Box<dyn Fn(&mut App)> {
    return Box::new(|app: &mut App| {app.retry_login()})
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

fn generate_keys() -> (EphemeralSecret, PublicKey) {
    let private = EphemeralSecret::random();
    let public = PublicKey::from(&private);
    return (private, public)
}

// converts dict to msg object
fn unpack_msg(raw_msg: &HashMap<String, String>) -> Msg {
    return Msg { 
        msg: raw_msg.get("msg").unwrap().clone(), 
        id: raw_msg.get("time").unwrap().parse::<u64>().unwrap(),  // use time as id
        user: raw_msg.get("user").unwrap().clone(), 
        time: (raw_msg.get("time").unwrap().parse::<u64>().unwrap() / 1000) as u32,
        replying_to: match raw_msg.get("replying_to").unwrap().parse::<u64>() {
            Ok(v) => Some(v),
            Err(_) => None
        } 
    }
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
        self.selected_login_field = LoginField::Username;
        self.login = Default::default();
        self.screen = Screen::Selection;
    }

    pub fn retry_login(&mut self) {
        self.selected_login_field = LoginField::Username;
        self.login = Default::default();
        self.screen = Screen::Login;
    }

    pub async fn connect(&mut self) {
        // todo: make a WSS for tls (and e2ee eventually)
        let url = format!("ws://{}:{PORT}", self.servers[self.selected_server].hostname).into_client_request().unwrap();
        self.status("Connecting...");
        let res: Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response<Option<Vec<u8>>>), Error> = connect_async(url).await;
        match res {
            Ok((ws_stream, response)) => {
                // ws_stream is a WebSocketStream<MaybeTlsStream<TcpStream>>
                
                let (mut write, mut read) = ws_stream.split();

                if let Some(msg) = read.next().await {
                    match msg {
                        Ok(text) => {
                            if text.to_string() == "already connected".to_string() {
                                self.message("You are already connected to the server on this IP.", go_to_selection());
                                return;
                            } else {
                                self.status(text.to_string());
                            }
                        },
                        Err(_) => {
                            self.message("Server threw an error.", go_to_selection());
                            return;
                        }
                    }
                };

                self.status(
                    format!("Connected to the server. ({})", response.status())
                );

                let _ = write.send(payload("session", HashMap::new()).into()).await;
                if let Some(msg) = read.next().await {
                    match msg {
                        Ok(v) => {
                            self.connected_socket = Some(ConnectedSocket { write: write, read: read });
                            let raw = v.into_text().unwrap().to_string();
                            let json: Value = serde_json::from_str(&raw).unwrap();
                            self.session = json["session"].as_str().unwrap().to_string();
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
                            self.status("Logged in successfully. Fetching server data...");
                        },
                        false => {
                            let reason = json["reason"].as_str().unwrap();
                            self.message(format!("Login failed: {reason}"), retry_login());
                            return;
                        }
                    }
                }
                Err(_) => self.message("Failed to log in.", go_to_selection())
            }
        }

        self.screen = Screen::Connected;
        // fetch data
        socket.send(payload("data", self.session_template())).await;
        if let Some(raw) = socket.read().await {
            match raw {
                Ok(data) => {
                    self.selected_msg_idcs = HashMap::new();
                    self.selected_msg_ids = HashMap::new();
                    let json: Value = serde_json::from_str(data.to_string().as_str()).unwrap();
                    let mut msgs: HashMap<String, Vec<Msg>> = HashMap::new();
                    let channels: Vec<String> = from_value(json["channels"].clone()).unwrap();
                    let raw_msg_lists: HashMap<String, Vec<HashMap<String, String>>> = from_value(json["msgs"].clone()).unwrap();
                    for channel in channels.iter() {
                        msgs.insert(
                            channel.clone(),
                            raw_msg_lists.get(channel).unwrap().iter()
                                .map(|msg| {unpack_msg(msg)}).collect()
                        );
                        self.selected_msg_idcs.insert(channel.clone(), None);
                        self.selected_msg_ids.insert(channel.clone(), None);
                    } 
 
                    if channels.len() > 0 {
                        self.selected_channel = Some(0);
                    }
                    self.current_server = CurrentServer {channels: channels, msgs}
                },
                Err(_) => {
                    self.message("Failed to fetch server data", go_to_selection());
                    return;
                }
            }
        };
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

    pub async fn connected_keybinds(&mut self, key_event: KeyEvent) {
        let mut selected_channel: Option<String> = None;

        if let Some(idx) = self.selected_channel {
            selected_channel = Some(self.current_server.channels[idx].clone());
        }
        let prev_msg = |app: &mut Self| {
            let idcs_array = app.selected_msg_idcs.clone();
            let selected_msg_idx = idcs_array.get(&selected_channel.clone().unwrap()).unwrap();
            let new_idx = match selected_msg_idx {
                Some(idx) => if idx > &0usize {
                    idcs_array.get(&selected_channel.clone().unwrap()).unwrap().unwrap() - 1usize
                } else {
                    0
                },
                None => {
                    app.current_server.msgs[&selected_channel.clone().unwrap()].len() - 1
                }
            };

            if let Some(idx) = app.selected_msg_idcs.get_mut(&selected_channel.clone().unwrap()) {
                *idx = Some(new_idx);
            };
            if let Some(id) = app.selected_msg_ids.get_mut(&selected_channel.clone().unwrap()) {
                *id = Some(app.current_server.msgs[&selected_channel.clone().unwrap()][new_idx].id);
            };
            app.typing = false;
        };

        let next_msg = |app: &mut Self| {
            let idcs_array = app.selected_msg_idcs.clone();
            let selected_msg_idx = idcs_array.get(&selected_channel.clone().unwrap()).unwrap();
            if let Some(_) = selected_msg_idx {
                let new_idx = idcs_array.get(&selected_channel.clone().unwrap()).unwrap().unwrap() + 1usize;
                if new_idx < app.current_server.msgs[&selected_channel.clone().unwrap()].len() {
                    if let Some(idx) = app.selected_msg_idcs.get_mut(&selected_channel.clone().unwrap()) {
                        *idx = Some(new_idx);
                    };
                    if let Some(id) = app.selected_msg_ids.get_mut(&selected_channel.clone().unwrap()) {
                        *id = Some(app.current_server.msgs[&selected_channel.clone().unwrap()][new_idx].id);
                    }   
                }
            };
            app.typing = false;
        };
        if self.typing {
            match key_event.code {
                KeyCode::Esc => self.typing = false,
                KeyCode::Char(c) => {
                    self.new_message_buf.push(c)
                },
                KeyCode::Backspace => {
                    self.new_message_buf.pop();
                },
                KeyCode::PageUp => {
                    if let Some(idx) = self.selected_channel {
                        if idx > 0 {
                            self.selected_channel = Some(idx - 1);
                        }
                    }
                },
                KeyCode::PageDown => {
                    let length = self.current_server.channels.len();
                    match self.selected_channel {
                        Some(idx) => {
                            if idx < length - 1 {
                                self.selected_channel = Some(idx)
                            }
                        },
                        None => {
                            if length > 0 {
                                self.selected_channel = Some(0)
                            }
                        }
                    }
                },
                KeyCode::Enter => {
                    if let Some(channel) = self.selected_channel {
                        // send the msg
                    }
                },
                KeyCode::Up => prev_msg( self),
                KeyCode::Down => next_msg( self),
                KeyCode::Char('v') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                    // todo: paste
                }
                _ => {}
            }
        } else {
            match key_event.code {
                KeyCode::Up => prev_msg(self),
                KeyCode::Down => next_msg(self),
                KeyCode::Esc => {
                    let mut socket = self.connected_socket.take().unwrap();
                    let _ = socket.write.send(Message::Close(None)).await;
                    self.go_to_selection();
                },
                KeyCode::Tab => self.typing = true,
                KeyCode::PageUp => {
                    if let Some(idx) = self.selected_channel {
                        if idx > 0 {
                            self.selected_channel = Some(idx - 1);
                        }
                    }
                },
                KeyCode::PageDown => {
                    let length = self.current_server.channels.len();
                    match self.selected_channel {
                        Some(idx) => {
                            if idx < length - 1 {
                                self.selected_channel = Some(idx)
                            }
                        },
                        None => {
                            if length > 0 {
                                self.selected_channel = Some(0)
                            }
                        }
                    }
                },
                KeyCode::Char('t') => self.sidebar = !self.sidebar,
                KeyCode::Char('r') => {
                    if let Some(selected) = self.selected_msg_ids[&selected_channel.clone().unwrap()] {
                        let messages = self.current_server.msgs[&selected_channel.clone().unwrap()].clone();
                        let msg_id = messages.iter().find(|msg| selected == msg.id).unwrap().id;
                        self.replying_to = Some(msg_id);
                    }
                },
                KeyCode::Char('d') => {
                    if let Some(v) = self.selected_msg_ids.get_mut(&selected_channel.clone().unwrap()) {
                        *v = None;
                    };
                    if let Some(v) = self.selected_msg_idcs.get_mut(&selected_channel.clone().unwrap()) {
                        *v = None;
                    };
                },
                KeyCode::Char('u') => {
                    self.replying_to = None;
                }
                KeyCode::Char('o') => {
                    if let Some(selected) = self.selected_msg_ids[&selected_channel.clone().unwrap()] {
                        let mut replying_to: Option<u64> = None;
                        let mut replying_idx: Option<usize> = None;

                        let original = self.current_server.msgs[&selected_channel.clone().unwrap()].iter().find(|msg| selected == msg.id).unwrap();
                        if let Some(original_id) = original.replying_to {
                            for (i, msg) in self.current_server.msgs[&selected_channel.clone().unwrap()].iter().enumerate() {
                                if msg.id == original_id {
                                    replying_to = Some(msg.id);
                                    replying_idx = Some(i);
                                }
                            };
                        }
                        
                        if let Some(id) = replying_to {
                            if let Some(v) = self.selected_msg_ids.get_mut(&selected_channel.clone().unwrap()).unwrap() {
                                *v = id
                            }
                            if let Some(v) = self.selected_msg_idcs.get_mut(&selected_channel.clone().unwrap()).unwrap() {
                                *v = replying_idx.unwrap()
                            }
                        }
                    }
                },
                KeyCode::Char('c') => {
                    if let Some(selected) = self.selected_msg_ids[&selected_channel.clone().unwrap()] {
                        let msg = self.current_server.msgs[&selected_channel.clone().unwrap()].iter().find(|msg| msg.id == selected).unwrap();
                        let _ = self.clipboard.set_contents(msg.msg.clone());
                    }
                }
                _ => {}
            }
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
                            self.current_user = self.login.username.clone();
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
                    Screen::Connected => self.connected_keybinds(key_event).await,
                    Screen::New       => self.new_keybinds(key_event),
                    Screen::Status    => self.status_keybinds(key_event),
                    Screen::Message   => self.message_keybinds(key_event),
                    Screen::Login     => self.login_keybinds(key_event).await
                };
            }
        }
        Ok(())
    }
}

fn timestamp_str(timestamp: u32) -> String {
    let datetime = DateTime::from_timestamp(timestamp as i64, 0);
    let timestamp_str = format!("{}", datetime.unwrap().with_timezone(&chrono::Local));
    match timestamp_str.rfind(" ") {
        Some(pos) => timestamp_str[..pos].to_string(),
        None => timestamp_str
    }
}

fn render_msg(msg: &Msg, channel: &Vec<Msg>, width: u16, selected_id: &Option<u64>) -> Vec<Line<'static>> {
    let time = match width > 100 {
        false => timestamp_str(msg.time).split(" ").collect::<Vec<&str>>()[1].to_string(),
        true => timestamp_str(msg.time)
    };
    let mut header_vec: Vec<Span> = vec![" ".into(), time.into(), format!("  {} ", msg.user).into()];

    let mut used_chars = header_vec[0].content.chars().count() + header_vec[1].content.chars().count();
    let max_text_length = width as usize - 6;

    if let Some(reply_id) = msg.replying_to {
        // assign these message ids according to their timestamp so that you eliminate the possibilty of id collision
        let reply = channel.iter().find(|msg| msg.id == reply_id);
        header_vec.push(match reply {
            Some(reply) => {
                used_chars += 18 + reply.user.len();
                let space_left = width as usize - used_chars;
                let mut reply_content = reply.msg.clone();
                if reply_content.len() >= space_left {
                    reply_content.truncate(space_left - 1);
                    reply_content.push('…')
                }
                format!("[ \u{f17ab} {}: {}] ", reply.user, reply_content).into()
            },
            None => "[ \u{f17ab} ???] ".into()
        });
    }

    let header = Line::from(header_vec);
    let mut msg_lines = vec![header];
    
    let mut current_line = "".to_string();
    let mut current_width = 0usize;
    // greedy word wrapper
    for word in msg.msg.split(' ').collect::<Vec<&str>>() {
        let word_width = word.chars().count();

        if current_width + word_width + current_line.is_empty() as usize <= max_text_length {
            if !current_line.is_empty() {
                current_line.push(' ');
            }
            current_line += word
        } else {
            if current_line.len() >= max_text_length {
                current_line.truncate(max_text_length - 1);
                current_line.push('…')
            }
            msg_lines.push(format!("     {} ", current_line).into());
            current_line = word.to_string();
            current_width = word_width;
        }
    }

    if !current_line.is_empty() {
        msg_lines.push(format!("     {} ", current_line).into());
    }

    if let Some(selected) = selected_id &&& msg.id == selected {
        msg_lines = msg_lines.iter().map(|line| line.clone().bg(DARK_GRAY)).collect();
    }

    return msg_lines
}

fn dbg_write<T: Debug>(val: T) {
    let prev = fs::read_to_string("debug").unwrap();
    let new = format!(
        "[{:.3}] {val:?}: {}\n", 
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as f64 / 1000.0,
        type_name::<T>()
    );

    let _ = fs::write("debug", format!("{prev}{new}"));
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
                let layout = Layout::vertical(vec![
                    Constraint::Length(2),
                    Constraint::Min(1),
                ]).split(area);

                let is_server = self.selected_server < servers.len();

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
                        "Exit ".bold(), "<ESC> ".cyan().bold()
                    ]),
                    false => Line::from(vec![
                        " Move Cursor ".bold(), "<Up/Down>  ".cyan().bold(),
                        " New connection ".bold(), "<Enter> ".cyan().bold()
                    ])
                };

                let block = Block::bordered()
                    .border_set(border::DOUBLE)
                    .title(Line::from(format!(" Server Selection ").bold()).centered())
                    .title_bottom(keybinds.centered());

                let text = Text::from(lines_vec);
                Paragraph::new(text)
                    .block(block)
                    .render(layout[1], buf);

                Paragraph::new(Text::from(vec![
                    Line::from("Welcome to Volt IRC Client!").centered(),
                    Line::from("It is recommended that you use a nerd font").centered()
                ]))
                    .render(layout[0], buf);
            },
            Screen::Connected => {
                let horizontal = Layout::horizontal(vec![
                    Constraint::Percentage(25),
                    Constraint::Length(1), // spacing
                    Constraint::Min(1)
                ]).split(area);

                let message_area = Layout::vertical(vec![
                    Constraint::Min(1),
                    Constraint::Length(3)
                ]).split(match self.sidebar {
                    true => horizontal[2],
                    false => area
                });

                let width = message_area[1].width as usize - 4;
                let buf_length = self.new_message_buf.len();
                let visible = match buf_length > width {
                    true => self.new_message_buf[(buf_length - width)..].to_string(),
                    false => self.new_message_buf.clone()
                };

                let new_message_line = match self.new_message_buf.is_empty() {
                    true => vec![
                        " ".into(), 
                        match self.typing {true => "N".to_string().gray().on_cyan(), false => "N".to_string().dark_gray()}, 
                        "ew Message...".to_string().dark_gray()],
                    false => vec![
                        " ".into(), 
                        visible.into(), 
                        match self.typing {true => " ", false => ""}.to_string().on_cyan()
                    ],
                };

                let mut channels = self.current_server.channels.iter()
                    .map(|c| Line::from(format!(" #{c} "))).collect::<Vec<Line>>();

                let mut selected_channel: Option<String> = None;

                if let Some(idx) = self.selected_channel {
                    channels[idx] = channels[idx].clone().bg(DARK_GRAY);
                    selected_channel = Some(self.current_server.channels[idx].clone());
                }

                let msg_buf_height = message_area[0].height as usize;
                let msg_buf_width = message_area[0].width;

                let messages = match self.current_server.msgs.get(
                    &selected_channel.clone().unwrap()
                ) {
                    Some(v) => v,
                    None => &vec![]
                };

                // need to handle three cases:
                // 0. buffer is big enough to hold all messages
                // 1. selected is in the existing buffer and no scroll ups needs to be made
                // 2. start at selected and go until no more room

                // get all messages (at most buf_height / 2 + 1 msgs)
                let mut visible_msgs: Vec<Line> = vec![];
                // id: starting line of that msg
                let mut positions: HashMap<u64, Vec<usize>> = HashMap::new();

                let selected_msg_id = self.selected_msg_ids.get(
                    &selected_channel.clone().unwrap_or("".to_string())
                ).unwrap_or(&Some(0u64));
                
                for msg in messages.iter() {
                    let mut formatted = render_msg(msg, messages, msg_buf_width, selected_msg_id);
                    positions.insert(msg.id, vec![visible_msgs.len(), formatted.len()]);
                    visible_msgs.append(&mut formatted);
                }
                
                let mut scroll_offset = 0;
                if visible_msgs.len() > msg_buf_height {
                    let start_at_selected = match selected_msg_id {
                        Some(id) => visible_msgs.len() - msg_buf_height > std::cmp::max(positions[id][0] as i32 - positions[id][1] as i32, 0) as usize,
                        None => false
                    };
                    scroll_offset = match start_at_selected {
                        true => {
                            positions.get(&selected_msg_id.unwrap()).unwrap()[0]
                        },
                        false => visible_msgs.len() - msg_buf_height + 2
                    }
                }

                // keybinds
                let keybinds = match self.typing {
                    true => vec![
                        vec!["Send message", "enter"],
                        vec!["Select message", "up/down"],
                        vec!["Previous channel", "page up"],
                        vec!["Next channel", "page dn"],
                        vec!["Exit typing mode", "esc"],
                        vec!["DOES NOT COPY!", "CTRL-C"]
                    ],
                    false => {
                        let mut keybind_vec = vec![
                            vec!["Enter typing mode", "tab"],
                            vec!["Previous channel", "page up"],
                            vec!["Next channel", "page dn"],
                            vec!["Select message", "up/down"],
                            vec!["Leave server", "esc"],
                            vec!["Toggle sidebar", "t"]
                        ];

                        if let Some(id) = selected_msg_id {
                            keybind_vec.push(match self.replying_to {
                                Some(_) => vec!["Unreply to selected", "u"],
                                None => vec!["Reply to selected", "r"]
                            });
                            keybind_vec.push(vec!["Deselect message", "d"]);

                            let replying_to = self.current_server.msgs[&selected_channel.clone().unwrap()].iter().find(|msg| msg.id == *id).unwrap().replying_to;
                            keybind_vec.push(vec!["Copy message", "c"]);
                            match replying_to {
                                Some(_) => {
                                    keybind_vec.push(vec!["Go to original", "o"]);
                                },
                                None => {}
                            }
                        };

                        keybind_vec
                    }
                };

                let sidebar = Layout::vertical(vec![
                    Constraint::Min(1),
                    Constraint::Length(keybinds.len() as u16 + 2)
                ]).split(horizontal[0]);

                let keybind_text = keybinds
                    .iter()
                    .map(|kb| Line::from(vec![format!(" {:<8}", kb[1]).cyan(), kb[0].into()]))
                    .collect::<Text>();

                let channels_area = sidebar[0];
                let keybind_area = sidebar[1];

                Paragraph::new(Text::from(visible_msgs))
                    .block(Block::bordered()
                        .border_set(border::DOUBLE))
                    .scroll((scroll_offset as u16, 0))
                    .render(message_area[0], buf);

                if self.sidebar {
                    Paragraph::new(Text::from(channels))
                        .block(Block::bordered()
                            .border_set(border::DOUBLE)
                            .title(Line::from(" Channels ").centered()))
                        .render(channels_area, buf);

                    Paragraph::new(keybind_text)
                        .block(Block::bordered()
                            .border_set(border::DOUBLE)
                            .title(Line::from(" Keybinds ").centered()))
                        .render(keybind_area, buf);
                }                

                let mut msg_block = Block::bordered()
                    .border_set(border::DOUBLE)
                    .title(Line::from(" ".to_owned() + &timestamp_str(
                        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as u32
                    ) + " ").right_aligned());

                if let Some(reply_id) = self.replying_to {
                    let msg = messages.iter().find(|msg| msg.id == reply_id);
                    msg_block = msg_block.clone().title_bottom(Line::from(match msg {
                        Some(msg) => format!(
                            " Replying to {} on {} ",
                            msg.user,
                            timestamp_str(msg.time)
                        ),
                        None => format!(" Replying to [unloaded message] ")
                    }).right_aligned())
                } else if let Some(reply_id) = self.selected_msg_ids[&selected_channel.clone().unwrap()] {
                    let msg = messages.iter().find(|msg| msg.id == reply_id);
                    msg_block = msg_block.clone().title_bottom(Line::from(match msg {
                        Some(msg) => format!(
                            " Selected from {} on {} ",
                            msg.user,
                            timestamp_str(msg.time)
                        ),
                        None => format!(" Selected a non-existant message??? ")
                    }).right_aligned())
                }

                Paragraph::new(Line::from(new_message_line))
                    .block(msg_block)
                    .render(message_area[1], buf);
                                
            }
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
                    " ".on_cyan()
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
                    if *focused == LoginField::Username {
                        " ".on_cyan()
                    } else {
                        "".into()
                    }
                ];

                let password = vec![
                    " ".into(), 
                    self.login.password.clone().chars().map(|_| "*").collect::<String>().into(), 
                    if *focused == LoginField::Password {
                        " ".on_cyan()
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

    let (private, public) = generate_keys();
    
    let clipboard = ClipboardContext::new().unwrap();
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
        next_fn: Some(nop()),
        terminal: Some(terminal),
        clipboard: Box::new(clipboard),
        connected_socket: Default::default(),
        selected_login_field: LoginField::Username,
        login: Default::default(),
        session: Default::default(),
        typing: false,
        replying_to: None,
        current_server: Default::default(),
        current_user: String::new(),
        public: public,
        private: private,
        new_message_buf: String::new(),
        sidebar: true,
        selected_channel: None,
        selected_msg_ids: Default::default(),
        selected_msg_idcs: Default::default()
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

    // incase the app freezes
    tokio::spawn(async {
        tokio::signal::ctrl_c().await.unwrap();
        disable_raw_mode().unwrap();
        ratatui::restore();
        process::exit(0);
    });
    
    let app_result = app.run(sender).await;
    ratatui::restore(); 
    app_result
}