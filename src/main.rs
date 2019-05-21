// Spec found at https://tools.ietf.org/html/rfc959

extern crate mini;
extern crate time;

mod cmd;
mod error;
mod ftp;

use std::env;
use std::ffi::OsString;
use std::fs::{File, Metadata, create_dir, read_dir, remove_dir_all};
use std::io::{self, Read, Write};
use std::mem;
use std::net;
use std::path::{Component, Path, PathBuf, StripPrefixError};
use std::result;

use mini::aio::handler::{
    Handler,
    Loop,
    Stream,
};
use mini::aio::net::{
    ListenerMsg,
    TcpConnection,
    TcpConnectionNotify,
    TcpListener,
    TcpListenNotify,
};

use cmd::{Command, TransferType};
use error::{Error, Result};
use ftp::{Answer, ResultCode};
use self::Msg::*;

const MONTHS: [&'static str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun",
                                    "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

static ADMIN_USER: Option<User> = None;
static USERS: [User; 1] = [
    User {
        name: "test",
        password: "password",
    },
];

struct User {
    name: &'static str,
    password: &'static str,
}

#[cfg(windows)]
fn get_file_info(meta: &Metadata) -> (time::Tm, u64) {
    use std::os::windows::prelude::*;
    (time::at(time::Timespec::new((meta.last_write_time() / 10_000_000) as i64, 0)),
    meta.file_size())
}

#[cfg(unix)]
fn get_file_info(meta: &Metadata) -> (time::Tm, u64) {
    use std::os::unix::prelude::*;
    (time::at(time::Timespec::new(meta.mtime(), 0)), meta.size())
}

// If an error occurs when we try to get file's information, we just return and don't send its info.
fn add_file_info(path: PathBuf, out: &mut Vec<u8>) {
    let extra = if path.is_dir() { "/" } else { "" };
    let is_dir = if path.is_dir() { "d" } else { "-" };

    let meta = match ::std::fs::metadata(&path) {
        Ok(meta) => meta,
        _ => return,
    };
    let (time, file_size) = get_file_info(&meta);
    let path = match path.to_str() {
        Some(path) => match path.split("/").last() {
            Some(path) => path,
            _ => return,
        },
        _ => return,
    };
    // TODO: maybe improve how we get rights in here?
    let rights = if meta.permissions().readonly() {
        "r--r--r--"
    } else {
        "rw-rw-rw-"
    };
    let file_str = format!("{is_dir}{rights} {links} {owner} {group} {size} {month} {day} {hour}:{min} {path}{extra}\r\n",
                           is_dir=is_dir,
                           rights=rights,
                           links=1, // number of links
                           owner="anonymous", // owner name
                           group="anonymous", // group name
                           size=file_size,
                           month=MONTHS[time.tm_mon as usize],
                           day=time.tm_mday,
                           hour=time.tm_hour,
                           min=time.tm_min,
                           path=path,
                           extra=extra);
    out.extend(file_str.as_bytes());
    //println!("==> {:?}", &file_str);
}

fn invalid_path(path: &Path) -> bool {
    for component in path.components() {
        if let Component::ParentDir = component {
            return true;
        }
    }
    false
}

fn get_parent(path: PathBuf) -> Option<PathBuf> {
    path.parent().map(|p| p.to_path_buf())
}

fn get_filename(path: PathBuf) -> Option<OsString> {
    path.file_name().map(|p| p.to_os_string())
}

fn prefix_slash(path: &mut PathBuf) {
    if !path.is_absolute() {
        *path = Path::new("/").join(&path);
    }
}

struct DataListener {
    stream: Stream<Msg>,
}

impl DataListener {
    fn new(stream: &Stream<Msg>) -> Self {
        Self {
            stream: stream.clone(),
        }
    }
}

impl TcpListenNotify for DataListener {
    fn connected(&mut self, _listener: &net::TcpListener) -> Box<TcpConnectionNotify> {
        Box::new(DataConnection::new(&self.stream))
    }
}

struct DataConnection {
    buffer: Vec<u8>,
    stream: Stream<Msg>,
}

impl DataConnection {
    fn new(stream: &Stream<Msg>) -> Self {
        Self {
            buffer: vec![],
            stream: stream.clone(),
        }
    }
}

impl TcpConnectionNotify for DataConnection {
    fn accepted(&mut self, connection: &mut TcpConnection) {
        self.stream.send(DataConnected(connection.clone()));
    }

    fn closed(&mut self, _connection: &mut TcpConnection) {
        if !self.buffer.is_empty() {
            let buffer = mem::replace(&mut self.buffer, vec![]);
            self.stream.send(DataReceived(buffer));
        }
    }

    fn received(&mut self, _connection: &mut TcpConnection, data: Vec<u8>) {
        self.buffer.extend(data);
    }

    fn sent(&mut self) {
        self.stream.send(TransferDone);
    }
}

struct Listener {
    event_loop: Loop,
    server_root: PathBuf,
}

impl Listener {
    fn new(event_loop: &Loop, server_root: PathBuf) -> Self {
        Self {
            event_loop: event_loop.clone(),
            server_root,
        }
    }
}

enum Msg {
    Accepted(TcpConnection),
    DataConnected(TcpConnection),
    DataReceived(Vec<u8>),
    Received(Vec<u8>),
    TransferDone,
}

impl TcpListenNotify for Listener {
    fn listening(&mut self, listener: &net::TcpListener) {
        match listener.local_addr() {
            Ok(address) =>
                println!("Listening on {}:{}.", address.ip(), address.port()),
            Err(error) =>
                eprintln!("Could not get local address: {}.", error),
        }
    }

    fn not_listening(&mut self) {
        eprintln!("Could not listen.");
    }

    fn connected(&mut self, _listener: &net::TcpListener) -> Box<TcpConnectionNotify> {
        //println!("Waiting another client...");
        let client = Client::new(&self.server_root, &self.event_loop);
        let stream = self.event_loop.spawn(client);
        Box::new(ClientNotify::new(stream))
    }
}

struct Client {
    connection: Option<TcpConnection>,
    current_cmd: Vec<u8>,
    cwd: PathBuf,
    data_connection: Option<TcpConnection>,
    data_port: Option<u16>,
    event_loop: Loop,
    is_admin: bool,
    listener: Option<Stream<ListenerMsg>>,
    name: Option<String>,
    store_path: Option<PathBuf>,
    server_root: PathBuf,
    transfer_type: TransferType,
    waiting_password: bool,
}

impl Client {
    fn new(server_root: &PathBuf, event_loop: &Loop) -> Self {
        Self {
            connection: None,
            current_cmd: vec![],
            cwd: PathBuf::from("/"),
            data_connection: None,
            data_port: None,
            event_loop: event_loop.clone(),
            is_admin: true,
            listener: None,
            name: None,
            store_path: None,
            server_root: server_root.clone(),
            transfer_type: TransferType::Ascii,
            waiting_password: false,
        }
    }

    fn close_data_connection(&mut self) {
        if let Some(ref connection) = self.data_connection {
            connection.dispose();
        }
        self.data_connection = None;
    }

    fn complete_path(&self, path: PathBuf) -> result::Result<PathBuf, io::Error> {
        let directory =
            self.server_root.join(if path.has_root() {
                path.iter().skip(1).collect()
            }
            else {
                path
            });
        let dir = directory.canonicalize();
        if let Ok(ref dir) = dir {
            if !dir.starts_with(&self.server_root) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
        }
        dir
    }

    fn cwd(&mut self, directory: PathBuf) -> Result<()> {
        let connection = self.connection.as_ref().expect("Received without connection");
        let path = self.cwd.join(&directory);
        let res = self.complete_path(path);
        if let Ok(dir) = res {
            let res = self.strip_prefix(dir);
            if let Ok(prefix) = res {
                self.cwd = prefix.to_path_buf();
                prefix_slash(&mut self.cwd);
                let answer = Answer::new(ResultCode::RequestedFileActionOkay,
                    &format!("Directory changed to \"{}\"", directory.display()));
                connection.write(answer.to_bytes())?;
                return Ok(())
            }
        }
        let answer = Answer::new(ResultCode::FileNotFound, "No such file or directory");
        connection.write(answer.to_bytes())?;
        Ok(())
    }

    fn handle_cmd(&mut self, command: Command, stream: &Stream<Msg>) -> Result<()> {
        //println!("Received command: {:?}", command);
        let connection = self.connection.as_ref().expect("Received without connection");
        if self.is_logged() {
            match command {
                Command::Cwd(directory) => return Ok(self.cwd(directory)?),
                Command::List(path) => return Ok(self.list(path)?),
                Command::Pasv => return Ok(self.pasv(stream)?),
                Command::Port(port) => {
                    self.data_port = Some(port);
                    let answer = Answer::new(ResultCode::Ok, &format!("Data port is now {}", port));
                    return connection.write(answer.to_bytes()).map_err(|error| error.into());
                }
                Command::Pwd => {
                    let msg = format!("{}", self.cwd.to_str().unwrap_or("")); // small trick
                    if !msg.is_empty() {
                        let message = format!("\"{}\" ", msg);
                        let answer = Answer::new(ResultCode::PATHNAMECreated, &message);
                        return connection.write(answer.to_bytes()).map_err(|error| error.into());
                    }
                    else {
                        let answer = Answer::new(ResultCode::FileNotFound, "No such file or directory");
                        return connection.write(answer.to_bytes()).map_err(|error| error.into());
                    }
                }
                Command::Retr(file) => return self.retr(file),
                Command::Stor(file) => return self.stor(file),
                Command::CdUp => {
                    if let Some(path) = self.cwd.parent().map(Path::to_path_buf) {
                        self.cwd = path;
                        prefix_slash(&mut self.cwd);
                    }
                    let answer = Answer::new(ResultCode::Ok, "Done");
                    return connection.write(answer.to_bytes()).map_err(|error| error.into());
                }
                Command::Mkd(path) => return self.mkd(path),
                Command::Rmd(path) => return self.rmd(path),
                _ => (),
            }
        }
        else if self.name.is_some() && self.waiting_password {
            if let Command::Pass(content) = command {
                let mut ok = false;
                if self.is_admin {
                    ok = content == ADMIN_USER.as_ref().unwrap().password;
                }
                else {
                    for user in &USERS {
                        if Some(user.name) == self.name.as_ref().map(|string| string.as_str()) {
                            if user.password == content {
                                ok = true;
                                break;
                            }
                        }
                    }
                }
                if ok {
                    self.waiting_password = false;
                    let name = self.name.clone().unwrap_or(String::new());
                    let answer = Answer::new(ResultCode::UserLoggedIn, &format!("Welcome {}", name));
                    connection.write(answer.to_bytes())?;
                }
                else {
                    let answer = Answer::new(ResultCode::NotLoggedIn, "Invalid password");
                    connection.write(answer.to_bytes())?;
                }
                return Ok(());
            }
        }
        match command {
            Command::Auth => {
                let answer = Answer::new(ResultCode::CommandNotImplemented, "Not implemented");
                connection.write(answer.to_bytes())?;
            },
            Command::Quit => self.quit(connection)?,
            Command::Syst => {
                let answer = Answer::new(ResultCode::Ok, "I won't tell!");
                connection.write(answer.to_bytes())?;
            }
            Command::Type(typ) => {
                self.transfer_type = typ;
                let answer = Answer::new(ResultCode::Ok, "Transfer type changed successfully");
                connection.write(answer.to_bytes())?;
            }
            Command::User(content) => {
                if content.is_empty() {
                    let answer = Answer::new(ResultCode::InvalidParameterOrArgument, "Invalid username");
                    connection.write(answer.to_bytes())?;
                }
                else {
                    let mut name = None;
                    let mut pass_required = true;

                    self.is_admin = false;
                    if let Some(ref admin) = ADMIN_USER {
                        if admin.name == content {
                            name = Some(content.clone());
                            pass_required = admin.password.is_empty() == false;
                            self.is_admin = true;
                        }
                    }
                    if name.is_none() {
                        for user in &USERS {
                            if user.name == content {
                                name = Some(content.clone());
                                pass_required = user.password.is_empty() == false;
                                break;
                            }
                        }
                    }
                    if name.is_none() {
                        let answer = Answer::new(ResultCode::NotLoggedIn, "Unknown user...");
                        connection.write(answer.to_bytes())?;
                    }
                    else {
                        self.name = name.clone();
                        if pass_required {
                            self.waiting_password = true;
                            let answer = Answer::new(ResultCode::UserNameOkayNeedPassword,
                                &format!("Login OK, password needed for {}", name.unwrap()));
                            connection.write(answer.to_bytes())?;
                        }
                        else {
                            self.waiting_password = false;
                            let answer = Answer::new(ResultCode::UserLoggedIn, &format!("Welcome {}!", content));
                            connection.write(answer.to_bytes())?;
                        }
                    }
                }
            }
            Command::NoOp => {
                let answer = Answer::new(ResultCode::Ok, "Doing nothing");
                connection.write(answer.to_bytes())?;
            },
            Command::Unknown(s) => {
                let answer = Answer::new(ResultCode::UnknownCommand, &format!("\"{}\": Not implemented", s));
                connection.write(answer.to_bytes())?;
            },
            _ => {
                // It means that the user tried to send a command while they weren't logged yet.
                let answer = Answer::new(ResultCode::NotLoggedIn, "Please log first");
                connection.write(answer.to_bytes())?;
            }
        }
        Ok(())
    }

    fn rmd(&mut self, directory: PathBuf) -> Result<()> {
        let connection = self.connection.as_ref().expect("Received without connection");
        let path = self.cwd.join(&directory);
        let res = self.complete_path(path);
        if let Ok(dir) = res {
            if remove_dir_all(dir).is_ok() {
                let answer = Answer::new(ResultCode::RequestedFileActionOkay, "Folder successfully removed");
                connection.write(answer.to_bytes())?;
                return Ok(());
            }
        }
        let answer = Answer::new(ResultCode::FileNotFound, "Couldn't remove folder");
        connection.write(answer.to_bytes())?;
        Ok(())
    }


    fn mkd(&mut self, path: PathBuf) -> Result<()> {
        let connection = self.connection.as_ref().expect("Received without connection");
        let path = self.cwd.join(&path);
        let parent = get_parent(path.clone());
        if let Some(parent) = parent {
            let parent = parent.to_path_buf();
            let res = self.complete_path(parent);
            if let Ok(mut dir) = res {
                if dir.is_dir() {
                    let filename = get_filename(path);
                    if let Some(filename) = filename {
                        dir.push(filename);
                        if create_dir(dir).is_ok() {
                            let answer = Answer::new(ResultCode::PATHNAMECreated, "Folder successfully created!");
                            connection.write(answer.to_bytes())?;
                            return Ok(());
                        }
                    }
                }
            }
        }
        let answer = Answer::new(ResultCode::FileNotFound, "Couldn't create folder");
        connection.write(answer.to_bytes())?;
        Ok(())
    }

    fn is_logged(&self) -> bool {
        self.name.is_some() && self.waiting_password == false
    }

    fn list(&mut self, path: Option<PathBuf>) -> Result<()> {
        let connection = self.connection.as_ref().expect("Received without connection");
        if self.data_connection.is_some() {
            let path = self.cwd.join(path.unwrap_or_default());
            let directory = PathBuf::from(&path);
            let res = self.complete_path(directory);
            if let Ok(path) = res {
                let answer = Answer::new(ResultCode::DataConnectionAlreadyOpen, "Starting to list directory...");
                connection.write(answer.to_bytes())?;
                let mut out = vec![];
                if path.is_dir() {
                    if let Ok(dir) = read_dir(path) {
                        for entry in dir {
                            if let Ok(entry) = entry {
                                add_file_info(entry.path(), &mut out);
                            }
                        }
                    }
                    else {
                        let answer = Answer::new(ResultCode::InvalidParameterOrArgument, "No such file or directory");
                        connection.write(answer.to_bytes())?;
                        return Ok(());
                    }
                }
                else {
                    add_file_info(path, &mut out);
                }
                self.send_data(out)?;
                //println!("-> and done!");
            }
            else {
                let answer = Answer::new(ResultCode::InvalidParameterOrArgument, "No such file or directory");
                connection.write(answer.to_bytes())?;
            }
        }
        else {
            let answer = Answer::new(ResultCode::ConnectionClosed, "No opened data connection");
            connection.write(answer.to_bytes())?;
        }
        Ok(())
    }

    fn pasv(&mut self, stream: &Stream<Msg>) -> Result<()> {
        let connection = self.connection.as_ref().expect("Received without connection");
        let port =
            if let Some(port) = self.data_port {
                port
            }
            else {
                0
            };
        if self.data_connection.is_some() {
            let answer = Answer::new(ResultCode::DataConnectionAlreadyOpen, "Already listening...");
            connection.write(answer.to_bytes())?;
            return Ok(());
        }

        let (listener, addr) = TcpListener::ip4(&mut self.event_loop, &format!("127.0.0.1:{}", port), DataListener::new(stream))?;
        self.listener = Some(listener);
        let port = addr.port();
        //println!("Waiting clients on port {}...", port);

        let answer = Answer::new(ResultCode::EnteringPassiveMode, &format!("127,0,0,1,{},{}", port >> 8, port & 0xFF));
        connection.write(answer.to_bytes())?;
        connection.mute();
        Ok(())
    }

    fn quit(&self, connection: &TcpConnection) -> Result<()> {
        if self.data_connection.is_some() {
            unimplemented!();
        }
        else {
            let answer = Answer::new(ResultCode::ServiceClosingControlConnection, "Closing connection...");
            connection.write(answer.to_bytes())?;
            connection.dispose();
        }
        Ok(())
    }

    fn retr(&mut self, path: PathBuf) -> Result<()> {
        // TODO: check if multiple data connection can be opened at the same time.
        let connection = self.connection.as_ref().expect("Received without connection");
        if self.data_connection.is_some() {
            let path = self.cwd.join(path);
            let res = self.complete_path(path.clone()); // TODO: ugly clone
            if let Ok(path) = res {
                if path.is_file() {
                    let answer = Answer::new(ResultCode::DataConnectionAlreadyOpen, "Starting to send file...");
                    connection.write(answer.to_bytes())?;
                    let mut file = File::open(&path)?;
                    let mut out = vec![];
                    // TODO: send the file chunck by chunck if it is big (if needed).
                    file.read_to_end(&mut out)?;
                    self.send_data(out)?;
                    //println!("-> file transfer done!");
                } else {
                    let answer = Answer::new(ResultCode::LocalErrorInProcessing,
                                      &format!("\"{}\" doesn't exist", path.to_str()
                                               .ok_or_else(||
                                                   Error::Msg("No path".to_string()))?));
                    connection.write(answer.to_bytes())?;
                }
            } else {
                let answer = Answer::new(ResultCode::LocalErrorInProcessing,
                                      &format!("\"{}\" doesn't exist", path.to_str()
                                               .ok_or_else(||
                                                   Error::Msg("No path".to_string()))?));
                connection.write(answer.to_bytes())?;
            }
        } else {
            let answer = Answer::new(ResultCode::ConnectionClosed, "No opened data connection");
            connection.write(answer.to_bytes())?;
        }
        if self.data_connection.is_some() {
            // TODO: might need to be moved in the sent() method.
            self.close_data_connection();
            let connection = self.connection.as_ref().expect("Received without connection");
            let answer = Answer::new(ResultCode::ClosingDataConnection, "Transfer done");
            connection.write(answer.to_bytes())?;
        }
        Ok(())
    }

    fn stor(&mut self, path: PathBuf) -> Result<()> {
        let connection = self.connection.as_ref().expect("Received without connection");
        if self.data_connection.is_some() {
            if invalid_path(&path) {
                let error: io::Error = io::ErrorKind::PermissionDenied.into();
                return Err(error.into());
            }
            let path = self.cwd.join(path);
            let path =
                self.server_root.join(if path.has_root() {
                    path.iter().skip(1).collect()
                }
                else {
                    path
                });
            self.store_path = Some(path);
            let answer = Answer::new(ResultCode::DataConnectionAlreadyOpen, "Starting to send file...");
            connection.write(answer.to_bytes())?;
        } else {
            let answer = Answer::new(ResultCode::ConnectionClosed, "No opened data connection");
            connection.write(answer.to_bytes())?;
        }
        Ok(())
    }

    fn send_data(&self, data: Vec<u8>) -> Result<()> {
        if let Some(ref connection) = self.data_connection {
            //println!("Sent data: {}", String::from_utf8_lossy(&data));
            connection.write(data)?;
        }
        Ok(())
    }

    fn strip_prefix(&self, dir: PathBuf) -> result::Result<PathBuf, StripPrefixError> {
        dir.strip_prefix(&self.server_root).map(|p| p.to_path_buf())
    }
}

impl Handler for Client {
    type Msg = Msg;

    fn update(&mut self, stream: &Stream<Msg>, msg: Self::Msg) {
        match msg {
            Accepted(connection) => {
                /*let address = format!("[address : {}]", connection.local_addr()); // TODO
                println!("New client: {}", address);*/
                //println!("New client");
                let answer = Answer::new(ResultCode::ServiceReadyForNewUser, "Welcome to this FTP server!");
                connection.write(answer.to_bytes());
                self.connection = Some(connection);
            },
            DataConnected(stream) => {
                self.data_connection = Some(stream);
                let connection = self.connection.as_ref().expect("Received without connection");
                connection.unmute();
                if let Some(listener) = self.listener.take() {
                    listener.send(ListenerMsg::Dispose);
                }
            },
            DataReceived(data) => {
                if let Some(path) = self.store_path.take() {
                    let write_data = || -> io::Result<()> {
                        //println!("Writing to {:?}", path.to_str());
                        let mut file = File::create(path)?;
                        file.write_all(&data)?;
                        //println!("-> file transfer done!");
                        self.close_data_connection();
                        let answer = Answer::new(ResultCode::ClosingDataConnection, "Transfer done");
                        let connection = self.connection.as_ref().expect("Received without connection");
                        connection.write(answer.to_bytes())?;
                        Ok(())
                    };
                    if let Err(error) = write_data() {
                        eprintln!("Error: {}", error);
                    };
                }
            },
            Received(data) => {
                if let Some(index) = find_crlf(&data) {
                    let line =
                        if self.current_cmd.is_empty() {
                            data[..index].to_vec()
                        }
                        else {
                            let mut line = mem::replace(&mut self.current_cmd, vec![]);
                            line.extend(&data[..index]);
                            line
                        };
                    //data.split_to(2); // TODO: handle multiple commands in one message.
                    if let Ok(command) = Command::new(line) {
                        if let Err(error) = self.handle_cmd(command, stream) {
                            eprintln!("Error: {}", error);
                        }
                    }
                    // TODO: handle error.
                }
                else {
                    self.current_cmd.extend(data);
                }
            },
            TransferDone => {
                if self.data_connection.is_some() {
                    let answer = Answer::new(ResultCode::ClosingDataConnection, "Transfer done");
                    let connection = self.connection.as_ref().expect("Received without connection");
                    connection.write(answer.to_bytes());
                    self.close_data_connection();
                }
            },
        }
    }
}

struct ClientNotify {
    stream: Stream<Msg>,
}

impl ClientNotify {
    fn new(stream: Stream<Msg>) -> Self {
        Self {
            stream,
        }
    }
}

impl TcpConnectionNotify for ClientNotify {
    fn accepted(&mut self, connection: &mut TcpConnection) {
        self.stream.send(Accepted(connection.clone()));
    }

    fn received(&mut self, _connection: &mut TcpConnection, data: Vec<u8>) {
        self.stream.send(Received(data));
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|bytes| bytes == b"\r\n")
}

fn main() -> io::Result<()> {
    let mut event_loop = Loop::new()?;
    let server_root = env::current_dir()?;
    let listener = Listener::new(&event_loop, server_root);
    TcpListener::ip4(&mut event_loop, "127.0.0.1:1337", listener)?;
    event_loop.run()
}
