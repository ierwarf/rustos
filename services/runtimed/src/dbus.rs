//! Minimal Ring3 D-Bus message bus broker (system + per-session buses).
//!
//! - **Owner:** `runtimed` hosts the Ring3 D-Bus listeners on `/run/dbus/system_bus_socket`
//!   and `{runtime_dir}/bus` so Linux toolkits (`libdbus-1`, Chromium, QtDBus) connect
//!   over genuine `AF_UNIX` `SOCK_STREAM` sockets with kernel `SO_PEERCRED` auth.
//! - **Protocol:** D-Bus SASL `EXTERNAL` authentication + D-Bus binary wire v1
//!   (`org.freedesktop.DBus` bus management + peer-to-peer routing + signal matching).
//! - **Concurrency:** Runs on a dedicated `runtimed-dbus` background thread driven by
//!   nonblocking I/O and `libc::poll`; never blocks the main `runtimed` broker loop.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::CString;
use std::io::{ErrorKind, Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{chown, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use rustos_user_abi::desktop_session::{launch_credentials, RuntimeDir};

pub(crate) const SYSTEM_BUS_SOCKET_PATH: &str = "/run/dbus/system_bus_socket";
const DEFAULT_BUS_UUID: &str = "727573746f7364627573303030303031";
const MAX_SASL_LINE_BYTES: usize = 512;
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CLIENT_IN_BYTES: usize = 4 * 1024 * 1024 + 4096;
const MAX_CLIENT_OUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_BUS_CLIENTS: usize = 128;
const MAX_MATCHES_PER_CLIENT: usize = 256;
const MAX_NAMES_PER_CLIENT: usize = 256;
const MAX_READS_PER_TURN: usize = 64;

const MSG_METHOD_CALL: u8 = 1;
const MSG_METHOD_RETURN: u8 = 2;
const MSG_ERROR: u8 = 3;
const MSG_SIGNAL: u8 = 4;

const FLAG_NO_REPLY_EXPECTED: u8 = 0x1;

const FIELD_PATH: u8 = 1;
const FIELD_INTERFACE: u8 = 2;
const FIELD_MEMBER: u8 = 3;
const FIELD_ERROR_NAME: u8 = 4;
const FIELD_REPLY_SERIAL: u8 = 5;
const FIELD_DESTINATION: u8 = 6;
const FIELD_SENDER: u8 = 7;
const FIELD_SIGNATURE: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BusKind {
    System,
    Session(u32),
}

#[derive(Clone, Copy, Debug)]
struct PeerCreds {
    pid: u32,
    uid: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MatchRule {
    msg_type: Option<u8>,
    sender: Option<String>,
    interface: Option<String>,
    member: Option<String>,
    path: Option<String>,
    destination: Option<String>,
    arg0: Option<String>,
}

impl MatchRule {
    fn parse(spec: &str) -> Self {
        let mut rule = Self::default();
        for part in split_match_parts(spec) {
            let Some((k, v)) = part.split_once('=') else {
                continue;
            };
            let key = k.trim();
            let val = unquote_match_value(v.trim());
            match key {
                "type" => {
                    rule.msg_type = match val.as_str() {
                        "signal" => Some(MSG_SIGNAL),
                        "method_call" => Some(MSG_METHOD_CALL),
                        "method_return" => Some(MSG_METHOD_RETURN),
                        "error" => Some(MSG_ERROR),
                        _ => None,
                    };
                }
                "sender" => rule.sender = Some(val),
                "interface" => rule.interface = Some(val),
                "member" => rule.member = Some(val),
                "path" => rule.path = Some(val),
                "destination" => rule.destination = Some(val),
                "arg0" => rule.arg0 = Some(val),
                _ => {}
            }
        }
        rule
    }

    fn matches(&self, msg: &DbusMessage, sender_names: &[&str], arg0: Option<&str>) -> bool {
        if let Some(t) = self.msg_type {
            if msg.msg_type != t {
                return false;
            }
        }
        if let Some(ref iface) = self.interface {
            if msg.interface.as_deref() != Some(iface.as_str()) {
                return false;
            }
        }
        if let Some(ref member) = self.member {
            if msg.member.as_deref() != Some(member.as_str()) {
                return false;
            }
        }
        if let Some(ref path) = self.path {
            if msg.path.as_deref() != Some(path.as_str()) {
                return false;
            }
        }
        if let Some(ref dest) = self.destination {
            if msg.destination.as_deref() != Some(dest.as_str()) {
                return false;
            }
        }
        if let Some(ref want_sender) = self.sender {
            if !sender_names.iter().any(|name| *name == want_sender.as_str()) {
                return false;
            }
        }
        if let Some(ref want_arg0) = self.arg0 {
            if arg0 != Some(want_arg0.as_str()) {
                return false;
            }
        }
        true
    }
}

fn split_match_parts(spec: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let bytes = spec.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => in_quotes = !in_quotes,
            b'\\' if in_quotes && i + 1 < bytes.len() => i += 1,
            b',' if !in_quotes => {
                parts.push(&spec[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start <= spec.len() {
        parts.push(&spec[start..]);
    }
    parts
}

fn unquote_match_value(raw: &str) -> String {
    if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        raw[1..raw.len() - 1].to_owned()
    } else {
        raw.to_owned()
    }
}

#[derive(Clone, Debug)]
struct DbusMessage {
    endian: u8,
    msg_type: u8,
    flags: u8,
    serial: u32,
    path: Option<String>,
    interface: Option<String>,
    member: Option<String>,
    error_name: Option<String>,
    reply_serial: Option<u32>,
    destination: Option<String>,
    sender: Option<String>,
    signature: String,
    body: Vec<u8>,
}

impl DbusMessage {
    fn decode(buf: &[u8]) -> Result<Option<(Self, usize)>, &'static str> {
        if buf.len() < 16 {
            return Ok(None);
        }
        let endian = buf[0];
        if endian != b'l' && endian != b'B' {
            return Err("invalid endian");
        }
        let msg_type = buf[1];
        let flags = buf[2];
        let version = buf[3];
        if version != 1 || !(1..=4).contains(&msg_type) {
            return Err("invalid header");
        }
        let body_len = read_u32(endian, &buf[4..8]) as usize;
        let serial = read_u32(endian, &buf[8..12]);
        let fields_len = read_u32(endian, &buf[12..16]) as usize;
        if serial == 0 || fields_len > MAX_MESSAGE_BYTES || body_len > MAX_MESSAGE_BYTES {
            return Err("invalid lengths");
        }
        let header_end = align_up(16 + fields_len, 8);
        let total = header_end
            .checked_add(body_len)
            .ok_or("message length overflow")?;
        if total > MAX_MESSAGE_BYTES {
            return Err("oversized message");
        }
        if buf.len() < total {
            return Ok(None);
        }

        let mut msg = Self {
            endian,
            msg_type,
            flags,
            serial,
            path: None,
            interface: None,
            member: None,
            error_name: None,
            reply_serial: None,
            destination: None,
            sender: None,
            signature: String::new(),
            body: buf[header_end..total].to_vec(),
        };

        let mut pos = 16;
        let fields_end = 16 + fields_len;
        while pos < fields_end {
            pos = align_up(pos, 8);
            if pos >= fields_end {
                break;
            }
            let code = buf[pos];
            pos += 1;
            if pos >= fields_end {
                return Err("truncated field signature");
            }
            let sig_len = buf[pos] as usize;
            pos += 1;
            if pos + sig_len + 1 > fields_end {
                return Err("truncated field signature body");
            }
            let sig = std::str::from_utf8(&buf[pos..pos + sig_len]).map_err(|_| "bad sig")?;
            pos += sig_len + 1; // + NUL
            match (code, sig) {
                (FIELD_PATH, "o") => msg.path = Some(read_dbus_string(endian, &buf[..fields_end], &mut pos)?),
                (FIELD_INTERFACE, "s") => msg.interface = Some(read_dbus_string(endian, &buf[..fields_end], &mut pos)?),
                (FIELD_MEMBER, "s") => msg.member = Some(read_dbus_string(endian, &buf[..fields_end], &mut pos)?),
                (FIELD_ERROR_NAME, "s") => msg.error_name = Some(read_dbus_string(endian, &buf[..fields_end], &mut pos)?),
                (FIELD_DESTINATION, "s") => msg.destination = Some(read_dbus_string(endian, &buf[..fields_end], &mut pos)?),
                (FIELD_SENDER, "s") => msg.sender = Some(read_dbus_string(endian, &buf[..fields_end], &mut pos)?),
                (FIELD_REPLY_SERIAL, "u") => {
                    pos = align_up(pos, 4);
                    if pos + 4 > fields_end {
                        return Err("truncated reply serial");
                    }
                    msg.reply_serial = Some(read_u32(endian, &buf[pos..pos + 4]));
                    pos += 4;
                }
                (FIELD_SIGNATURE, "g") => {
                    if pos >= fields_end {
                        return Err("truncated body signature");
                    }
                    let len = buf[pos] as usize;
                    pos += 1;
                    if pos + len + 1 > fields_end {
                        return Err("truncated body signature text");
                    }
                    msg.signature = std::str::from_utf8(&buf[pos..pos + len])
                        .map_err(|_| "bad body sig")?
                        .to_owned();
                    pos += len + 1;
                }
                (_, "s" | "o") => {
                    let _ = read_dbus_string(endian, &buf[..fields_end], &mut pos)?;
                }
                (_, "u" | "i") => {
                    pos = align_up(pos, 4);
                    if pos + 4 > fields_end {
                        return Err("truncated u32 field");
                    }
                    pos += 4;
                }
                (_, "g") => {
                    if pos >= fields_end {
                        return Err("truncated g field");
                    }
                    let len = buf[pos] as usize;
                    pos += 1 + len + 1;
                }
                (_, "y") => {
                    if pos >= fields_end {
                        return Err("truncated y field");
                    }
                    pos += 1;
                }
                _ => return Err("unsupported header field type"),
            }
        }
        Ok(Some((msg, total)))
    }

    fn encode(&self) -> Vec<u8> {
        let mut fields = Vec::with_capacity(128);
        let endian = self.endian;
        let push_str_field = |fields: &mut Vec<u8>, code: u8, sig: &str, val: &str| {
            pad_to(fields, 8);
            fields.push(code);
            fields.push(sig.len() as u8);
            fields.extend_from_slice(sig.as_bytes());
            fields.push(0);
            pad_to(fields, 4);
            write_u32(endian, fields, val.len() as u32);
            fields.extend_from_slice(val.as_bytes());
            fields.push(0);
        };
        if let Some(ref p) = self.path {
            push_str_field(&mut fields, FIELD_PATH, "o", p);
        }
        if let Some(ref i) = self.interface {
            push_str_field(&mut fields, FIELD_INTERFACE, "s", i);
        }
        if let Some(ref m) = self.member {
            push_str_field(&mut fields, FIELD_MEMBER, "s", m);
        }
        if let Some(ref e) = self.error_name {
            push_str_field(&mut fields, FIELD_ERROR_NAME, "s", e);
        }
        if let Some(rs) = self.reply_serial {
            pad_to(&mut fields, 8);
            fields.push(FIELD_REPLY_SERIAL);
            fields.push(1);
            fields.push(b'u');
            fields.push(0);
            pad_to(&mut fields, 4);
            write_u32(endian, &mut fields, rs);
        }
        if let Some(ref d) = self.destination {
            push_str_field(&mut fields, FIELD_DESTINATION, "s", d);
        }
        if let Some(ref s) = self.sender {
            push_str_field(&mut fields, FIELD_SENDER, "s", s);
        }
        if !self.signature.is_empty() {
            pad_to(&mut fields, 8);
            fields.push(FIELD_SIGNATURE);
            fields.push(1);
            fields.push(b'g');
            fields.push(0);
            fields.push(self.signature.len() as u8);
            fields.extend_from_slice(self.signature.as_bytes());
            fields.push(0);
        }

        let mut out = Vec::with_capacity(align_up(16 + fields.len(), 8) + self.body.len());
        out.push(endian);
        out.push(self.msg_type);
        out.push(self.flags);
        out.push(1);
        write_u32(endian, &mut out, self.body.len() as u32);
        write_u32(endian, &mut out, self.serial);
        write_u32(endian, &mut out, fields.len() as u32);
        out.extend_from_slice(&fields);
        pad_to(&mut out, 8);
        out.extend_from_slice(&self.body);
        out
    }
}

fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

fn pad_to(buf: &mut Vec<u8>, align: usize) {
    let target = align_up(buf.len(), align);
    buf.resize(target, 0);
}

fn read_u32(endian: u8, bytes: &[u8]) -> u32 {
    let arr: [u8; 4] = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if endian == b'B' {
        u32::from_be_bytes(arr)
    } else {
        u32::from_le_bytes(arr)
    }
}

fn write_u32(endian: u8, out: &mut Vec<u8>, val: u32) {
    let bytes = if endian == b'B' {
        val.to_be_bytes()
    } else {
        val.to_le_bytes()
    };
    out.extend_from_slice(&bytes);
}

fn read_dbus_string(endian: u8, buf: &[u8], pos: &mut usize) -> Result<String, &'static str> {
    read_dbus_str(endian, buf, pos).map(str::to_owned)
}

fn read_dbus_str<'a>(endian: u8, buf: &'a [u8], pos: &mut usize) -> Result<&'a str, &'static str> {
    *pos = align_up(*pos, 4);
    if *pos + 4 > buf.len() {
        return Err("truncated string length");
    }
    let len = read_u32(endian, &buf[*pos..*pos + 4]) as usize;
    *pos += 4;
    if *pos + len + 1 > buf.len() {
        return Err("truncated string bytes");
    }
    let bytes = &buf[*pos..*pos + len];
    if buf[*pos + len] != 0 || bytes.contains(&0) {
        return Err("invalid string termination");
    }
    let s = std::str::from_utf8(bytes).map_err(|_| "invalid utf8")?;
    *pos += len + 1;
    Ok(s)
}

struct BodyReader<'a> {
    endian: u8,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BodyReader<'a> {
    fn new(endian: u8, bytes: &'a [u8]) -> Self {
        Self {
            endian,
            bytes,
            pos: 0,
        }
    }

    fn read_str(&mut self) -> Result<String, &'static str> {
        read_dbus_string(self.endian, self.bytes, &mut self.pos)
    }

    fn read_str_ref(&mut self) -> Result<&'a str, &'static str> {
        read_dbus_str(self.endian, self.bytes, &mut self.pos)
    }

    fn read_u32(&mut self) -> Result<u32, &'static str> {
        self.pos = align_up(self.pos, 4);
        if self.pos + 4 > self.bytes.len() {
            return Err("truncated u32");
        }
        let val = read_u32(self.endian, &self.bytes[self.pos..self.pos + 4]);
        self.pos += 4;
        Ok(val)
    }
}

fn encode_string(buf: &mut Vec<u8>, s: &str) {
    pad_to(buf, 4);
    write_u32(b'l', buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

fn encode_u32(buf: &mut Vec<u8>, val: u32) {
    pad_to(buf, 4);
    write_u32(b'l', buf, val);
}

fn encode_bool(buf: &mut Vec<u8>, val: bool) {
    encode_u32(buf, u32::from(val));
}

fn encode_string_array(buf: &mut Vec<u8>, items: &[String]) {
    pad_to(buf, 4);
    let len_pos = buf.len();
    write_u32(b'l', buf, 0);
    let start = buf.len();
    for item in items {
        encode_string(buf, item);
    }
    let byte_len = (buf.len() - start) as u32;
    buf[len_pos..len_pos + 4].copy_from_slice(&byte_len.to_le_bytes());
}

fn encode_credentials_dict(buf: &mut Vec<u8>, uid: u32, pid: u32) {
    pad_to(buf, 4);
    let len_pos = buf.len();
    write_u32(b'l', buf, 0);
    pad_to(buf, 8);
    let start = buf.len();
    for (key, val) in [("UnixUserID", uid), ("ProcessID", pid)] {
        pad_to(buf, 8);
        encode_string(buf, key);
        buf.push(1);
        buf.push(b'u');
        buf.push(0);
        encode_u32(buf, val);
    }
    let byte_len = (buf.len() - start) as u32;
    buf[len_pos..len_pos + 4].copy_from_slice(&byte_len.to_le_bytes());
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClientAuthState {
    AwaitingNul,
    Sasl { pending_external: bool, ok_sent: bool },
    Authenticated,
}

struct BusClient {
    id: u64,
    bus: BusKind,
    stream: UnixStream,
    creds: PeerCreds,
    auth: ClientAuthState,
    unique_name: Option<String>,
    in_buf: Vec<u8>,
    out_buf: VecDeque<u8>,
    matches: Vec<MatchRule>,
    /// Set when a message had to be dropped because the peer stopped reading;
    /// the broker disconnects such a client instead of silently losing traffic.
    overflowed: bool,
}

impl BusClient {
    fn queue_bytes(&mut self, bytes: &[u8]) -> bool {
        if self.out_buf.len().saturating_add(bytes.len()) > MAX_CLIENT_OUT_BYTES {
            self.overflowed = true;
            return false;
        }
        self.out_buf.extend(bytes);
        true
    }

    fn queue_message(&mut self, msg: &DbusMessage) -> bool {
        self.queue_bytes(&msg.encode())
    }
}

struct BusState {
    bus_uuid: String,
    next_serial: u32,
    next_client_id: u64,
    clients: BTreeMap<u64, BusClient>,
    /// `(bus_kind, well_known_name) -> client_id`
    names: HashMap<(u32, String), u64>,
}

impl BusState {
    fn new() -> Self {
        Self {
            bus_uuid: DEFAULT_BUS_UUID.to_owned(),
            next_serial: 1,
            next_client_id: 1,
            clients: BTreeMap::new(),
            names: HashMap::new(),
        }
    }

    fn alloc_serial(&mut self) -> u32 {
        let s = self.next_serial.max(1);
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        s
    }

    fn bus_scope(bus: BusKind) -> u32 {
        match bus {
            BusKind::System => u32::MAX,
            BusKind::Session(uid) => uid,
        }
    }

    fn find_owner(&self, bus: BusKind, name: &str) -> Option<u64> {
        if name.starts_with(':') {
            return self
                .clients
                .values()
                .find(|c| c.bus == bus && c.unique_name.as_deref() == Some(name))
                .map(|c| c.id);
        }
        self.names.get(&(Self::bus_scope(bus), name.to_owned())).copied()
    }

    fn disconnect_client(&mut self, client_id: u64) {
        let Some(client) = self.clients.remove(&client_id) else {
            return;
        };
        let scope = Self::bus_scope(client.bus);
        let owned: Vec<String> = self
            .names
            .iter()
            .filter_map(|((s, name), owner)| (*s == scope && *owner == client_id).then(|| name.clone()))
            .collect();
        for name in &owned {
            self.names.remove(&(scope, name.clone()));
            if let Some(ref unique) = client.unique_name {
                self.broadcast_name_owner_changed(client.bus, name, unique, "");
            }
        }
        if let Some(ref unique) = client.unique_name {
            self.broadcast_name_owner_changed(client.bus, unique, unique, "");
        }
    }

    fn broadcast_name_owner_changed(&mut self, bus: BusKind, name: &str, old: &str, new: &str) {
        let mut body = Vec::new();
        encode_string(&mut body, name);
        encode_string(&mut body, old);
        encode_string(&mut body, new);
        let serial = self.alloc_serial();
        let signal = DbusMessage {
            endian: b'l',
            msg_type: MSG_SIGNAL,
            flags: FLAG_NO_REPLY_EXPECTED,
            serial,
            path: Some(String::from("/org/freedesktop/DBus")),
            interface: Some(String::from("org.freedesktop.DBus")),
            member: Some(String::from("NameOwnerChanged")),
            error_name: None,
            reply_serial: None,
            destination: None,
            sender: Some(String::from("org.freedesktop.DBus")),
            signature: String::from("sss"),
            body,
        };
        let encoded = signal.encode();
        for client in self.clients.values_mut() {
            if client.bus != bus || client.auth != ClientAuthState::Authenticated {
                continue;
            }
            if client
                .matches
                .iter()
                .any(|rule| rule.matches(&signal, &["org.freedesktop.DBus"], Some(name)))
            {
                let _ = client.queue_bytes(&encoded);
            }
        }
    }

    fn process_client_read(&mut self, client_id: u64) -> bool {
        let mut chunk = [0_u8; 8192];
        // Bounded per poll turn so one chatty client cannot starve the others;
        // the level-triggered poll re-reports any remaining input.
        for _ in 0..MAX_READS_PER_TURN {
            let res = {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                if client.in_buf.len() >= MAX_CLIENT_IN_BYTES {
                    return false;
                }
                let room = chunk.len().min(MAX_CLIENT_IN_BYTES - client.in_buf.len());
                client.stream.read(&mut chunk[..room])
            };
            match res {
                Ok(0) => return false,
                Ok(n) => {
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        client.in_buf.extend_from_slice(&chunk[..n]);
                    }
                    // Consume complete messages after every read so the input
                    // cap only ever bounds one partial message.
                    if !self.advance_sasl(client_id) || !self.advance_messages(client_id) {
                        return false;
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
        true
    }

    fn advance_sasl(&mut self, client_id: u64) -> bool {
        let uuid = &self.bus_uuid;
        loop {
            let Some(client) = self.clients.get_mut(&client_id) else {
                return false;
            };
            match client.auth {
                ClientAuthState::Authenticated => return true,
                ClientAuthState::AwaitingNul => {
                    if client.in_buf.is_empty() {
                        return true;
                    }
                    if client.in_buf[0] != 0 {
                        return false;
                    }
                    client.in_buf.remove(0);
                    client.auth = ClientAuthState::Sasl {
                        pending_external: false,
                        ok_sent: false,
                    };
                }
                ClientAuthState::Sasl {
                    mut pending_external,
                    mut ok_sent,
                } => {
                    let Some(pos) = client.in_buf.windows(2).position(|w| w == b"\r\n") else {
                        if client.in_buf.len() > MAX_SASL_LINE_BYTES {
                            return false;
                        }
                        return true;
                    };
                    if pos > MAX_SASL_LINE_BYTES {
                        return false;
                    }
                    let line_bytes: Vec<u8> = client.in_buf.drain(..pos + 2).collect();
                    let Ok(line) = std::str::from_utf8(&line_bytes[..pos]) else {
                        return false;
                    };
                    if line == "BEGIN" && ok_sent {
                        client.auth = ClientAuthState::Authenticated;
                        return true;
                    } else if line == "NEGOTIATE_UNIX_FD" {
                        if !client.queue_bytes(b"ERROR \"Unix fd passing not supported\"\r\n") {
                            return false;
                        }
                    } else if line == "CANCEL" || line == "AUTH" {
                        pending_external = false;
                        ok_sent = false;
                        if !client.queue_bytes(b"REJECTED EXTERNAL\r\n") {
                            return false;
                        }
                    } else if line == "AUTH EXTERNAL" {
                        pending_external = true;
                        if !client.queue_bytes(b"DATA\r\n") {
                            return false;
                        }
                    } else if let Some(rest) = line.strip_prefix("AUTH EXTERNAL ") {
                        if verify_external_hex(rest.trim(), client.creds.uid) {
                            pending_external = false;
                            ok_sent = true;
                            if !client.queue_bytes(format!("OK {uuid}\r\n").as_bytes()) {
                                return false;
                            }
                        } else if !client.queue_bytes(b"REJECTED EXTERNAL\r\n") {
                            return false;
                        }
                    } else if pending_external && (line == "DATA" || line.starts_with("DATA ")) {
                        let arg = line.strip_prefix("DATA").unwrap_or("").trim();
                        if arg.is_empty() || verify_external_hex(arg, client.creds.uid) {
                            pending_external = false;
                            ok_sent = true;
                            if !client.queue_bytes(format!("OK {uuid}\r\n").as_bytes()) {
                                return false;
                            }
                        } else {
                            pending_external = false;
                            if !client.queue_bytes(b"REJECTED EXTERNAL\r\n") {
                                return false;
                            }
                        }
                    } else if line.starts_with("AUTH ") {
                        if !client.queue_bytes(b"REJECTED EXTERNAL\r\n") {
                            return false;
                        }
                    } else if !client.queue_bytes(b"ERROR \"Unknown command\"\r\n") {
                        return false;
                    }
                    client.auth = ClientAuthState::Sasl {
                        pending_external,
                        ok_sent,
                    };
                }
            }
        }
    }

    fn advance_messages(&mut self, client_id: u64) -> bool {
        let mut offset = 0;
        loop {
            let decoded = {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                if client.auth != ClientAuthState::Authenticated {
                    return true;
                }
                match DbusMessage::decode(&client.in_buf[offset..]) {
                    Ok(Some((msg, consumed))) => {
                        offset += consumed;
                        Some(msg)
                    }
                    Ok(None) => {
                        // Dispatch in wire order, then move the incomplete
                        // suffix only once. Draining each complete frame made
                        // a batch of small messages move its tail repeatedly.
                        client.in_buf.drain(..offset);
                        None
                    }
                    Err(_) => return false,
                }
            };
            let Some(msg) = decoded else {
                return true;
            };
            if !self.dispatch_message(client_id, msg) {
                return false;
            }
        }
    }

    fn dispatch_message(&mut self, sender_id: u64, mut msg: DbusMessage) -> bool {
        let Some((bus, sender_unique, sender_creds)) = self.clients.get(&sender_id).map(|c| {
            (c.bus, c.unique_name.clone(), c.creds)
        }) else {
            return false;
        };

        if sender_unique.is_none() {
            let is_hello = msg.msg_type == MSG_METHOD_CALL
                && msg.destination.as_deref() == Some("org.freedesktop.DBus")
                && msg.member.as_deref() == Some("Hello");
            if !is_hello {
                return false;
            }
            let unique = format!(":1.{sender_id}");
            if let Some(c) = self.clients.get_mut(&sender_id) {
                c.unique_name = Some(unique.clone());
            }
            let mut reply_body = Vec::new();
            encode_string(&mut reply_body, &unique);
            let reply_serial = self.alloc_serial();
            let reply = DbusMessage {
                endian: b'l',
                msg_type: MSG_METHOD_RETURN,
                flags: FLAG_NO_REPLY_EXPECTED,
                serial: reply_serial,
                path: None,
                interface: None,
                member: None,
                error_name: None,
                reply_serial: Some(msg.serial),
                destination: Some(unique.clone()),
                sender: Some(String::from("org.freedesktop.DBus")),
                signature: String::from("s"),
                body: reply_body.clone(),
            };
            let acq_serial = self.alloc_serial();
            let acquired = DbusMessage {
                endian: b'l',
                msg_type: MSG_SIGNAL,
                flags: FLAG_NO_REPLY_EXPECTED,
                serial: acq_serial,
                path: Some(String::from("/org/freedesktop/DBus")),
                interface: Some(String::from("org.freedesktop.DBus")),
                member: Some(String::from("NameAcquired")),
                error_name: None,
                reply_serial: None,
                destination: Some(unique.clone()),
                sender: Some(String::from("org.freedesktop.DBus")),
                signature: String::from("s"),
                body: reply_body,
            };
            let ok = if let Some(c) = self.clients.get_mut(&sender_id) {
                c.queue_message(&reply) && c.queue_message(&acquired)
            } else {
                false
            };
            if ok {
                self.broadcast_name_owner_changed(bus, &unique, "", &unique);
            }
            return ok;
        }

        let unique = sender_unique.unwrap_or_default();
        if msg.destination.as_deref() == Some("org.freedesktop.DBus") {
            return self.handle_driver_call(sender_id, bus, &unique, sender_creds, &msg);
        }

        msg.sender = Some(unique.clone());
        let encoded = msg.encode();
        if let Some(ref dest) = msg.destination {
            if let Some(target_id) = self.find_owner(bus, dest) {
                if let Some(target) = self.clients.get_mut(&target_id) {
                    let _ = target.queue_bytes(&encoded);
                }
            } else if msg.msg_type == MSG_METHOD_CALL && (msg.flags & FLAG_NO_REPLY_EXPECTED) == 0 {
                self.send_error(
                    sender_id,
                    &unique,
                    msg.serial,
                    "org.freedesktop.DBus.Error.ServiceUnknown",
                    &format!("The name {dest} was not provided by any .service files"),
                );
            }
            return true;
        }

        let scope = Self::bus_scope(bus);
        let mut sender_refs = vec![unique.as_str()];
        for ((s, name), owner) in &self.names {
            if *s == scope && *owner == sender_id {
                sender_refs.push(name.as_str());
            }
        }
        let arg0 = if msg.signature.starts_with('s') {
            BodyReader::new(msg.endian, &msg.body).read_str_ref().ok()
        } else {
            None
        };
        for client in self.clients.values_mut() {
            if client.bus != bus || client.auth != ClientAuthState::Authenticated {
                continue;
            }
            if client
                .matches
                .iter()
                .any(|rule| rule.matches(&msg, &sender_refs, arg0))
            {
                let _ = client.queue_bytes(&encoded);
            }
        }
        true
    }

    fn handle_driver_call(
        &mut self,
        sender_id: u64,
        bus: BusKind,
        unique: &str,
        creds: PeerCreds,
        msg: &DbusMessage,
    ) -> bool {
        if msg.msg_type != MSG_METHOD_CALL {
            return true;
        }
        let no_reply = (msg.flags & FLAG_NO_REPLY_EXPECTED) != 0;
        let member = msg.member.as_deref().unwrap_or("");
        let mut reader = BodyReader::new(msg.endian, &msg.body);
        match member {
            "Hello" => {
                if !no_reply {
                    let mut body = Vec::new();
                    encode_string(&mut body, unique);
                    self.send_reply(sender_id, unique, msg.serial, "s", body);
                }
            }
            "RequestName" => {
                let Ok(name) = reader.read_str() else {
                    if !no_reply {
                        self.send_error(sender_id, unique, msg.serial, "org.freedesktop.DBus.Error.InvalidArgs", "bad args");
                    }
                    return true;
                };
                let _flags = reader.read_u32().unwrap_or(0);
                if name.is_empty() || name.starts_with(':') || name == "org.freedesktop.DBus" {
                    if !no_reply {
                        self.send_error(sender_id, unique, msg.serial, "org.freedesktop.DBus.Error.InvalidArgs", "invalid name");
                    }
                    return true;
                }
                let scope = Self::bus_scope(bus);
                let reply_code = match self.names.get(&(scope, name.clone())).copied() {
                    None => {
                        // Every name is a heap key held for the connection's
                        // life; an unbounded count is client-driven growth.
                        let owned = self
                            .names
                            .iter()
                            .filter(|((s, _), owner)| *s == scope && **owner == sender_id)
                            .count();
                        if owned >= MAX_NAMES_PER_CLIENT {
                            if !no_reply {
                                self.send_error(
                                    sender_id,
                                    unique,
                                    msg.serial,
                                    "org.freedesktop.DBus.Error.LimitsExceeded",
                                    "too many names owned by this connection",
                                );
                            }
                            return true;
                        }
                        self.names.insert((scope, name.clone()), sender_id);
                        let mut sig_body = Vec::new();
                        encode_string(&mut sig_body, &name);
                        self.send_signal(sender_id, unique, "NameAcquired", "s", sig_body);
                        self.broadcast_name_owner_changed(bus, &name, "", unique);
                        1 // PRIMARY_OWNER
                    }
                    Some(owner) if owner == sender_id => 4, // ALREADY_OWNER
                    Some(_) => 3, // EXISTS
                };
                if !no_reply {
                    let mut body = Vec::new();
                    encode_u32(&mut body, reply_code);
                    self.send_reply(sender_id, unique, msg.serial, "u", body);
                }
            }
            "ReleaseName" => {
                let Ok(name) = reader.read_str() else {
                    if !no_reply {
                        self.send_error(sender_id, unique, msg.serial, "org.freedesktop.DBus.Error.InvalidArgs", "bad args");
                    }
                    return true;
                };
                let scope = Self::bus_scope(bus);
                let code = match self.names.get(&(scope, name.clone())).copied() {
                    None => 2, // NON_EXISTENT
                    Some(owner) if owner == sender_id => {
                        self.names.remove(&(scope, name.clone()));
                        let mut sig_body = Vec::new();
                        encode_string(&mut sig_body, &name);
                        self.send_signal(sender_id, unique, "NameLost", "s", sig_body);
                        self.broadcast_name_owner_changed(bus, &name, unique, "");
                        1 // RELEASED
                    }
                    Some(_) => 3, // NOT_OWNER
                };
                if !no_reply {
                    let mut body = Vec::new();
                    encode_u32(&mut body, code);
                    self.send_reply(sender_id, unique, msg.serial, "u", body);
                }
            }
            "NameHasOwner" => {
                let Ok(name) = reader.read_str() else {
                    if !no_reply {
                        self.send_error(sender_id, unique, msg.serial, "org.freedesktop.DBus.Error.InvalidArgs", "bad args");
                    }
                    return true;
                };
                let has = name == "org.freedesktop.DBus" || self.find_owner(bus, &name).is_some();
                if !no_reply {
                    let mut body = Vec::new();
                    encode_bool(&mut body, has);
                    self.send_reply(sender_id, unique, msg.serial, "b", body);
                }
            }
            "GetNameOwner" => {
                let Ok(name) = reader.read_str() else {
                    if !no_reply {
                        self.send_error(sender_id, unique, msg.serial, "org.freedesktop.DBus.Error.InvalidArgs", "bad args");
                    }
                    return true;
                };
                if name == "org.freedesktop.DBus" {
                    if !no_reply {
                        let mut body = Vec::new();
                        encode_string(&mut body, "org.freedesktop.DBus");
                        self.send_reply(sender_id, unique, msg.serial, "s", body);
                    }
                } else if let Some(owner) = self
                    .find_owner(bus, &name)
                    .and_then(|id| self.clients.get(&id))
                    .and_then(|c| c.unique_name.clone())
                {
                    if !no_reply {
                        let mut body = Vec::new();
                        encode_string(&mut body, &owner);
                        self.send_reply(sender_id, unique, msg.serial, "s", body);
                    }
                } else if !no_reply {
                    self.send_error(
                        sender_id,
                        unique,
                        msg.serial,
                        "org.freedesktop.DBus.Error.NameHasNoOwner",
                        &format!("Could not get owner of name '{name}': no such name"),
                    );
                }
            }
            "ListNames" => {
                if !no_reply {
                    let scope = Self::bus_scope(bus);
                    let mut names = vec![String::from("org.freedesktop.DBus")];
                    for client in self.clients.values() {
                        if client.bus == bus {
                            if let Some(ref u) = client.unique_name {
                                names.push(u.clone());
                            }
                        }
                    }
                    for (s, name) in self.names.keys() {
                        if *s == scope {
                            names.push(name.clone());
                        }
                    }
                    let mut body = Vec::new();
                    encode_string_array(&mut body, &names);
                    self.send_reply(sender_id, unique, msg.serial, "as", body);
                }
            }
            "ListActivatableNames" => {
                if !no_reply {
                    let mut body = Vec::new();
                    encode_string_array(&mut body, &[]);
                    self.send_reply(sender_id, unique, msg.serial, "as", body);
                }
            }
            "GetConnectionUnixUser" => {
                let Ok(name) = reader.read_str() else {
                    if !no_reply {
                        self.send_error(sender_id, unique, msg.serial, "org.freedesktop.DBus.Error.InvalidArgs", "bad args");
                    }
                    return true;
                };
                let uid = if name == "org.freedesktop.DBus" {
                    Some(0)
                } else {
                    self.find_owner(bus, &name)
                        .and_then(|id| self.clients.get(&id))
                        .map(|c| c.creds.uid)
                };
                if !no_reply {
                    match uid {
                        Some(uid) => {
                            let mut body = Vec::new();
                            encode_u32(&mut body, uid);
                            self.send_reply(sender_id, unique, msg.serial, "u", body);
                        }
                        None => self.send_error(
                            sender_id,
                            unique,
                            msg.serial,
                            "org.freedesktop.DBus.Error.NameHasNoOwner",
                            "no such name",
                        ),
                    }
                }
            }
            "GetConnectionUnixProcessID" => {
                let Ok(name) = reader.read_str() else {
                    if !no_reply {
                        self.send_error(sender_id, unique, msg.serial, "org.freedesktop.DBus.Error.InvalidArgs", "bad args");
                    }
                    return true;
                };
                let pid = if name == "org.freedesktop.DBus" {
                    Some(std::process::id())
                } else {
                    self.find_owner(bus, &name)
                        .and_then(|id| self.clients.get(&id))
                        .map(|c| c.creds.pid)
                };
                if !no_reply {
                    match pid {
                        Some(pid) => {
                            let mut body = Vec::new();
                            encode_u32(&mut body, pid);
                            self.send_reply(sender_id, unique, msg.serial, "u", body);
                        }
                        None => self.send_error(
                            sender_id,
                            unique,
                            msg.serial,
                            "org.freedesktop.DBus.Error.NameHasNoOwner",
                            "no such name",
                        ),
                    }
                }
            }
            "GetConnectionCredentials" => {
                let Ok(name) = reader.read_str() else {
                    if !no_reply {
                        self.send_error(sender_id, unique, msg.serial, "org.freedesktop.DBus.Error.InvalidArgs", "bad args");
                    }
                    return true;
                };
                let target = if name == "org.freedesktop.DBus" {
                    Some(PeerCreds {
                        pid: std::process::id(),
                        uid: 0,
                    })
                } else {
                    self.find_owner(bus, &name)
                        .and_then(|id| self.clients.get(&id))
                        .map(|c| c.creds)
                };
                if !no_reply {
                    match target {
                        Some(c) => {
                            let mut body = Vec::new();
                            encode_credentials_dict(&mut body, c.uid, c.pid);
                            self.send_reply(sender_id, unique, msg.serial, "a{sv}", body);
                        }
                        None => self.send_error(
                            sender_id,
                            unique,
                            msg.serial,
                            "org.freedesktop.DBus.Error.NameHasNoOwner",
                            "no such name",
                        ),
                    }
                }
            }
            "GetId" => {
                if !no_reply {
                    let mut body = Vec::new();
                    encode_string(&mut body, &self.bus_uuid);
                    self.send_reply(sender_id, unique, msg.serial, "s", body);
                }
            }
            "AddMatch" => {
                let Ok(spec) = reader.read_str() else {
                    if !no_reply {
                        self.send_error(sender_id, unique, msg.serial, "org.freedesktop.DBus.Error.InvalidArgs", "bad args");
                    }
                    return true;
                };
                let rule = MatchRule::parse(&spec);
                let mut accepted = false;
                if let Some(c) = self.clients.get_mut(&sender_id) {
                    if c.matches.len() < MAX_MATCHES_PER_CLIENT {
                        c.matches.push(rule);
                        accepted = true;
                    }
                }
                if !no_reply {
                    // A rule that was dropped must not be acknowledged: the
                    // client would wait forever for signals it never subscribed to.
                    if accepted {
                        self.send_reply(sender_id, unique, msg.serial, "", Vec::new());
                    } else {
                        self.send_error(
                            sender_id,
                            unique,
                            msg.serial,
                            "org.freedesktop.DBus.Error.LimitsExceeded",
                            "too many match rules on this connection",
                        );
                    }
                }
            }
            "RemoveMatch" => {
                let Ok(spec) = reader.read_str() else {
                    if !no_reply {
                        self.send_error(sender_id, unique, msg.serial, "org.freedesktop.DBus.Error.InvalidArgs", "bad args");
                    }
                    return true;
                };
                let rule = MatchRule::parse(&spec);
                let removed = if let Some(c) = self.clients.get_mut(&sender_id) {
                    if let Some(idx) = c.matches.iter().position(|r| *r == rule) {
                        c.matches.remove(idx);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !no_reply {
                    if removed {
                        self.send_reply(sender_id, unique, msg.serial, "", Vec::new());
                    } else {
                        self.send_error(
                            sender_id,
                            unique,
                            msg.serial,
                            "org.freedesktop.DBus.Error.MatchRuleNotFound",
                            "match rule not found",
                        );
                    }
                }
            }
            "StartServiceByName" => {
                if !no_reply {
                    let name = reader.read_str().unwrap_or_default();
                    self.send_error(
                        sender_id,
                        unique,
                        msg.serial,
                        "org.freedesktop.DBus.Error.ServiceUnknown",
                        &format!("The name {name} was not provided by any .service files"),
                    );
                }
            }
            "UpdateActivationEnvironment" => {
                if !no_reply {
                    let _ = creds;
                    self.send_reply(sender_id, unique, msg.serial, "", Vec::new());
                }
            }
            _ => {
                if !no_reply {
                    self.send_error(
                        sender_id,
                        unique,
                        msg.serial,
                        "org.freedesktop.DBus.Error.UnknownMethod",
                        &format!("Method \"{member}\" is not implemented"),
                    );
                }
            }
        }
        true
    }

    fn send_reply(
        &mut self,
        target_id: u64,
        destination: &str,
        reply_serial: u32,
        signature: &str,
        body: Vec<u8>,
    ) {
        let serial = self.alloc_serial();
        let reply = DbusMessage {
            endian: b'l',
            msg_type: MSG_METHOD_RETURN,
            flags: FLAG_NO_REPLY_EXPECTED,
            serial,
            path: None,
            interface: None,
            member: None,
            error_name: None,
            reply_serial: Some(reply_serial),
            destination: Some(destination.to_owned()),
            sender: Some(String::from("org.freedesktop.DBus")),
            signature: signature.to_owned(),
            body,
        };
        if let Some(c) = self.clients.get_mut(&target_id) {
            let _ = c.queue_message(&reply);
        }
    }

    fn send_signal(
        &mut self,
        target_id: u64,
        destination: &str,
        member: &str,
        signature: &str,
        body: Vec<u8>,
    ) {
        let serial = self.alloc_serial();
        let sig = DbusMessage {
            endian: b'l',
            msg_type: MSG_SIGNAL,
            flags: FLAG_NO_REPLY_EXPECTED,
            serial,
            path: Some(String::from("/org/freedesktop/DBus")),
            interface: Some(String::from("org.freedesktop.DBus")),
            member: Some(member.to_owned()),
            error_name: None,
            reply_serial: None,
            destination: Some(destination.to_owned()),
            sender: Some(String::from("org.freedesktop.DBus")),
            signature: signature.to_owned(),
            body,
        };
        if let Some(c) = self.clients.get_mut(&target_id) {
            let _ = c.queue_message(&sig);
        }
    }

    fn send_error(
        &mut self,
        target_id: u64,
        destination: &str,
        reply_serial: u32,
        error_name: &str,
        message: &str,
    ) {
        let serial = self.alloc_serial();
        let mut body = Vec::new();
        encode_string(&mut body, message);
        let err = DbusMessage {
            endian: b'l',
            msg_type: MSG_ERROR,
            flags: FLAG_NO_REPLY_EXPECTED,
            serial,
            path: None,
            interface: None,
            member: None,
            error_name: Some(error_name.to_owned()),
            reply_serial: Some(reply_serial),
            destination: Some(destination.to_owned()),
            sender: Some(String::from("org.freedesktop.DBus")),
            signature: String::from("s"),
            body,
        };
        if let Some(c) = self.clients.get_mut(&target_id) {
            let _ = c.queue_message(&err);
        }
    }
}

fn verify_external_hex(hex_str: &str, expected_uid: u32) -> bool {
    if hex_str.is_empty() || hex_str.len() % 2 != 0 {
        return false;
    }
    let mut bytes = Vec::with_capacity(hex_str.len() / 2);
    let raw = hex_str.as_bytes();
    for pair in raw.chunks_exact(2) {
        let Ok(s) = std::str::from_utf8(pair) else {
            return false;
        };
        let Ok(byte) = u8::from_str_radix(s, 16) else {
            return false;
        };
        bytes.push(byte);
    }
    let Ok(uid_text) = std::str::from_utf8(&bytes) else {
        return false;
    };
    uid_text.parse::<u32>() == Ok(expected_uid)
}

struct ListenerEntry {
    bus: BusKind,
    listener: UnixListener,
}

struct BrokerRegistry {
    listeners: Vec<ListenerEntry>,
    wake_write_fd: RawFd,
}

static REGISTRY: OnceLock<Mutex<BrokerRegistry>> = OnceLock::new();

fn bind_unix_socket(
    path: &str,
    mode: u32,
    owner: Option<(u32, u32)>,
) -> std::io::Result<UnixListener> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let c_path = CString::new(path).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let unlink_rc = unsafe { libc::unlink(c_path.as_ptr()) };
    if unlink_rc < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ENOENT) {
            unsafe { libc::close(fd) };
            return Err(err);
        }
    }
    let path_bytes = c_path.as_bytes_with_nul();
    let mut addr = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if path_bytes.len() > addr.sun_path.len() {
        unsafe { libc::close(fd) };
        return Err(std::io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    for (idx, byte) in path_bytes.iter().enumerate() {
        addr.sun_path[idx] = *byte as libc::c_char;
    }
    let bind_rc = unsafe {
        libc::bind(
            fd,
            (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
            size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if bind_rc < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }
    if let Some((uid, gid)) = owner {
        if let Err(err) = chown(path, Some(uid), Some(gid)) {
            unsafe {
                libc::close(fd);
                libc::unlink(c_path.as_ptr());
            }
            return Err(err);
        }
    }
    if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        unsafe {
            libc::close(fd);
            libc::unlink(c_path.as_ptr());
        }
        return Err(err);
    }
    if unsafe { libc::listen(fd, 32) } < 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            libc::close(fd);
            libc::unlink(c_path.as_ptr());
        }
        return Err(err);
    }
    Ok(unsafe { UnixListener::from_raw_fd(fd) })
}

fn accept_dbus_client(listener: &UnixListener) -> std::io::Result<(UnixStream, PeerCreds)> {
    let fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let mut ucred = unsafe { std::mem::zeroed::<libc::ucred>() };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut ucred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc < 0 || len as usize != size_of::<libc::ucred>() || ucred.pid <= 0 {
        return Err(std::io::Error::from_raw_os_error(libc::EPROTO));
    }
    Ok((
        stream,
        PeerCreds {
            pid: ucred.pid as u32,
            uid: ucred.uid,
        },
    ))
}

/// A per-user session bus only serves its owner (and root); the kernel socket
/// mode already enforces this, so this is defence in depth against a mis-moded
/// socket.
fn peer_allowed(bus: BusKind, creds: PeerCreds) -> bool {
    match bus {
        BusKind::System => true,
        BusKind::Session(uid) => creds.uid == uid || creds.uid == 0,
    }
}

fn wake_broker(fd: RawFd) {
    let byte = [1_u8; 1];
    let _ = unsafe { libc::write(fd, byte.as_ptr().cast(), 1) };
}

fn drain_wake(fd: RawFd) {
    let mut buf = [0_u8; 64];
    loop {
        let rc = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if rc <= 0 {
            break;
        }
    }
}

fn flush_client_writes(client: &mut BusClient) -> bool {
    while !client.out_buf.is_empty() {
        let (head, _) = client.out_buf.as_slices();
        match client.stream.write(head) {
            Ok(0) => return false,
            Ok(n) => {
                client.out_buf.drain(..n);
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => return true,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    true
}

fn run_broker_loop(wake_read: OwnedFd, registry: &'static Mutex<BrokerRegistry>) {
    let mut state = BusState::new();
    let mut pollfds: Vec<libc::pollfd> = Vec::new();
    let mut tags: Vec<(bool, u64)> = Vec::new(); // (is_listener, listener_idx or client_id)
    let mut accepted = Vec::new();
    let mut to_drop = Vec::new();

    loop {
        pollfds.clear();
        tags.clear();
        pollfds.push(libc::pollfd {
            fd: wake_read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        tags.push((true, u64::MAX));

        {
            let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
            for (idx, entry) in reg.listeners.iter().enumerate() {
                pollfds.push(libc::pollfd {
                    fd: entry.listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                tags.push((true, idx as u64));
            }
        }
        for client in state.clients.values() {
            let mut events = libc::POLLIN;
            if !client.out_buf.is_empty() {
                events |= libc::POLLOUT;
            }
            pollfds.push(libc::pollfd {
                fd: client.stream.as_raw_fd(),
                events,
                revents: 0,
            });
            tags.push((false, client.id));
        }

        let rc = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }

        for (idx, pfd) in pollfds.iter().enumerate() {
            if pfd.revents == 0 {
                continue;
            }
            let (is_listener, id) = tags[idx];
            if is_listener && id == u64::MAX {
                drain_wake(wake_read.as_raw_fd());
                continue;
            }
            if is_listener {
                {
                    let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(entry) = reg.listeners.get(id as usize) {
                        for _ in 0..16 {
                            match accept_dbus_client(&entry.listener) {
                                Ok((stream, creds)) => accepted.push((entry.bus, stream, creds)),
                                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                                Err(_) => break,
                            }
                        }
                    }
                }
                for (bus, stream, creds) in accepted.drain(..) {
                    if state.clients.len() >= MAX_BUS_CLIENTS || !peer_allowed(bus, creds) {
                        continue;
                    }
                    let cid = state.next_client_id;
                    state.next_client_id += 1;
                    state.clients.insert(
                        cid,
                        BusClient {
                            id: cid,
                            bus,
                            stream,
                            creds,
                            auth: ClientAuthState::AwaitingNul,
                            unique_name: None,
                            in_buf: Vec::new(),
                            out_buf: VecDeque::new(),
                            matches: Vec::new(),
                            overflowed: false,
                        },
                    );
                }
                continue;
            }

            if (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0
                && (pfd.revents & libc::POLLIN) == 0
            {
                state.disconnect_client(id);
                continue;
            }
            if (pfd.revents & libc::POLLIN) != 0 && !state.process_client_read(id) {
                state.disconnect_client(id);
                continue;
            }
            if (pfd.revents & libc::POLLOUT) != 0 {
                let ok = state
                    .clients
                    .get_mut(&id)
                    .is_some_and(flush_client_writes);
                if !ok {
                    state.disconnect_client(id);
                }
            }
        }
        // Flush freshly queued replies immediately without waiting for another poll turn.
        for (&cid, client) in &mut state.clients {
            if client.overflowed || (!client.out_buf.is_empty() && !flush_client_writes(client)) {
                to_drop.push(cid);
            }
        }
        for cid in to_drop.drain(..) {
            state.disconnect_client(cid);
        }
    }
}

/// Start the Ring3 D-Bus daemon thread and bind the system bus + default desktop session bus.
pub(crate) fn start() {
    let mut fds = [0_i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } < 0 {
        super::debug_line("runtimed: dbus wake pipe failed");
        return;
    }
    let wake_read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let wake_write = fds[1];
    let mut listeners = Vec::new();

    match bind_unix_socket(SYSTEM_BUS_SOCKET_PATH, 0o666, None) {
        Ok(listener) => {
            listeners.push(ListenerEntry {
                bus: BusKind::System,
                listener,
            });
            super::debug_line("runtimed: dbus system bus ready");
        }
        Err(err) => {
            super::debug_line(&format!("runtimed: dbus system bus bind failed: {err}"));
        }
    }

    let (uid, gid) = launch_credentials(false);
    let runtime_dir = RuntimeDir(uid).to_string();
    if let Err(err) = std::fs::create_dir_all("/run/user") {
        super::debug_line(&format!("runtimed: /run/user create failed: {err}"));
    } else if super::launch_profile::ensure_owned_private_dir(&runtime_dir, uid, gid).is_ok() {
        let session_sock = format!("{runtime_dir}/bus");
        match bind_unix_socket(&session_sock, 0o600, Some((uid, gid))) {
            Ok(listener) => {
                listeners.push(ListenerEntry {
                    bus: BusKind::Session(uid),
                    listener,
                });
                super::debug_line(&format!("runtimed: dbus session bus ready path={session_sock}"));
            }
            Err(err) => {
                super::debug_line(&format!(
                    "runtimed: dbus session bus bind failed path={session_sock}: {err}"
                ));
            }
        }
    }

    let registry = REGISTRY.get_or_init(|| {
        Mutex::new(BrokerRegistry {
            listeners,
            wake_write_fd: wake_write,
        })
    });
    if let Err(err) = std::thread::Builder::new()
        .name(String::from("runtimed-dbus"))
        .spawn(move || run_broker_loop(wake_read, registry))
    {
        super::debug_line(&format!("runtimed: dbus thread failed: {err}"));
    }
}

/// Ensure a session bus socket `{runtime_dir}/bus` exists for `(uid, gid)`.
pub(crate) fn ensure_session_bus(runtime_dir: &str, uid: u32, gid: u32) -> std::io::Result<()> {
    let Some(registry) = REGISTRY.get() else {
        return Ok(());
    };
    let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
    let bus = BusKind::Session(uid);
    if reg.listeners.iter().any(|entry| entry.bus == bus) {
        return Ok(());
    }
    let session_sock = format!("{runtime_dir}/bus");
    let listener = bind_unix_socket(&session_sock, 0o600, Some((uid, gid)))?;
    reg.listeners.push(ListenerEntry { bus, listener });
    let wake_fd = reg.wake_write_fd;
    drop(reg);
    wake_broker(wake_fd);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_test_bus(path: &Path) -> RawFd {
        let listener = bind_unix_socket(path.to_str().unwrap(), 0o600, None).unwrap();
        let mut fds = [0_i32; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) }, 0);
        let wake_read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let registry: &'static Mutex<BrokerRegistry> = Box::leak(Box::new(Mutex::new(BrokerRegistry {
            listeners: vec![ListenerEntry {
                bus: BusKind::System,
                listener,
            }],
            wake_write_fd: fds[1],
        })));
        std::thread::spawn(move || run_broker_loop(wake_read, registry));
        fds[1]
    }

    fn sasl_handshake(stream: &mut UnixStream) {
        let uid_str = unsafe { libc::geteuid() }.to_string();
        let hex_uid: String = uid_str.bytes().map(|b| format!("{b:02x}")).collect();
        stream.write_all(format!("\0AUTH EXTERNAL {hex_uid}\r\n").as_bytes()).unwrap();
        let mut line = [0_u8; 256];
        let n = stream.read(&mut line).unwrap();
        let text = std::str::from_utf8(&line[..n]).unwrap();
        assert!(text.starts_with("OK "), "{text:?}");
        stream.write_all(b"NEGOTIATE_UNIX_FD\r\n").unwrap();
        let n = stream.read(&mut line).unwrap();
        assert!(std::str::from_utf8(&line[..n]).unwrap().starts_with("ERROR "));
        stream.write_all(b"BEGIN\r\n").unwrap();
    }

    fn read_dbus_msg(stream: &mut UnixStream, buf: &mut Vec<u8>) -> DbusMessage {
        let mut tmp = [0_u8; 4096];
        loop {
            if let Some((msg, consumed)) = DbusMessage::decode(buf).unwrap() {
                buf.drain(..consumed);
                return msg;
            }
            let n = stream.read(&mut tmp).unwrap();
            assert!(n > 0);
            buf.extend_from_slice(&tmp[..n]);
        }
    }

    #[test]
    fn sasl_external_and_dbus_driver_round_trip() {
        let dir = std::env::temp_dir().join(format!("rustos-dbus-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("system_bus_socket");
        let _wake = start_test_bus(&sock);

        let mut c1 = UnixStream::connect(&sock).unwrap();
        c1.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        sasl_handshake(&mut c1);

        let hello = DbusMessage {
            endian: b'l',
            msg_type: MSG_METHOD_CALL,
            flags: 0,
            serial: 1,
            path: Some(String::from("/org/freedesktop/DBus")),
            interface: Some(String::from("org.freedesktop.DBus")),
            member: Some(String::from("Hello")),
            error_name: None,
            reply_serial: None,
            destination: Some(String::from("org.freedesktop.DBus")),
            sender: None,
            signature: String::new(),
            body: Vec::new(),
        };
        c1.write_all(&hello.encode()).unwrap();

        let mut buf1 = Vec::new();
        let reply = read_dbus_msg(&mut c1, &mut buf1);
        assert_eq!(reply.msg_type, MSG_METHOD_RETURN);
        assert_eq!(reply.reply_serial, Some(1));
        let unique1 = BodyReader::new(reply.endian, &reply.body).read_str().unwrap();
        assert!(unique1.starts_with(":1."), "{unique1}");
        let acq = read_dbus_msg(&mut c1, &mut buf1);
        assert_eq!(acq.msg_type, MSG_SIGNAL);
        assert_eq!(acq.member.as_deref(), Some("NameAcquired"));

        // RequestName + NameHasOwner + GetNameOwner + ListNames + AddMatch
        let mut req_body = Vec::new();
        encode_string(&mut req_body, "org.example.Test");
        encode_u32(&mut req_body, 0);
        c1.write_all(
            &DbusMessage {
                endian: b'l',
                msg_type: MSG_METHOD_CALL,
                flags: 0,
                serial: 2,
                path: Some(String::from("/org/freedesktop/DBus")),
                interface: Some(String::from("org.freedesktop.DBus")),
                member: Some(String::from("RequestName")),
                error_name: None,
                reply_serial: None,
                destination: Some(String::from("org.freedesktop.DBus")),
                sender: None,
                signature: String::from("su"),
                body: req_body,
            }
            .encode(),
        )
        .unwrap();
        let acq2 = read_dbus_msg(&mut c1, &mut buf1);
        assert_eq!(acq2.member.as_deref(), Some("NameAcquired"));
        let r2 = read_dbus_msg(&mut c1, &mut buf1);
        assert_eq!(BodyReader::new(r2.endian, &r2.body).read_u32().unwrap(), 1);

        let mut has_body = Vec::new();
        encode_string(&mut has_body, "org.example.Test");
        c1.write_all(
            &DbusMessage {
                endian: b'l',
                msg_type: MSG_METHOD_CALL,
                flags: 0,
                serial: 3,
                path: Some(String::from("/org/freedesktop/DBus")),
                interface: Some(String::from("org.freedesktop.DBus")),
                member: Some(String::from("NameHasOwner")),
                error_name: None,
                reply_serial: None,
                destination: Some(String::from("org.freedesktop.DBus")),
                sender: None,
                signature: String::from("s"),
                body: has_body,
            }
            .encode(),
        )
        .unwrap();
        let r3 = read_dbus_msg(&mut c1, &mut buf1);
        assert_eq!(BodyReader::new(r3.endian, &r3.body).read_u32().unwrap(), 1);

        let mut missing_body = Vec::new();
        encode_string(&mut missing_body, "org.freedesktop.UPower");
        c1.write_all(
            &DbusMessage {
                endian: b'l',
                msg_type: MSG_METHOD_CALL,
                flags: 0,
                serial: 4,
                path: Some(String::from("/org/freedesktop/DBus")),
                interface: Some(String::from("org.freedesktop.DBus")),
                member: Some(String::from("GetNameOwner")),
                error_name: None,
                reply_serial: None,
                destination: Some(String::from("org.freedesktop.DBus")),
                sender: None,
                signature: String::from("s"),
                body: missing_body,
            }
            .encode(),
        )
        .unwrap();
        let r4 = read_dbus_msg(&mut c1, &mut buf1);
        assert_eq!(r4.msg_type, MSG_ERROR);
        assert_eq!(r4.error_name.as_deref(), Some("org.freedesktop.DBus.Error.NameHasNoOwner"));

        // Second client routes a peer call to org.example.Test
        let mut c2 = UnixStream::connect(&sock).unwrap();
        c2.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        sasl_handshake(&mut c2);
        c2.write_all(&hello.encode()).unwrap();
        let mut buf2 = Vec::new();
        let _h2 = read_dbus_msg(&mut c2, &mut buf2);
        let _a2 = read_dbus_msg(&mut c2, &mut buf2);

        c2.write_all(
            &DbusMessage {
                endian: b'l',
                msg_type: MSG_METHOD_CALL,
                flags: 0,
                serial: 10,
                path: Some(String::from("/test")),
                interface: Some(String::from("org.example.Iface")),
                member: Some(String::from("Ping")),
                error_name: None,
                reply_serial: None,
                destination: Some(String::from("org.example.Test")),
                sender: None,
                signature: String::new(),
                body: Vec::new(),
            }
            .encode(),
        )
        .unwrap();
        let forwarded = read_dbus_msg(&mut c1, &mut buf1);
        assert_eq!(forwarded.member.as_deref(), Some("Ping"));
        assert!(forwarded.sender.as_deref().is_some_and(|s| s.starts_with(":1.")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_bus_only_serves_owner_and_root() {
        let creds = |uid| PeerCreds { pid: 7, uid };
        assert!(peer_allowed(BusKind::System, creds(1234)));
        assert!(peer_allowed(BusKind::Session(1000), creds(1000)));
        assert!(peer_allowed(BusKind::Session(1000), creds(0)));
        assert!(!peer_allowed(BusKind::Session(1000), creds(1001)));
    }

    #[test]
    fn oversized_sasl_line_is_rejected_and_overflow_is_flagged() {
        let (a, _b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let mut client = BusClient {
            id: 1,
            bus: BusKind::System,
            stream: a,
            creds: PeerCreds { pid: 1, uid: 0 },
            auth: ClientAuthState::AwaitingNul,
            unique_name: None,
            in_buf: Vec::new(),
            out_buf: VecDeque::new(),
            matches: Vec::new(),
            overflowed: false,
        };
        assert!(client.queue_bytes(b"ok"));
        assert!(!client.overflowed);
        let big = vec![0_u8; MAX_CLIENT_OUT_BYTES];
        assert!(!client.queue_bytes(&big));
        assert!(client.overflowed);

        let mut state = BusState::new();
        client.in_buf.push(0);
        client.in_buf.extend_from_slice(&vec![b'A'; MAX_SASL_LINE_BYTES + 8]);
        client.in_buf.extend_from_slice(b"\r\n");
        state.clients.insert(1, client);
        assert!(!state.advance_sasl(1));
    }
    fn test_client(bus: BusKind, unique: &str) -> (BusClient, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        (
            BusClient {
                id: 1,
                bus,
                stream: a,
                creds: PeerCreds { pid: 1, uid: 0 },
                auth: ClientAuthState::Authenticated,
                unique_name: Some(unique.to_owned()),
                in_buf: Vec::new(),
                out_buf: VecDeque::new(),
                matches: Vec::new(),
                overflowed: false,
            },
            b,
        )
    }

    fn driver_call(member: &str, serial: u32, arg: &str) -> DbusMessage {
        let mut body = Vec::new();
        encode_string(&mut body, arg);
        DbusMessage {
            endian: b'l',
            msg_type: MSG_METHOD_CALL,
            flags: 0,
            serial,
            path: Some(String::from("/org/freedesktop/DBus")),
            interface: Some(String::from("org.freedesktop.DBus")),
            member: Some(member.to_owned()),
            error_name: None,
            reply_serial: None,
            destination: Some(String::from("org.freedesktop.DBus")),
            sender: None,
            signature: String::from("s"),
            body,
        }
    }

    fn last_queued(client: &mut BusClient) -> DbusMessage {
        let bytes: Vec<u8> = client.out_buf.iter().copied().collect();
        let mut pos = 0;
        let mut last = None;
        while let Some((msg, used)) = DbusMessage::decode(&bytes[pos..]).unwrap() {
            pos += used;
            last = Some(msg);
        }
        last.expect("a queued reply")
    }

    #[test]
    fn coalesced_messages_dispatch_in_order_and_preserve_one_partial_suffix() {
        let mut state = BusState::new();
        let (mut client, _peer) = test_client(BusKind::System, ":1.1");
        let first = driver_call("Hello", 71, "").encode();
        let second = driver_call("Hello", 72, "").encode();
        let third = driver_call("Hello", 73, "").encode();
        client.in_buf.extend_from_slice(&first);
        client.in_buf.extend_from_slice(&second);
        client.in_buf.extend_from_slice(&third[..13]);
        state.clients.insert(1, client);
        assert!(state.advance_messages(1));
        let client = state.clients.get_mut(&1).unwrap();
        assert_eq!(client.in_buf, third[..13]);
        let replies: Vec<u8> = client.out_buf.drain(..).collect();
        let (reply, used) = DbusMessage::decode(&replies).unwrap().unwrap();
        assert_eq!(reply.reply_serial, Some(71));
        let (reply, used_second) = DbusMessage::decode(&replies[used..]).unwrap().unwrap();
        assert_eq!(reply.reply_serial, Some(72));
        assert_eq!(used + used_second, replies.len());
        client.in_buf.extend_from_slice(&third[13..]);
        assert!(state.advance_messages(1));
        let client = state.clients.get_mut(&1).unwrap();
        assert!(client.in_buf.is_empty());
        assert_eq!(last_queued(client).reply_serial, Some(73));
    }

    #[test]
    fn dbus_strings_require_one_terminating_nul_and_no_embedded_nul() {
        for endian in [b'l', b'B'] {
            let mut bytes = Vec::new();
            write_u32(endian, &mut bytes, 3);
            bytes.extend_from_slice(b"abc\0");
            let mut pos = 0;
            assert_eq!(read_dbus_str(endian, &bytes, &mut pos), Ok("abc"));
            assert_eq!(pos, bytes.len());
            bytes[7] = b'x';
            assert!(read_dbus_string(endian, &bytes, &mut 0).is_err());
            bytes[7] = 0;
            bytes[5] = 0;
            assert!(read_dbus_string(endian, &bytes, &mut 0).is_err());
            bytes.truncate(4);
            assert!(read_dbus_str(endian, &bytes, &mut 0).is_err());
        }
    }

    #[test]
    fn rejected_match_and_name_registrations_are_errors_not_acknowledged() {
        let mut state = BusState::new();
        let (client, _peer) = test_client(BusKind::System, ":1.1");
        state.clients.insert(1, client);
        let creds = PeerCreds { pid: 1, uid: 0 };

        for i in 0..MAX_MATCHES_PER_CLIENT {
            let call = driver_call("AddMatch", 2 + i as u32, &format!("type='signal',member='M{i}'"));
            assert!(state.handle_driver_call(1, BusKind::System, ":1.1", creds, &call));
            let reply = last_queued(state.clients.get_mut(&1).unwrap());
            assert_eq!(reply.msg_type, MSG_METHOD_RETURN);
            state.clients.get_mut(&1).unwrap().out_buf.clear();
        }
        let call = driver_call("AddMatch", 9_000, "type='signal',member='Overflow'");
        assert!(state.handle_driver_call(1, BusKind::System, ":1.1", creds, &call));
        let client = state.clients.get_mut(&1).unwrap();
        assert_eq!(client.matches.len(), MAX_MATCHES_PER_CLIENT);
        let reply = last_queued(client);
        assert_eq!(reply.msg_type, MSG_ERROR);
        assert_eq!(reply.reply_serial, Some(9_000));
        assert_eq!(
            reply.error_name.as_deref(),
            Some("org.freedesktop.DBus.Error.LimitsExceeded")
        );
        client.out_buf.clear();

        for i in 0..MAX_NAMES_PER_CLIENT {
            let mut call = driver_call("RequestName", 10_000 + i as u32, &format!("org.example.N{i}"));
            encode_u32(&mut call.body, 0);
            call.signature = String::from("su");
            assert!(state.handle_driver_call(1, BusKind::System, ":1.1", creds, &call));
            state.clients.get_mut(&1).unwrap().out_buf.clear();
        }
        let mut call = driver_call("RequestName", 20_000, "org.example.Overflow");
        encode_u32(&mut call.body, 0);
        call.signature = String::from("su");
        assert!(state.handle_driver_call(1, BusKind::System, ":1.1", creds, &call));
        assert!(!state
            .names
            .contains_key(&(BusState::bus_scope(BusKind::System), String::from("org.example.Overflow"))));
        let reply = last_queued(state.clients.get_mut(&1).unwrap());
        assert_eq!(reply.msg_type, MSG_ERROR);
        assert_eq!(reply.reply_serial, Some(20_000));
    }
}
