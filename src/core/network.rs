//! Network locations: the `sftp://`, `smb://`, `ftp://`, `ftps://`, `dav://`
//! and `davs://` addresses Fileflux connects to, and the `gio mount` runner
//! that connects to them.
//!
//! The address model ([`ServerUri`]) is pure: it parses and validates what a
//! person types or a GTK bookmark holds. An address that carries a password
//! (`sftp://user:secret@host`) is refused, never stored or shown; the password
//! is asked for in a prompt instead.
//!
//! The runner ([`spawn_mount`]) starts `gio mount <uri>` with its standard
//! streams piped and talks to it on a worker thread: gio's prompts (user,
//! domain, password, host-key questions) are parsed by [`PromptScanner`] and
//! reported as [`MountEvent`]s; the answers come back through
//! [`MountControl`] and are written to gio's standard input. Cancelling kills
//! gio.
//!
//! [`spawn_unlock`] runs the same machinery for `gio mount -d <device>`, which
//! unlocks an encrypted volume: gio's passphrase prompt is a Password prompt.
//!
//! What this module does with a password: it travels in a [`Secret`] (never
//! printed by `Debug`, overwritten with zeros when dropped), is written once
//! to gio's standard input and is never put in argv, the environment, a log
//! or a message. The failure message gio prints is redacted of every password
//! that was answered before it is reported. The worker drops its copy when
//! the attempt ends.

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use super::devices::{UnlockTarget, unlock_args};

// --- the address --------------------------------------------------------------

/// A network scheme gvfs can mount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Sftp,
    Smb,
    Ftp,
    Ftps,
    Dav,
    Davs,
}

impl Scheme {
    /// The scheme as written before `://`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sftp => "sftp",
            Self::Smb => "smb",
            Self::Ftp => "ftp",
            Self::Ftps => "ftps",
            Self::Dav => "dav",
            Self::Davs => "davs",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [
            Self::Sftp,
            Self::Smb,
            Self::Ftp,
            Self::Ftps,
            Self::Dav,
            Self::Davs,
        ]
        .into_iter()
        .find(|scheme| scheme.as_str().eq_ignore_ascii_case(text))
    }

    /// The port the scheme uses when the address names none.
    fn default_port(self) -> Option<u16> {
        match self {
            Self::Sftp => Some(22),
            Self::Ftp | Self::Ftps => Some(21),
            Self::Dav => Some(80),
            Self::Davs => Some(443),
            Self::Smb => None,
        }
    }
}

/// Why an address was refused. The messages are what the dialog shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UriError {
    Empty,
    MissingScheme,
    UnsupportedScheme(String),
    MissingHost,
    InvalidHost,
    InvalidPort,
    InvalidCharacter,
    /// The user-info held `user:password`.
    PasswordInUri,
}

impl std::fmt::Display for UriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("Type a server address, for example sftp://host/path"),
            Self::MissingScheme => f.write_str(
                "The address needs a scheme: sftp://, smb://, ftp://, ftps://, dav:// or davs://",
            ),
            Self::UnsupportedScheme(scheme) => write!(
                f,
                "\u{201c}{scheme}://\u{201d} is not supported: use sftp, smb, ftp, ftps, dav or davs"
            ),
            Self::MissingHost => f.write_str("The address needs a host name"),
            Self::InvalidHost => f.write_str("The host name is not valid"),
            Self::InvalidPort => f.write_str("The port must be a number from 1 to 65535"),
            Self::InvalidCharacter => f.write_str("The address cannot contain control characters"),
            Self::PasswordInUri => f.write_str(
                "Do not put the password in the address: leave it out and enter it in the \
                 password prompt that appears when connecting",
            ),
        }
    }
}

impl std::error::Error for UriError {}

/// A validated server address: scheme, optional user, host, optional port and
/// path. It never holds a password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerUri {
    pub scheme: Scheme,
    /// The user-info as written (`bob`, or `DOMAIN;bob` for smb).
    pub user: Option<String>,
    /// Lower case; an IPv6 address without its brackets.
    pub host: String,
    pub port: Option<u16>,
    /// Empty, or starting with `/`; percent-escapes are kept as written.
    pub path: String,
}

fn is_host_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')
}

fn parse_port(text: &str) -> Result<u16, UriError> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(UriError::InvalidPort);
    }
    text.parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(UriError::InvalidPort)
}

/// Host and port of the part after `user@`.
fn parse_host_port(text: &str) -> Result<(String, Option<u16>), UriError> {
    if let Some(inner) = text.strip_prefix('[') {
        let (address, after) = inner.split_once(']').ok_or(UriError::InvalidHost)?;
        if address.is_empty() {
            return Err(UriError::MissingHost);
        }
        if !address.contains(':')
            || !address
                .chars()
                .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.'))
        {
            return Err(UriError::InvalidHost);
        }
        let port = match after.strip_prefix(':') {
            Some(port) => Some(parse_port(port)?),
            None if after.is_empty() => None,
            None => return Err(UriError::InvalidHost),
        };
        return Ok((address.to_ascii_lowercase(), port));
    }
    let (host, port) = match text.rsplit_once(':') {
        Some((host, port)) => (host, Some(parse_port(port)?)),
        None => (text, None),
    };
    if host.is_empty() {
        return Err(UriError::MissingHost);
    }
    if !host.chars().all(is_host_char) {
        return Err(UriError::InvalidHost);
    }
    Ok((host.to_ascii_lowercase(), port))
}

impl ServerUri {
    /// Parses and validates `text`: a supported scheme, a host, an optional
    /// `user@`, `:port` and path. Surrounding whitespace is ignored and a
    /// space in the path is escaped.
    ///
    /// # Errors
    ///
    /// Any other scheme, a missing or malformed host, a bad port, control
    /// characters, or a password in the user-info.
    pub fn parse(text: &str) -> Result<Self, UriError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(UriError::Empty);
        }
        if text.chars().any(char::is_control) {
            return Err(UriError::InvalidCharacter);
        }
        let (scheme_text, rest) = text.split_once("://").ok_or(UriError::MissingScheme)?;
        let scheme = Scheme::parse(scheme_text)
            .ok_or_else(|| UriError::UnsupportedScheme(scheme_text.to_owned()))?;
        let (authority, path) = match rest.split_once('/') {
            Some((authority, tail)) => (authority, format!("/{tail}")),
            None => (rest, String::new()),
        };
        let (user, host_port) = match authority.rsplit_once('@') {
            Some((info, host_port)) => (Some(info), host_port),
            None => (None, authority),
        };
        if user.is_some_and(|info| info.contains(':')) {
            return Err(UriError::PasswordInUri);
        }
        let (host, port) = parse_host_port(host_port)?;
        Ok(Self {
            scheme,
            user: user.filter(|info| !info.is_empty()).map(str::to_owned),
            host,
            port,
            path: path.replace(' ', "%20"),
        })
    }

    fn host_display(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    /// What a bookmark or a list shows: `title` when it has one, else the
    /// host (with its port) and the decoded path.
    #[must_use]
    pub fn label(&self, title: Option<&str>) -> String {
        if let Some(title) = title.map(str::trim).filter(|title| !title.is_empty()) {
            return title.to_owned();
        }
        let mut label = self.host_display();
        if let Some(port) = self.port {
            label.push_str(&format!(":{port}"));
        }
        label.push_str(&decode(self.path.trim_end_matches('/')));
        label
    }

    /// The path segments below the mount: a share is the mount itself, so
    /// smb skips its first segment, and webdav has none (its prefix is part
    /// of the mount).
    fn inner_segments(&self) -> Vec<String> {
        let skip = match self.scheme {
            Scheme::Smb => 1,
            Scheme::Dav | Scheme::Davs => return Vec::new(),
            _ => 0,
        };
        self.path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .skip(skip)
            .map(decode)
            .filter(|segment| {
                !matches!(segment.as_str(), "" | "." | "..") && !segment.contains(['/', '\0'])
            })
            .collect()
    }
}

impl std::fmt::Display for ServerUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://", self.scheme.as_str())?;
        if let Some(user) = &self.user {
            write!(f, "{user}@")?;
        }
        write!(f, "{}", self.host_display())?;
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        f.write_str(&self.path)
    }
}

fn hex(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|digit| u8::try_from(digit).ok())
}

/// `%XX` escapes decoded; a malformed escape stays as written.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let escaped = (byte == b'%')
            .then(|| {
                let high = hex(*bytes.get(index + 1)?)?;
                let low = hex(*bytes.get(index + 2)?)?;
                Some(high << 4 | low)
            })
            .flatten();
        if let Some(decoded) = escaped {
            out.push(decoded);
            index += 3;
        } else {
            out.push(byte);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// --- the gvfs mount of an address ---------------------------------------------

/// The fields of a gvfs mount directory name (`scheme:key=value,...`).
fn gvfs_fields(name: &str) -> Option<(&str, Vec<(&str, String)>)> {
    let (kind, fields) = name.split_once(':')?;
    let fields = fields
        .split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key, decode(value)))
        .collect();
    Some((kind, fields))
}

fn name_matches(uri: &ServerUri, name: &str) -> bool {
    let Some((kind, fields)) = gvfs_fields(name) else {
        return false;
    };
    let field = |key: &str| {
        fields
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, value)| value.as_str())
    };
    let host_is = |key: &str| {
        field(key).is_some_and(|host| {
            host.trim_matches(['[', ']'])
                .eq_ignore_ascii_case(&uri.host)
        })
    };
    let user_ok = uri
        .user
        .as_deref()
        .is_none_or(|user| field("user") == Some(user));
    let default = uri.scheme.default_port();
    let effective = |port: Option<u16>| port.filter(|port| Some(*port) != default);
    let port_ok =
        effective(uri.port) == effective(field("port").and_then(|port| port.parse::<u16>().ok()));
    match uri.scheme {
        Scheme::Sftp => kind == "sftp" && host_is("host") && user_ok && port_ok,
        Scheme::Ftp | Scheme::Ftps => kind == "ftp" && host_is("host") && user_ok && port_ok,
        Scheme::Dav | Scheme::Davs => {
            kind == "dav"
                && host_is("host")
                && user_ok
                && port_ok
                && (field("ssl") == Some("true")) == (uri.scheme == Scheme::Davs)
        }
        Scheme::Smb => {
            let share = uri
                .path
                .split('/')
                .find(|segment| !segment.is_empty())
                .map(decode);
            match share {
                Some(share) => {
                    kind == "smb-share"
                        && host_is("server")
                        && user_ok
                        && field("share").is_some_and(|found| found.eq_ignore_ascii_case(&share))
                }
                None => kind == "smb-server" && host_is("server") && user_ok,
            }
        }
    }
}

/// The name, among the gvfs mount directory `names`, of the mount `uri`
/// made.
#[must_use]
pub fn mounted_name(uri: &ServerUri, names: &[String]) -> Option<String> {
    names.iter().find(|name| name_matches(uri, name)).cloned()
}

/// The folder to open once `uri` is mounted as `name` under `root`: the
/// mount plus the path of the address inside it (never above the mount).
#[must_use]
pub fn mount_target(root: &Path, name: &str, uri: &ServerUri) -> PathBuf {
    let mut target = root.join(name);
    for segment in uri.inner_segments() {
        target.push(segment);
    }
    target
}

/// The names in the gvfs directory `root`; only names are read, nothing is
/// opened or mounted. A missing directory lists nothing.
fn gvfs_names(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// The folder of the mount `uri` made in the gvfs directory `root`. The FUSE
/// directory can trail gio's exit by a moment, so the directory is read up to
/// `attempts` times, `delay` apart. Blocking: run it off the UI thread.
#[must_use]
pub fn wait_for_mount(
    root: &Path,
    uri: &ServerUri,
    attempts: u32,
    delay: Duration,
) -> Option<PathBuf> {
    for attempt in 0..attempts {
        if let Some(name) = mounted_name(uri, &gvfs_names(root)) {
            return Some(mount_target(root, &name, uri));
        }
        if attempt + 1 < attempts {
            std::thread::sleep(delay);
        }
    }
    None
}

// --- gio's prompts ------------------------------------------------------------

/// What a text prompt of gio asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    User,
    Domain,
    Password,
    /// Any other question that ends in a prompt.
    Other,
}

/// One thing gio waits for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prompt {
    /// A line of text: `User [bob]: `, `Domain: `, `Password: `.
    Text {
        kind: PromptKind,
        /// The prompt without its default and colon (`User`).
        label: String,
        /// The default gio offers (`bob` in `User [bob]: `).
        default: Option<String>,
        /// The lines gio printed before the prompt.
        message: Vec<String>,
    },
    /// A question with numbered choices (an unknown SSH host key).
    Question {
        message: Vec<String>,
        /// Each choice with the number gio printed for it (gio counts from 1
        /// and expects that number back).
        choices: Vec<(u32, String)>,
    },
}

impl Prompt {
    /// The kind of a text prompt.
    #[must_use]
    pub fn kind(&self) -> Option<PromptKind> {
        match self {
            Self::Text { kind, .. } => Some(*kind),
            Self::Question { .. } => None,
        }
    }

    /// Whether the answer is a secret that must be masked.
    #[must_use]
    pub fn is_secret(&self) -> bool {
        self.kind() == Some(PromptKind::Password)
    }
}

/// Finds gio's prompts in the bytes it writes. gio ends a prompt without a
/// newline (`User [bob]: `), so a prompt is the unfinished last line when it
/// ends in `:`, `?` or `]` and a space.
#[derive(Debug, Default)]
pub struct PromptScanner {
    partial: Vec<u8>,
    lines: Vec<String>,
}

fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

/// `[3] Log in` as `(3, "Log in")`.
fn choice_line(line: &str) -> Option<(u32, &str)> {
    let rest = line.strip_prefix('[')?;
    let (number, text) = rest.split_once("] ")?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((number.parse().ok()?, text))
}

/// Splits `lines` into the message and the choice block that ends them: the
/// last run of lines numbered consecutively from 1. Numbered lines anywhere
/// else (a server's own text) stay in the message.
fn split_choices(lines: Vec<String>) -> (Vec<String>, Vec<(u32, String)>) {
    let numbers: Vec<Option<u32>> = lines
        .iter()
        .map(|line| choice_line(line).map(|(number, _)| number))
        .collect();
    let start = (0..lines.len()).rev().find(|&at| {
        numbers.get(at) == Some(&Some(1))
            && numbers
                .get(at..)
                .unwrap_or_default()
                .iter()
                .zip(1_u32..)
                .all(|(number, want)| *number == Some(want))
    });
    let Some(start) = start else {
        return (lines, Vec::new());
    };
    let mut message = lines;
    let block = message.split_off(start);
    let choices = block
        .iter()
        .filter_map(|line| choice_line(line).map(|(number, text)| (number, text.to_owned())))
        .collect();
    (message, choices)
}

fn kind_of(label: &str) -> PromptKind {
    let lower = label.to_ascii_lowercase();
    if lower == "user" || lower == "username" {
        PromptKind::User
    } else if lower == "domain" {
        PromptKind::Domain
    } else if lower.contains("password") || lower.contains("passphrase") {
        PromptKind::Password
    } else {
        PromptKind::Other
    }
}

fn looks_like_prompt(text: &str) -> bool {
    text.ends_with(' ') && matches!(text.trim_end().chars().last(), Some(':' | '?' | ']'))
}

/// The most of an unfinished line the scanner keeps.
const MAX_PARTIAL: usize = 64 * 1024;
/// The most message lines the scanner keeps for the next prompt.
const MAX_LINES: usize = 200;

impl PromptScanner {
    /// Takes the next bytes gio wrote; returns the prompts they completed.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Prompt> {
        self.partial.extend_from_slice(bytes);
        let mut found = Vec::new();
        let mut consumed = 0;
        while let Some(length) = self
            .partial
            .get(consumed..)
            .and_then(|rest| rest.iter().position(|byte| *byte == b'\n'))
        {
            let line = self
                .partial
                .get(consumed..consumed + length)
                .unwrap_or_default();
            let line = clean(&String::from_utf8_lossy(line));
            consumed += length + 1;
            if !line.is_empty() && !line.contains("will be echoed") {
                self.lines.push(line);
                if self.lines.len() > MAX_LINES {
                    self.lines.remove(0);
                }
            }
        }
        self.partial.drain(..consumed);
        // An unfinished line never grows past the cap: only its end matters.
        if self.partial.len() > MAX_PARTIAL {
            let excess = self.partial.len() - MAX_PARTIAL;
            self.partial.drain(..excess);
        }
        let text = String::from_utf8_lossy(&self.partial).into_owned();
        if looks_like_prompt(&text) {
            self.partial.clear();
            found.push(self.prompt(&clean(&text)));
        }
        found
    }

    fn prompt(&mut self, text: &str) -> Prompt {
        let (label, default) = match text.strip_suffix(':') {
            Some(label) => match label
                .strip_suffix(']')
                .and_then(|label| label.rsplit_once(" ["))
            {
                Some((label, default)) => (label.to_owned(), Some(default.to_owned())),
                None => (label.to_owned(), None),
            },
            None => (text.to_owned(), None),
        };
        let lines = std::mem::take(&mut self.lines);
        if label.eq_ignore_ascii_case("choice") {
            let (message, choices) = split_choices(lines.clone());
            if !choices.is_empty() {
                return Prompt::Question { message, choices };
            }
        }
        Prompt::Text {
            kind: kind_of(&label),
            label,
            default: default.filter(|default| !default.is_empty()),
            message: lines,
        }
    }
}

// --- secrets and answers ------------------------------------------------------

/// A typed answer, usually a password. `Debug` prints nothing of it and the
/// bytes are overwritten with zeros when it drops.
pub struct Secret(Vec<u8>);

impl Secret {
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self(text.as_bytes().to_vec())
    }

    /// Takes over the buffer of `text`, so no other copy is left behind.
    #[must_use]
    pub fn from_string(text: String) -> Self {
        Self(text.into_bytes())
    }

    fn has_line_break(&self) -> bool {
        self.0.iter().any(|byte| matches!(byte, b'\n' | b'\r' | 0))
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(\u{2022}\u{2022}\u{2022})")
    }
}

/// What the person answered to a [`Prompt`].
#[derive(Debug)]
pub enum Answer {
    /// The line for a text prompt.
    Line(Secret),
    /// The number of the chosen option of a question.
    Choice(u32),
}

// --- the runner ---------------------------------------------------------------

/// How an attempt ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountOutcome {
    Mounted,
    /// gio failed; its message with every answered password redacted.
    Failed(String),
    Cancelled,
}

/// What the worker reports.
#[derive(Debug)]
pub enum MountEvent {
    /// gio waits for an answer: send it with [`MountControl::answer`].
    Prompt(Prompt),
    /// The attempt is over; nothing follows.
    Done(MountOutcome),
}

enum Inbound {
    Out(Vec<u8>),
    OutEnd,
    Err(Vec<u8>),
    ErrEnd,
    Answer(Answer),
    Cancel,
}

/// Talks to a running attempt from any thread.
#[derive(Clone)]
pub struct MountControl {
    inbox: Sender<Inbound>,
}

impl std::fmt::Debug for MountControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountControl").finish_non_exhaustive()
    }
}

impl MountControl {
    /// Answers the prompt gio waits at. An attempt that already ended ignores
    /// it (the answer is dropped, and zeroed, here).
    pub fn answer(&self, answer: Answer) {
        self.inbox.send(Inbound::Answer(answer)).ok();
    }

    /// Kills gio; the attempt reports [`MountOutcome::Cancelled`].
    pub fn cancel(&self) {
        self.inbox.send(Inbound::Cancel).ok();
    }
}

/// A running `gio mount`.
#[derive(Debug)]
pub struct MountSession {
    pub events: Receiver<MountEvent>,
    pub control: MountControl,
}

const STDERR_CAP: usize = 16 * 1024;
const MESSAGE_CAP: usize = 300;
/// How long the pipes may keep delivering after gio exited.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(500);
const TICK: Duration = Duration::from_millis(100);

fn pump<R: std::io::Read + Send + 'static>(
    name: &str,
    mut reader: R,
    inbox: Sender<Inbound>,
    data: fn(Vec<u8>) -> Inbound,
    end: fn() -> Inbound,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let chunk = buffer.get(..count).unwrap_or_default().to_vec();
                        if inbox.send(data(chunk)).is_err() {
                            return;
                        }
                    }
                }
            }
            inbox.send(end()).ok();
        })
        .map(drop)
}

/// Starts `gio mount <uri>` (`gio` is the program to run). The address goes
/// in argv; it never holds a password. gio runs with `LC_ALL=C`, so its
/// prompts are the English ones [`PromptScanner`] knows.
///
/// # Errors
///
/// `gio` cannot be started.
pub fn spawn_mount(gio: &Path, uri: &ServerUri) -> io::Result<MountSession> {
    spawn_gio_mount(gio, &[OsString::from("mount"), uri.to_string().into()])
}

/// Starts `gio mount -d <device>`, which unlocks an encrypted volume and
/// mounts what is inside; gio asks for the passphrase like for a password,
/// and the same prompt relay, redaction and cancel apply. The device is an
/// [`UnlockTarget`]: only a locked container of the device listing.
///
/// # Errors
///
/// `gio` cannot be started.
pub fn spawn_unlock(gio: &Path, target: &UnlockTarget) -> io::Result<MountSession> {
    spawn_gio_mount(gio, &unlock_args(target))
}

/// The shared runner of every `gio mount` attempt: `args` come from a typed
/// builder, never from the person, and hold no secret.
fn spawn_gio_mount(gio: &Path, args: &[OsString]) -> io::Result<MountSession> {
    let mut child = Command::new(gio)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let missing = || io::Error::other("gio did not open its standard streams");
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        child.kill().ok();
        child.wait().ok();
        return Err(missing());
    };
    let (inbox, inbound) = channel();
    let (events_tx, events) = channel();
    let readers = pump(
        "fileflux-gio-out",
        stdout,
        inbox.clone(),
        Inbound::Out,
        || Inbound::OutEnd,
    )
    .and_then(|()| {
        pump(
            "fileflux-gio-err",
            stderr,
            inbox.clone(),
            Inbound::Err,
            || Inbound::ErrEnd,
        )
    });
    if let Err(error) = readers {
        child.kill().ok();
        child.wait().ok();
        return Err(error);
    }
    let worker = Worker {
        child,
        stdin: Some(stdin),
        inbound,
        events: events_tx,
        scanner: PromptScanner::default(),
        asked: None,
        secrets: Vec::new(),
        stderr: Vec::new(),
    };
    std::thread::Builder::new()
        .name("fileflux-gio-mount".to_owned())
        .spawn(move || worker.run())?;
    Ok(MountSession {
        events,
        control: MountControl { inbox },
    })
}

struct Worker {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    inbound: Receiver<Inbound>,
    events: Sender<MountEvent>,
    scanner: PromptScanner,
    /// The kind of the prompt waiting for an answer.
    asked: Option<PromptKind>,
    /// The passwords answered, to redact them from gio's message; zeroed
    /// when the worker ends.
    secrets: Vec<Secret>,
    stderr: Vec<u8>,
}

impl Worker {
    fn run(mut self) {
        let outcome = self.drive();
        self.child.kill().ok();
        self.child.wait().ok();
        self.events.send(MountEvent::Done(outcome)).ok();
    }

    fn drive(&mut self) -> MountOutcome {
        let (mut out_open, mut err_open) = (true, true);
        let mut exited: Option<(std::process::ExitStatus, Instant)> = None;
        loop {
            match self.inbound.recv_timeout(TICK) {
                Ok(Inbound::Out(bytes)) => {
                    for prompt in self.scanner.feed(&bytes) {
                        self.asked = prompt.kind();
                        if self.events.send(MountEvent::Prompt(prompt)).is_err() {
                            // Nobody can answer any more.
                            return MountOutcome::Cancelled;
                        }
                    }
                }
                Ok(Inbound::OutEnd) => out_open = false,
                Ok(Inbound::Err(bytes)) => {
                    let room = STDERR_CAP.saturating_sub(self.stderr.len());
                    self.stderr
                        .extend_from_slice(bytes.get(..room.min(bytes.len())).unwrap_or_default());
                }
                Ok(Inbound::ErrEnd) => err_open = false,
                Ok(Inbound::Answer(answer)) => {
                    if let Some(outcome) = self.answer(answer) {
                        return outcome;
                    }
                }
                Ok(Inbound::Cancel) => return MountOutcome::Cancelled,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    out_open = false;
                    err_open = false;
                }
            }
            if exited.is_none()
                && let Ok(Some(status)) = self.child.try_wait()
            {
                exited = Some((status, Instant::now()));
            }
            let drained = !out_open && !err_open;
            // gio that closed its output but still runs is waited for by
            // polling, so a Cancel still reaches it.
            if let Some((status, at)) = exited
                && (drained || at.elapsed() >= DRAIN_AFTER_EXIT)
            {
                return self.finish(status);
            }
            if drained {
                std::thread::sleep(TICK);
            }
        }
    }

    /// Writes the answer to gio; `Some` ends the attempt.
    fn answer(&mut self, answer: Answer) -> Option<MountOutcome> {
        self.stdin.as_mut()?;
        let written = match answer {
            Answer::Choice(number) => self.write_line(format!("{number}").as_bytes()),
            Answer::Line(secret) => {
                if secret.has_line_break() {
                    return Some(MountOutcome::Failed(
                        "The answer cannot contain a line break".to_owned(),
                    ));
                }
                let written = self.write_line(&secret.0);
                if self.asked == Some(PromptKind::Password) && !secret.0.is_empty() {
                    self.secrets.push(secret);
                }
                written
            }
        };
        self.asked = None;
        // gio that stopped reading has exited or is about to: its exit
        // status says what happened.
        written.ok();
        None
    }

    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::other("gio no longer reads"))?;
        stdin.write_all(line)?;
        stdin.write_all(b"\n")?;
        stdin.flush()
    }

    fn finish(&mut self, status: std::process::ExitStatus) -> MountOutcome {
        self.stdin = None;
        let message = self.message();
        if status.success() || message.to_lowercase().contains("already mounted") {
            return MountOutcome::Mounted;
        }
        if !message.is_empty() {
            return MountOutcome::Failed(message);
        }
        MountOutcome::Failed(match status.code() {
            Some(code) => format!("gio mount failed (exit status {code})"),
            None => "gio mount was stopped by a signal".to_owned(),
        })
    }

    /// What gio said on its error stream: redacted of every answered
    /// password, stripped of control characters and of the `gio:` prefix.
    fn message(&self) -> String {
        let mut text = self.stderr.clone();
        // The longest first: a short password inside a longer one must not
        // leave the rest of the longer one readable.
        let mut secrets: Vec<&Secret> = self.secrets.iter().collect();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.0.len()));
        for secret in secrets {
            text = redact(&text, &secret.0);
        }
        let text = String::from_utf8_lossy(&text);
        let lines: Vec<String> = text
            .lines()
            .map(clean)
            .map(|line| {
                line.strip_prefix("gio:")
                    .map_or(line.clone(), |rest| rest.trim().to_owned())
            })
            .filter(|line| !line.is_empty() && !line.contains("will be echoed"))
            .collect();
        lines.join(" ").chars().take(MESSAGE_CAP).collect()
    }
}

/// `text` with every occurrence of `secret` replaced by `***`.
fn redact(text: &[u8], secret: &[u8]) -> Vec<u8> {
    if secret.is_empty() {
        return text.to_vec();
    }
    let mut out = Vec::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if rest.starts_with(secret) {
            out.extend_from_slice(b"***");
            rest = rest.get(secret.len()..).unwrap_or_default();
        } else if let Some((first, tail)) = rest.split_first() {
            out.push(*first);
            rest = tail;
        }
    }
    out
}

#[cfg(test)]
#[path = "network_tests.rs"]
mod tests;
