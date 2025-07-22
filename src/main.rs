use std::{
    io, net::ToSocketAddrs, sync::{
        mpsc::{self, Receiver, Sender}, Arc, Mutex
    }, thread::{self, JoinHandle}, time::{
        Duration, Instant, SystemTime, UNIX_EPOCH
    }
};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    layout::{Constraint, Layout}, prelude::{
        Buffer, Rect
    }, style::Stylize, symbols::border, text::{
        Line, Text
    }, widgets::{
        Block, Paragraph, Widget
    }, DefaultTerminal, Frame
};

#[derive(Clone, Debug, Default)]
pub struct Server {
    hostname: String
}

#[derive(Debug, Default)]
pub struct App {
    screen: u8, // 0: server selection, 1: in server, 2: configuring new connection
    servers: Vec<Server>,
    selected_server: usize,
    server_selection_scroll_size: u16,
    scroll_offset: i32,
    ping_times: Arc<Mutex<Vec<Option<u128>>>>,
    editing_server: Server, // the server that is in the new connection screen
    editing_existing_index: Option<usize>,
    exit: bool,
}

impl App {
    pub fn run(&mut self, terminal: &mut DefaultTerminal, sender: Sender<Vec<Server>>) -> io::Result<()> {
        while !self.exit {
            let _ = sender.send(self.servers.clone());
            terminal.draw(|frame| self.draw(frame))?;
            self.event_handler()?;
           
        }
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
        self.screen = 2;
    }

    pub fn event_handler(&mut self) -> io::Result<()> {
        if event::poll(Duration::from_millis(0))? {
            if let Event::Key(key_event) = event::read()? {
                if key_event.kind == KeyEventKind::Press {
                    let servers = &mut self.servers;
                    match self.screen {
                        0 => {
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
                                            // todo: connect to the server
                                        }
                                    }
                                },
                                KeyCode::Char('e') => {
                                    if cursor_on_server {
                                        self.new_server(true);
                                    }
                                    self.screen = 2;
                                    // todo: new connection with values filled in
                                },
                                KeyCode::Delete => {
                                    if cursor_on_server {
                                        self.servers.remove(self.selected_server);
                                    }
                                },
                                KeyCode::Char('n') => {
                                    self.new_server(false);
                                }
                                KeyCode::Char('q') => {
                                    self.exit = true;
                                },
                                _ => {}
                            }
                        },
                        1 => {
                            match key_event.code {
                                KeyCode::Char('q') => {
                                    self.exit = true;
                                },
                                _ => {}
                            }
                        },
                        2 => {
                            match key_event.code {
                                KeyCode::Esc => {
                                    self.screen = 0;
                                },
                                KeyCode::Enter => {
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
                                    self.screen = 0;
                                },
                                KeyCode::Char(input) => {
                                    self.editing_server.hostname.push(input);
                                },
                                KeyCode::Backspace => {
                                    self.editing_server.hostname.pop();
                                }
                                _ => {}
                            }
                        },
                        _ => {}
                    };
                }
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
        match self.screen {
            0 => { // server selection
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
                        match ping_times[i] {
                            Some(v) => format!(" {:<7} ", v.to_string() + "ms").into(),
                            None => " Offline ".red()
                        },
                        server.hostname.clone().bold(),
                        ":38338 ".gray(),
                    ]);
                    lines_vec.push(
                        match i == self.selected_server {
                            true => line.on_dark_gray(),
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
                        false => add_server_line.on_dark_gray()
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
            1 => { // connected to server

            }, // todo
            2 => { // new connection config
                let keybinds = Line::from(vec![
                    " Confirm ".bold(), "<Enter>  ".cyan().bold(),
                    "Cancel ".bold(), "<Esc>  ".cyan().bold()
                ]);

                let vertical = Layout::vertical(vec![
                    Constraint::Min(1),
                    Constraint::Length(3),
                    Constraint::Min(1)
                ]).split(area);

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

                Paragraph::new( Line::from(text))
                    .block(block)
                    .render(vertical[1], buf);

            }, // todo
            _ => {}
        }      
    }
}

fn ping(domain: String) -> Option<u128> {
    // port 38338 is what the backend is gonna use 
    let addr = format!("{domain}:38338");
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

fn main () -> io::Result<()> {
    let servers = vec![
        Server { hostname: "bitfeller.uk".to_string()}; 1
    ];
    let ping_times_raw: Vec<Option<u128>> = vec![Some(100); servers.len()];
    let ping_times = Arc::new(Mutex::new(ping_times_raw));
    let thread_ping_times = Arc::clone(&ping_times);
    
    let mut terminal = ratatui::init();
    let mut app = App::default();
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
    
    let app_result = app.run(&mut terminal, sender);
    ratatui::restore(); 
    app_result
}