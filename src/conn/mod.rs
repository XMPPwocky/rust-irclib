//! Management of IRC server connection

use std::from_str::from_str;
use std::str::from_utf8;
use std::fmt;
use std::io;
use std::io::{IoError, IoResult, TcpStream};
use std::io::BufferedStream;
use std::{char,str,uint};
use std::str::MaybeOwned;
use std::cmp::min;
use std::comm;
use std::task::TaskBuilder;
use User;

mod handlers;

/// Conn represenets a connection to a single IRC server
///
/// The Payload type parameter is extra state that is accessible as a public
/// field from the Conn object. It's meant to allow your program to provide
/// extra data for your handler to use. It is completely ignored by this
/// library otherwise.
pub struct Conn<'a> {
    host: &'a str,
    write_tx: Option<Sender<Vec<u8>>>,
    logged_in: bool,
    user: User,
}

/// Options used with Conn for connecting to the server.
///
/// Payload is the type of the payload carried along by the connection.
/// It's needed here for the commands channel.
pub struct Options<'a, Payload=()> {
    /// The server host to connect to
    pub host: &'a str,
    /// The server port to connect to
    pub port: u16,
    /// The nickname to use
    pub nick: &'a str,
    /// The username to use
    pub user: &'a str,
    /// The real name to use
    pub real: &'a str,
    /// A Port to send procs to.
    /// The Port will be closed when connect() returns.
    /// Any proc sent to this port will be executed on the connection's task,
    /// with a handle to the connection.
    ///
    /// When the connection shuts down, all already-scheduled procs will read from the
    /// channel, the channel closed, and then the procs will execute. Any procs added
    /// to the channel after the channel is drained, but before it's closed, will be
    /// discarded.
    pub commands: Option<Receiver<Cmd<Payload>>>,
}

impl<'a, Payload> Options<'a, Payload> {
    /// Returns a new Options struct with default values
    pub fn new(host: &'a str, port: u16) -> Options<'a, Payload> {
        #![inline]
        Options {
            host: host,
            port: port,
            nick: "ircnick",
            user: "ircuser",
            real: "rust-irclib user",
            commands: None
        }
    }
}

/// Typedef for commands that can be sent to the commands Port
pub type Cmd<Payload=()> = proc(&mut Conn, &mut Payload) : Send;

/// Events that can be handled in the callback
pub enum Event {
    /// The connection was established
    Connected,
    /// A line was received from the server.
    /// This event is not sent until the user has successfully logged in.
    /// The first received line should be 001
    LineReceived(Line),
    /// The connection has terminated
    Disconnected
}

/// Errors that can be returned from connect()
pub enum Error {
    /// Error connecting to server
    ErrConnect(IoError),
    /// I/O error raised while connection is active
    ErrIO(IoError)
}

impl fmt::Show for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ErrConnect(ref err) => { write!(f, "connect error: {}", *err) }
            ErrIO(ref err) => err.fmt(f)
        }
    }
}

/// Typedef for connection results
pub type Result = ::std::result::Result<(),Error>;

pub static DefaultPort: u16 = 6667;

/// Connects to the remote server. This method will not return until the connection
/// is terminated. Returns Ok(()) after connection termination if the connection was
/// established successfully, or Err(_) if the connection could not be established in the
/// first place, or if an error is thrown while the connection is active.
///
/// This method spawns some I/O-blocked tasks, so it is recommended that it be called
/// from a libgreen task.
///
/// Note: If your Conn has no payload, you should pass () as the payload parameter.
pub fn connect<Payload>(opts: Options<Payload>, mut payload: Payload,
                        cb: |&mut Conn, Event, &mut Payload|) -> Result {
    let stream = match TcpStream::connect((opts.host, opts.port)) {
        Err(e) => return Err(ErrConnect(e)),
        Ok(stream) => stream
    };

    let mut conn = Conn{
        host: opts.host,
        write_tx: None,
        logged_in: false,
        user: User::new(opts.nick.as_bytes(), Some(opts.user.as_bytes()), None),
    };

    cb(&mut conn, Connected, &mut payload);

    let res = conn.run(stream, opts, &mut payload, |c,e,p| cb(c,e,p));

    cb(&mut conn, Disconnected, &mut payload);

    match res {
        Err(e) => Err(ErrIO(e)),
        Ok(()) => Ok(())
    }
}

impl<'a> Conn<'a> {
    fn run<Payload>(&mut self, stream: TcpStream, opts: Options<Payload>, payload: &mut Payload,
                    cb: |&mut Conn, Event, &mut Payload|) -> IoResult<()> {
        // spawn I/O tasks
        let (write_tx, write_rx) = channel();
        self.write_tx = Some(write_tx);
        let (read_tx, read_rx) = channel();
        let (err_tx, err_rx) = channel();

        {
            let stream = stream.clone();
            let err_tx = err_tx.clone();
            TaskBuilder::new().named("libirc writer").spawn(proc() {
                let mut stream = stream;
                loop {
                    let line = match write_rx.recv_opt() {
                        Err(_) => break,
                        Ok(v) => v
                    };
                    match stream.write(line.as_slice()).and_then(|_| stream.flush()) {
                        Ok(_) => (),
                        Err(e) => {
                            if e.kind != io::EndOfFile {
                                err_tx.send(Err(e));
                            }
                            break;
                        }
                    }
                }
            });
        }
        {
            TaskBuilder::new().named("libirc reader").spawn(proc() {
                let mut stream = BufferedStream::new(stream);
                loop {
                    let mut line = match stream.read_until('\n' as u8) {
                        Ok(v) => v,
                        Err(e) => {
                            if e.kind != io::EndOfFile {
                                err_tx.send(Err(e));
                            }
                            break;
                        }
                    };
                    if !chomp_owned(&mut line) {
                        // no line terminator? Must have hit EOF
                        break;
                    }
                    if line.len() > 0 {
                        if read_tx.send_opt(line).is_err() {
                            break;
                        }
                    }
                }
            })
        }

        // send handshake commands
        self.send_command(IRCCmd("NICK".into_maybe_owned()), [opts.nick.as_bytes()], false);
        self.send_command(IRCCmd("USER".into_maybe_owned()), [opts.user.as_bytes(), b"8 *",
                          opts.real.as_bytes()], true);


        // run event loop
        // need to do some shenanigans with scoping to make borrowck happy
        let mut result = Ok(());
        let procs = {
            let select = comm::Select::new();
            let mut read_handle = select.handle(&read_rx);
            unsafe { read_handle.add() }
            let mut err_handle = select.handle(&err_rx);
            unsafe { err_handle.add() }
            let commands = opts.commands;
            let mut cmd_handle = commands.as_ref().map(|p| select.handle(p));
            if cmd_handle.is_some() {
                unsafe { cmd_handle.as_mut().unwrap().add(); }
            }
            loop {
                // wait on the Select, but ignore the id
                // On each pass we simply check all ports. Keeps things a bit more fair.
                select.wait();
                match err_rx.try_recv() {
                    Err(comm::Empty) => (),
                    Err(comm::Disconnected) => break,
                    Ok(err) => {
                        result = err;
                        break;
                    }
                }
                if commands.is_some() {
                    match commands.as_ref().unwrap().try_recv() {
                        Err(comm::Empty) => (),
                        Err(comm::Disconnected) => {
                            unsafe { cmd_handle.as_mut().unwrap().remove(); }
                            cmd_handle = None;
                        }
                        Ok(cmd) => {
                            cmd(self, payload);
                        }
                    }
                }
                let line = match read_rx.try_recv() {
                    Err(comm::Empty) => continue,
                    Err(comm::Disconnected) => break,
                    Ok(line) => line
                };
                let line = match Line::parse(line.as_slice()) {
                    None => {
                        let line = line.as_slice();
                        info!("[DEBUG] Found non-parseable line: {}", String::from_utf8_lossy(line));
                        continue;
                    }
                    Some(line) => line
                };
                if log_enabled!(::log::DEBUG) {
                    let line = line.to_raw();
                    debug!("[DEBUG] Received line: {}", String::from_utf8_lossy(line.as_slice()));
                }
                handlers::handle_line(self, &line);
                if self.logged_in {
                    cb(self, LineReceived(line), payload);
                }
            }
            if result.is_ok() {
                // check the err_handle one more time
                match err_rx.try_recv() {
                    Ok(err) => {
                        result = err;
                    }
                    _ => ()
                }
            }

            // drain the commands
            match commands {
                None => None,
                Some(ref port) => {
                    let mut procs = Vec::new();
                    loop {
                        match port.try_recv() {
                            Err(_) => break,
                            Ok(cmd) => procs.push(cmd)
                        }
                    }
                    Some(procs)
                }
            }
        };
        // at this point the commands port is out of scope and therefore closed
        // ensure our write handle is closed out, in case we stopped due to read shutting down,
        // and then run any buffered procs
        self.write_tx = None;
        match procs {
            None => (),
            Some(procs) => {
                for cmd in procs.into_iter() {
                    cmd(self, payload);
                }
            }
        }

        // return the result
        result
    }

    /// Returns `true` if the connection is still active
    /// (or was at the last pass through the runloop).
    pub fn is_connected(&self) -> bool {
        self.write_tx.is_some()
    }

    /// Returns the host that was used to create this Conn
    pub fn host(&self) -> &'a str {
        self.host
    }

    /// Returns the current User.
    pub fn me<'a>(&'a self) -> &'a User {
        &self.user
    }

    /// Sends a command to the server.
    /// The line is truncated to 510 bytes (not including newline) before sending.
    ///
    /// If the command is an IRCCmd or IRCCode, the args vector is interpreted as a
    /// space-separated list of arguments, with a ':' argument prefix denoting the final
    /// (possibly space-containing) argument.
    ///
    /// If the command is an IRCAction, IRCCTCP, or IRCCTCPReply, the args vector is interpreted
    /// as the message that is being sent. It should be not be prefixed with a ':'.
    ///
    /// No attempt is made to ensure that the args vector is valid. All values in the vector are
    /// separated with a single space, and no special handling of ':' is performed. It is assumed
    /// that the caller will provide valid arguments and will ':'-prefix as necessary.
    ///
    /// The add_colon flag causes the final argument in the args list to have a ':' prepended.
    pub fn send_command(&mut self, cmd: Command, args: &[&[u8]], add_colon: bool) {
        if !{
            let chan = match self.write_tx {
                None => return,
                Some(ref mut c) => c
            };
            let mut line = [0u8, ..512];
            let len = {
                let mut buf = line.slice_to_mut(510);

                fn append(buf: &mut &mut [u8], v: &[u8]) {
                    let len = buf.clone_from_slice(v);
                    // this should work:
                    //   *buf = buf.slice_from_mut(len);
                    // but I'm getting weird borrowck issues (see mozilla/rust#11361)
                    *buf = unsafe { ::std::mem::transmute(buf.slice_from_mut(len)) };
                }

                let is_ctcp = cmd.is_ctcp();
                match cmd {
                    IRCCmd(cmd) => {
                        append(&mut buf, cmd.as_slice().as_bytes());
                    }
                    IRCCode(code) => {
                        uint::to_str_bytes(code, 10, |v| {
                            append(&mut buf, v);
                        });
                    }
                    IRCAction(ref dst) | IRCCTCP(ref dst,_) => {
                        append(&mut buf, b"PRIVMSG ");
                        append(&mut buf, dst.as_slice());
                        append(&mut buf, b" :\x01");
                        let action = match cmd {
                            IRCAction(_) => { static b: &'static [u8] = b"ACTION"; b }
                            IRCCTCP(_,ref action) => action.as_slice(),
                            _ => unreachable!()
                        };
                        append(&mut buf, action);
                    }
                    IRCCTCPReply(dst, action) => {
                        append(&mut buf, b"NOTICE ");
                        append(&mut buf, dst.as_slice());
                        append(&mut buf, b" :\x01");
                        append(&mut buf, action.as_slice());
                    }
                }
                if !args.is_empty() {
                    for arg in args.init().iter() {
                        append(&mut buf, b" ");
                        append(&mut buf, arg.as_slice());
                    }
                    if add_colon {
                        append(&mut buf, b" :");
                    } else {
                        append(&mut buf, b" ");
                    }
                    append(&mut buf, args.last().unwrap().as_slice());
                }
                if is_ctcp {
                    append(&mut buf, b"\x01");
                }
                510 - buf.len()
            };
            debug!("[DEBUG] Sent line: {}", String::from_utf8_lossy(line.slice_to(len)));
            line.slice_from_mut(len).clone_from_slice(b"\r\n");
            chan.send_opt(line.slice_to(len+2).to_vec()).is_ok()
        } {
            self.write_tx = None;
        }
    }

    /// Sends a raw command to the server
    ///
    /// The line is sent exactly as provided, except truncated to 510 characters
    /// and terminated with \r\n.
    pub fn send_raw(&mut self, raw: &[u8]) {
        let raw = chomp(raw);
        if raw.is_empty() { return }
        if !{
            let chan = match self.write_tx {
                None => return,
                Some(ref mut c) => c
            };
            let mut line = [0u8, ..512];
            let len = line.slice_to_mut(510).clone_from_slice(raw);
            debug!("[DEBUG] Sent line: {}", String::from_utf8_lossy(line.slice_to(len)));
            line.slice_from_mut(len).clone_from_slice(b"\r\n");
            chan.send_opt(line.slice_to(len+2).to_vec()).is_ok()
        } {
            self.write_tx = None;
        }
    }

    /// Sets the user's nickname.
    pub fn set_nick(&mut self, nick: &[u8]) {
        self.send_command(IRCCmd("NICK".into_maybe_owned()), [nick], false);
        // if we're logged in, watch for the NICK reply before changing our nick
        if !self.logged_in {
            self.user = self.user.with_nick(nick);
        }
    }

    /// Quits the connection
    /// Pass [] for the message to use the default.
    pub fn quit(&mut self, msg: &[u8]) {
        if msg.is_empty() {
            let args: &[&[u8]] = [];
            self.send_command(IRCCmd("QUIT".into_maybe_owned()), args, false);
        } else {
            self.send_command(IRCCmd("QUIT".into_maybe_owned()), [msg], true);
        }
    }

    /// Sends a PRIVMSG
    pub fn privmsg(&mut self, dst: &[u8], msg: &[u8]) {
        // NB: .as_slice() calls are necessary to work around mozilla/rust#8874
        self.send_command(IRCCmd("PRIVMSG".into_maybe_owned()),
                          [dst.as_slice(), msg.as_slice()], true)
    }

    /// Sends a NOTICE
    pub fn notice(&mut self, dst: &[u8], msg: &[u8]) {
        self.send_command(IRCCmd("NOTICE".into_maybe_owned()),
                          [dst.as_slice(), msg.as_slice()], true)
    }

    /// Sends a JOIN
    /// Pass [] for keys if there are none.
    pub fn join(&mut self, room: &[u8], keys: &[u8]) {
        if keys.is_empty() {
            self.send_command(IRCCmd("JOIN".into_maybe_owned()), [room], false);
        } else {
            self.send_command(IRCCmd("JOIN".into_maybe_owned()),
                              [room.as_slice(), keys.as_slice()], false);
        }
    }

    /// Sends a PART
    /// Pass [] for the message to use the default.
    pub fn part(&mut self, room: &[u8], msg: &[u8]) {
        if msg.is_empty() {
            self.send_command(IRCCmd("PART".into_maybe_owned()), [room], false);
        } else {
            self.send_command(IRCCmd("PART".into_maybe_owned()),
                              [room.as_slice(), msg.as_slice()], true);
        }
    }
}

fn chomp_owned(s: &mut Vec<u8>) -> bool {
    let len = chomp(s.as_slice()).len();
    if len < s.len() {
        s.truncate(len);
        true
    } else { false }
}

fn chomp<'a>(s: &'a [u8]) -> &'a [u8] {
    if s.len() > 0 {
        match s[s.len()-1] as char {
            '\r' => s.slice_to(s.len()-1),
            '\n' => {
                if s.len() > 1 && s[s.len()-2] == '\r' as u8 {
                    s.slice_to(s.len()-2)
                } else {
                    s.slice_to(s.len()-1)
                }
            }
            _ => s
        }
    } else { s }
}

/// An IRC command
#[deriving(PartialEq,Eq,Clone)]
pub enum Command {
    /// An IRC command
    IRCCmd(MaybeOwned<'static>),
    /// A 3-digit command code
    IRCCode(uint),
    /// CTCP actions. The first arg is the destination
    IRCAction(Vec<u8>),
    /// CTCP commands. The first arg is the command, the second is the destination
    IRCCTCP(Vec<u8>, Vec<u8>),
    /// CTCP replies. The first arg is the command, the second is the destination
    IRCCTCPReply(Vec<u8>, Vec<u8>)
}

impl Command {
    /// Returns true if the command is a CTCP command
    pub fn is_ctcp(&self) -> bool {
        match *self {
            IRCAction(_) | IRCCTCP(_,_) | IRCCTCPReply(_,_) => true,
            _ => false }
    }
}

impl fmt::Show for Command {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            IRCCmd(ref s) => write!(f, "IRCCmd({})", *s),
            IRCCode(code) => write!(f, "IRCCode({})", code),
            IRCAction(ref v) => write!(f, "IRCAction({})", String::from_utf8_lossy(v.as_slice())),
            IRCCTCP(ref cmd, ref dst) => {
                let cmd = String::from_utf8_lossy(cmd.as_slice());
                let dst = String::from_utf8_lossy(dst.as_slice());
                write!(f, "IRCCTCP({}, {})", cmd, dst)
            }
            IRCCTCPReply(ref cmd, ref dst) => {
                let cmd = String::from_utf8_lossy(cmd.as_slice());
                let dst = String::from_utf8_lossy(dst.as_slice());
                write!(f, "IRCCTCPReply({}, {})", cmd, dst)
            }
        }
    }
}

/// A parsed line
#[deriving(PartialEq, Eq,Clone)]
pub struct Line {
    /// The optional prefix
    pub prefix: Option<User>,
    /// The command
    pub command: Command,
    /// Any arguments
    pub args: Vec<Vec<u8>>,
}

impl fmt::Show for Line {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        try!(write!(f, r"Line{{ prefix: {}, command: {}, args: [", self.prefix, self.command));
        for (i, v) in self.args.iter().enumerate() {
            if i != 0 {
                try!(write!(f, ", "));
            }
            try!(write!(f, "{}", String::from_utf8_lossy(v.as_slice())));
        }
        write!(f, "]")
    }
}

impl Line {
    /// Parse a line into a Line struct
    pub fn parse(mut v: &[u8]) -> Option<Line> {
        let mut prefix = None;
        if v.starts_with(b":") {
            let idx = match v.position_elem(&(' ' as u8)) {
                None => return None,
                Some(idx) => idx
            };
            prefix = Some(User::parse(v.slice(1, idx)));
            v = v.slice_from(idx+1);
        }
        let (mut command, checkCTCP) = {
            let cmd;
            match v.position_elem(&(' ' as u8)) {
                Some(0) => return None,
                None => {
                    cmd = v;
                    v = [].as_slice();
                }
                Some(idx) => {
                    cmd = v.slice_to(idx);
                    v = v.slice_from(idx+1);
                }
            }
            if cmd.len() == 3 && cmd.iter().all(|&b| b >= '0' as u8 && b <= '9' as u8) {
                (IRCCode(from_utf8(cmd).and_then(|cmd| from_str(cmd)).unwrap_or(0u)), false)
            } else if cmd.iter().all(|&b| b < 0x80 && char::is_alphabetic(b as char)) {
                let shouldCheck = cmd == b"PRIVMSG" || cmd == b"NOTICE";
                (IRCCmd(str::from_utf8(cmd).unwrap().to_string().into_maybe_owned()), shouldCheck)
            } else {
                return None;
            }
        };
        let mut args = Vec::new();
        while !v.is_empty() {
            if v[0] == ':' as u8 {
                args.push((v.slice_from(1)).to_vec());
                break;
            }
            let idx = match v.position_elem(&(' ' as u8)) {
                None => {
                    args.push(v.to_vec());
                    break;
                }
                Some(idx) => idx
            };
            args.push(v.slice_to(idx).to_vec());
            v = v.slice_from(idx+1);
        }
        if checkCTCP && args.last().map_or(false, |v| v.as_slice().starts_with([0x1])) {
            let mut text = args.pop().unwrap();
            if text.len() > 1 && text.as_slice().ends_with([0x1]) {
                text = text.slice(1,text.len()-1).to_vec();
            } else {
              text.remove(0);
            }
            let dst = args.into_iter().next().unwrap();
            let ctcpcmd;
            match text.as_slice().position_elem(&(' ' as u8)) {
                Some(idx) => {
                    ctcpcmd = (text.slice_to(idx)).to_vec();
                    args = vec![text.slice_from(idx+1).to_vec()];
                }
                None => {
                    ctcpcmd = text.clone();
                    args = Vec::new();
                }
            }
            let cmdstr = match command {
                IRCCmd(ref s) if "PRIVMSG" == s.as_slice() => "PRIVMSG",
                IRCCmd(ref s) if "NOTICE" == s.as_slice() => "NOTICE",
                _ => unreachable!()
            };
            match cmdstr {
                "PRIVMSG" => {
                    if b"ACTION" == ctcpcmd.as_slice() {
                        command = IRCAction(dst);
                        if args.is_empty() {
                            args.push(Vec::new());
                        }
                    } else {
                        command = IRCCTCP(ctcpcmd, dst);
                    }
                }
                "NOTICE" => {
                    command = IRCCTCPReply(ctcpcmd, dst);
                }
                _ => unreachable!()
            }
        }
        Some(Line{
            prefix: prefix,
            command: command,
            args: args
        })
    }

    /// Converts into the "raw" representation :prefix cmd args
    pub fn to_raw(&self) -> Vec<u8> {
        let mut cap = self.prefix.as_ref().map_or(0, |s| 1+s.raw().len()+1);
        let mut found_space = false;
        cap += match self.command {
            IRCCmd(ref cmd) => cmd.len(),
            IRCCode(_) => 3,
            IRCAction(ref dst) => {
                "PRIVMSG".len() + 1 + dst.len() + 1 + ":\x01ACTION".len()
            }
            IRCCTCP(ref cmd, ref dst) => {
                "PRIVMSG".len() + 1 + dst.len() + 1 + 2 + cmd.len()
            }
            IRCCTCPReply(ref cmd, ref dst) => {
                "NOTICE".len() + 1 + dst.len() + 1 + 2 + cmd.len()
            }
        };
        if self.command.is_ctcp() {
            for arg in self.args.iter() {
                cap += 1 + arg.len();
            }
            cap += 1; // for the final \x01
        } else if !self.args.is_empty() {
            if self.args.len() > 1 {
                for arg in self.args.init().iter() {
                    cap += 1 + arg.len();
                }
            }
            let last = self.args.last().unwrap();
            found_space = last.contains(&(' ' as u8));
            if found_space {
                cap += 1 + 1 /* : */ + last.len();
            } else {
                cap += 1 + last.len();
            }
        }
        let mut res = Vec::with_capacity(cap);
        if self.prefix.is_some() {
            res.push(':' as u8);
            res.push_all(self.prefix.as_ref().unwrap().raw());
            res.push(' ' as u8);
        }
        match self.command {
            IRCCmd(ref cmd) => res.push_all(cmd.as_slice().as_bytes()),
            IRCCode(c) => {
                uint::to_str_bytes(c, 10, |v| {
                    for _ in range(0, 3 - min(v.len(), 3)) {
                        res.push('0' as u8);
                    }
                    res.push_all(v);
                })
            }
            IRCAction(ref dst) => {
                res.push_all(b"PRIVMSG ");
                res.push_all(dst.as_slice());
                res.push_all(b" :\x01ACTION");
            }
            IRCCTCP(ref cmd, ref dst) => {
                res.push_all(b"PRIVMSG ");
                res.push_all(dst.as_slice());
                res.push_all(b" :\x01");
                res.push_all(cmd.as_slice());
            }
            IRCCTCPReply(ref cmd, ref dst) => {
                res.push_all(b"NOTICE ");
                res.push_all(dst.as_slice());
                res.push_all(b" :\x01");
                res.push_all(cmd.as_slice());
            }
        }
        if self.command.is_ctcp() {
            for arg in self.args.iter() {
                res.push(' ' as u8);
                res.push_all(arg.as_slice());
            }
            res.push(0x1);
        } else if !self.args.is_empty() {
            if self.args.len() > 1 {
                for arg in self.args.init().iter() {
                    res.push(' ' as u8);
                    res.push_all(arg.as_slice());
                }
            }
            res.push(' ' as u8);
            if found_space {
                res.push(':' as u8);
            }
            res.push_all(self.args.last().unwrap().as_slice());
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::{Line,IRCCmd,IRCCode,IRCAction,IRCCTCP,IRCCTCPReply};
    use User;

    #[test]
    fn parse_line() {
        macro_rules! t(
            ($v:expr, Some($exp:expr)) => (
                t!($v, Some($exp), $v);
            );
            ($v:expr, Some($exp:expr), $res:expr) => ({
                let v = $v;
                let exp = $exp;
                let line = Line::parse(v);
                assert!(line.is_some());
                let line = line.unwrap();
                assert_eq!(line.prefix, exp.prefix);
                assert_eq!(line.command, exp.command);
                assert_eq!(line.args, exp.args);
                let line = line.to_raw();
                assert_eq!(line.as_slice(), $res);
            });
            ($s:expr, None) => (
                assert_eq!(Line::parse($s), None);
            )
        )
        t!(b":sendak.freenode.net 001 asldfkj :Welcome to the freenode Internet Relay Chat Network asldfkj",
            Some(Line{
                prefix: Some(User::parse(b"sendak.freenode.net")),
                command: IRCCode(1),
                args: vec![b"asldfkj",
                           b"Welcome to the freenode Internet Relay Chat Network asldfkj"]
            }));
        t!(b"004 asdf :This is a test",
            Some(Line{
                prefix: None,
                command: IRCCode(4),
                args: vec![b"asdf", b"This is a test"]
            }));
        t!(b":nick!user@host.com PRIVMSG #channel :Some message",
            Some(Line{
                prefix: Some(User::parse(b"nick!user@host.com")),
                command: IRCCmd("PRIVMSG".into_maybe_owned()),
                args: vec![b"#channel", b"Some message"]
            }));
        t!(b" :sendak.freenode.net 001 asdf :Test", None);
        t!(b":sendak  001 asdf :Test", None);
        t!(b"004",
            Some(Line{
                prefix: None,
                command: IRCCode(4),
                args: vec![]
            }));
        t!(b":bob!user@host.com PRIVMSG #channel :\x01ACTION does some stuff",
            Some(Line{
                prefix: Some(User::parse(b"bob!user@host.com")),
                command: IRCAction(b"#channel"),
                args: vec![b"does some stuff"]
            }),
            b":bob!user@host.com PRIVMSG #channel :\x01ACTION does some stuff\x01");
        t!(b":bob!user@host.com PRIVMSG #channel :\x01VERSION\x01",
            Some(Line{
                prefix: Some(User::parse(b"bob!user@host.com")),
                command: IRCCTCP(b"VERSION", b"#channel"),
                args: vec![]
            }));
        t!(b":bob NOTICE #frobnitz :\x01RESPONSE to whatever\x01",
            Some(Line{
                prefix: Some(User::parse(b"bob")),
                command: IRCCTCPReply(b"RESPONSE", b"#frobnitz"),
                args: vec![b"to whatever"]
            }));
        t!(b":bob f\xC3\x83\xC2\xB6o", None);
        t!(b":bob f23", None);
    }
}
