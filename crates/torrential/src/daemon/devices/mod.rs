//! Phones and TVs on the home network. This Mac lets a device in with a short code it shows, and
//! the device holds a token in a cookie from then on. A TV that plays a cast keeps no cookie, so it
//! fetches its one file through a grant in the URL instead. The HTTP guard asks this who sent each
//! request.

pub mod names;

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use torrential_core::{Device, DeviceList, Error, ErrorCode, PairingOffer, TorrentId};

use super::store::sql_error;
use names::Names;

/// How long a pairing code works, in seconds.
const OFFER_SECS: i64 = 5 * 60;
/// Wrong codes after which an offer stops working, so guessing six digits has a 5 in a million
/// chance.
const WRONG_CODES: u32 = 5;
/// Pairing tries one address may make in a minute.
const TRIES_PER_MINUTE: u32 = 10;
/// Addresses whose tries are counted at once. Past this, older minutes are forgotten.
const TRIES_KEPT: usize = 1024;
/// How often a device's last request is written to the database, in seconds.
const SEEN_EVERY: i64 = 60;
/// The cookie that holds a device's token.
pub const COOKIE: &str = "torrential_device";
/// Where a cast grant's file is served.
pub const CAST: &str = "/cast/";

/// Who sent a request, which decides what it may reach.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Asker {
    /// This Mac, over loopback: everything.
    Local,
    /// A paired phone or TV, by device ID: everything but adding and removing devices.
    Device(String),
    /// A TV playing a cast: the one file its grant names.
    Cast(Granted),
    /// Anyone else on the network: the pairing page and nothing more. `removed` when it holds the
    /// cookie of a device that is no longer paired.
    Stranger { removed: bool },
}

/// The file a cast grant serves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Granted {
    pub id: TorrentId,
    pub index: u32,
    key: Key,
}

/// Where the engine listens.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reach {
    /// 127.0.0.1 only, while nothing is paired.
    Loopback,
    /// Every address, while a device is paired, a code is open, or a TV plays a cast.
    Network,
}

/// 32 random bytes, which a cookie or a URL carries as 64 hex digits.
struct Token([u8; 32]);

impl Token {
    fn new() -> Result<Self, Error> {
        let mut bytes = [0; 32];
        random(&mut bytes)?;
        Ok(Self(bytes))
    }

    fn parse(hex: &str) -> Option<Self> {
        let digits = hex.as_bytes();
        if digits.len() != 64 {
            return None;
        }
        let mut bytes = [0; 32];
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(Self(bytes))
    }

    fn hex(&self) -> String {
        hex(&self.0)
    }

    fn key(&self) -> Key {
        Key(Sha256::digest(self.0).into())
    }
}

/// The SHA-256 of a token: all the engine keeps of it. Looking a token up by its hash means the
/// time a lookup takes says nothing about the tokens the engine holds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Key([u8; 32]);

fn random(bytes: &mut [u8]) -> Result<(), Error> {
    getrandom::fill(bytes).map_err(|e| Error::internal(format!("cannot read random bytes: {e}")))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Why a pairing try did not let the device in.
#[derive(Debug)]
pub enum Refused {
    TooManyTries,
    NoOffer,
    WrongCode,
    /// The code stopped working after too many wrong ones.
    Burned,
    Failed(Error),
}

impl Refused {
    pub fn status(&self) -> &'static str {
        match self {
            Self::TooManyTries => "429 Too Many Requests",
            Self::NoOffer | Self::Burned => "410 Gone",
            Self::WrongCode => "400 Bad Request",
            Self::Failed(_) => "500 Internal Server Error",
        }
    }

    pub fn error(self) -> Error {
        let message = match self {
            Self::TooManyTries => "too many tries from this device, so wait a minute and enter the code again",
            Self::NoOffer => {
                "no code is open, so choose Add a device in torrential on the Mac and enter the code it shows"
            }
            Self::WrongCode => "that is not the code on the Mac, so check it and enter it again",
            Self::Burned => {
                "that code stopped working after too many wrong tries, so make a new one with Add a device on the Mac"
            }
            Self::Failed(error) => return error,
        };
        Error::new(ErrorCode::NotPaired, message)
    }
}

struct Paired {
    device: Device,
    /// The `seen_at` the database has.
    written: i64,
}

struct Offer {
    code: String,
    expires_at: i64,
    wrong: u32,
}

struct Grant {
    id: TorrentId,
    index: u32,
    path: String,
}

struct State {
    paired: HashMap<Key, Paired>,
    offer: Option<Offer>,
    grants: HashMap<Key, Grant>,
    /// Pairing tries by address: the minute, and the tries in it.
    tries: HashMap<IpAddr, (i64, u32)>,
    names: Names,
}

impl State {
    fn reach(&self, now: i64) -> Reach {
        let open = self.offer.as_ref().is_some_and(|offer| offer.expires_at > now);
        if open || !self.paired.is_empty() || !self.grants.is_empty() { Reach::Network } else { Reach::Loopback }
    }

    fn device(&self, id: &str) -> Option<&Device> {
        self.paired.values().map(|paired| &paired.device).find(|device| device.id == id)
    }

    fn offer(&self, port: u16, now: i64) -> Option<PairingOffer> {
        let offer = self.offer.as_ref().filter(|offer| offer.expires_at > now)?;
        let lan_address = self.names.lan.first().map(|ip| format!("{ip}:{port}"));
        let address = self.names.local.as_ref().map(|name| format!("{name}:{port}")).or(lan_address.clone())?;
        Some(PairingOffer {
            code: offer.code.clone(),
            url: format!("http://{address}/pair#{}", offer.code),
            address,
            lan_address,
            expires_at: offer.expires_at,
        })
    }

    fn list(&self, port: u16, now: i64) -> DeviceList {
        let mut devices: Vec<Device> = self.paired.values().map(|paired| paired.device.clone()).collect();
        devices.sort_by(|a, b| (a.added_at, &a.id).cmp(&(b.added_at, &b.id)));
        DeviceList { devices, offer: self.offer(port, now) }
    }

    /// Counts a pairing try from `from`. False once it has made too many this minute.
    fn count_try(&mut self, from: IpAddr, now: i64) -> bool {
        let minute = now.div_euclid(60);
        if self.tries.len() >= TRIES_KEPT {
            self.tries.retain(|_, (counted, _)| *counted == minute);
        }
        let tries = self.tries.entry(from).or_insert((minute, 0));
        if tries.0 != minute {
            *tries = (minute, 0);
        }
        tries.1 += 1;
        tries.1 <= TRIES_PER_MINUTE
    }
}

/// The paired devices, the open pairing offer, and the live cast grants. Paired devices are kept
/// in the `devices` table, on a connection of their own to the engine's database, which
/// `Store::open` has migrated.
pub struct Devices {
    conn: Mutex<Connection>,
    path: PathBuf,
    state: Mutex<State>,
    reach: watch::Sender<Reach>,
    /// Counts removed devices and dropped grants, so a connection can end once its access does.
    revocations: watch::Sender<u64>,
}

impl Devices {
    pub fn open(path: &Path, now: i64) -> Result<Self, Error> {
        let conn = Connection::open(path).map_err(|e| sql_error(path, e))?;
        conn.execute_batch("PRAGMA synchronous = FULL;").map_err(|e| sql_error(path, e))?;
        conn.busy_timeout(Duration::from_secs(1)).map_err(|e| sql_error(path, e))?;
        let paired = {
            let mut stmt = conn
                .prepare("SELECT id, token_hash, name, added_at, seen_at FROM devices")
                .map_err(|e| sql_error(path, e))?;
            let rows = stmt
                .query_map([], |r| {
                    let device = Device { id: r.get(0)?, name: r.get(2)?, added_at: r.get(3)?, seen_at: r.get(4)? };
                    Ok((r.get::<_, Vec<u8>>(1)?, device))
                })
                .map_err(|e| sql_error(path, e))?;
            let mut paired = HashMap::new();
            for row in rows {
                let (hash, device) = row.map_err(|e| sql_error(path, e))?;
                let key = Key(hash.try_into().map_err(|_| {
                    Error::new(ErrorCode::StateCorrupt, format!("{} has an unreadable device token", path.display()))
                })?);
                paired.insert(key, Paired { written: device.seen_at, device });
            }
            paired
        };
        let state = State { paired, offer: None, grants: HashMap::new(), tries: HashMap::new(), names: Names::read() };
        let reach = watch::Sender::new(state.reach(now));
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
            state: Mutex::new(state),
            reach,
            revocations: watch::Sender::new(0),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn fail(&self, e: rusqlite::Error) -> Error {
        sql_error(&self.path, e)
    }

    fn publish(&self, state: &State, now: i64) {
        let reach = state.reach(now);
        self.reach.send_if_modified(|current| std::mem::replace(current, reach) != reach);
    }

    fn revoke(&self) {
        self.revocations.send_modify(|count| *count += 1);
    }

    /// Where the engine should listen, now and as it changes.
    pub fn reach(&self) -> watch::Receiver<Reach> {
        self.reach.subscribe()
    }

    /// This Mac's .local name, as phones and TVs on the home network open it.
    pub fn local_name(&self) -> Option<String> {
        self.state().names.local.clone()
    }

    /// The paired devices and the open offer.
    pub fn list(&self, port: u16, now: i64) -> DeviceList {
        self.state().list(port, now)
    }

    /// The device with this ID, if it is paired.
    pub fn device(&self, id: &str) -> Option<Device> {
        self.state().device(id).cloned()
    }

    /// Opens a pairing offer in place of any open one, and closes it once it expires. This Mac's
    /// names are read again for it, since a new Wi-Fi network or a renamed Mac changes them.
    pub fn add(self: &Arc<Self>, port: u16, now: i64) -> Result<DeviceList, Error> {
        let names = Names::read();
        if names.local.is_none() && names.lan.is_empty() {
            return Err(Error::new(
                ErrorCode::NetworkUnavailable,
                "this Mac is on no network, so no phone or TV can reach it",
            )
            .with_hint("join the home Wi-Fi or network, then add the device again"));
        }
        let mut number = [0u8; 4];
        let code = loop {
            random(&mut number)?;
            // Drawing again above the last whole million keeps every code equally likely.
            let n = u32::from_be_bytes(number);
            if n < 4_294_000_000 {
                break format!("{:06}", n % 1_000_000);
            }
        };
        let list = {
            let mut state = self.state();
            state.names = names;
            state.offer = Some(Offer { code, expires_at: now + OFFER_SECS, wrong: 0 });
            self.publish(&state, now);
            state.list(port, now)
        };
        let devices = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(OFFER_SECS as u64)).await;
            if let Some(devices) = devices.upgrade() {
                devices.expire(super::owner::now());
            }
        });
        Ok(list)
    }

    /// Closes the offer if it has expired by `now`.
    pub fn expire(&self, now: i64) {
        let mut state = self.state();
        if state.offer.as_ref().is_some_and(|offer| offer.expires_at <= now) {
            state.offer = None;
        }
        self.publish(&state, now);
    }

    /// Locks a device out, ending the requests it has open.
    pub fn remove(&self, id: &str, port: u16, now: i64) -> Result<DeviceList, Error> {
        let mut state = self.state();
        let Some(key) = state.paired.iter().find(|(_, paired)| paired.device.id == id).map(|(key, _)| *key) else {
            return Err(Error::new(ErrorCode::NotFound, format!("no device has the ID {id}"))
                .with_hint("run torrential devices to list them"));
        };
        self.conn().execute("DELETE FROM devices WHERE id = ?1", [id]).map_err(|e| self.fail(e))?;
        state.paired.remove(&key);
        self.publish(&state, now);
        self.revoke();
        Ok(state.list(port, now))
    }

    /// Lets a device in when `code` is the open offer's, and returns the token for its cookie.
    pub fn pair(&self, code: &str, agent: Option<&str>, from: IpAddr, now: i64) -> Result<(Device, String), Refused> {
        let mut state = self.state();
        if !state.count_try(from, now) {
            return Err(Refused::TooManyTries);
        }
        let Some(offer) = state.offer.as_mut().filter(|offer| offer.expires_at > now) else {
            return Err(Refused::NoOffer);
        };
        let typed: String = code.chars().filter(char::is_ascii_digit).collect();
        if !same(&typed, &offer.code) {
            offer.wrong += 1;
            if offer.wrong < WRONG_CODES {
                return Err(Refused::WrongCode);
            }
            state.offer = None;
            self.publish(&state, now);
            return Err(Refused::Burned);
        }
        let token = Token::new().map_err(Refused::Failed)?;
        let mut id = [0u8; 4];
        let id = loop {
            random(&mut id).map_err(Refused::Failed)?;
            let id = hex(&id);
            if state.device(&id).is_none() {
                break id;
            }
        };
        let device = Device { id, name: unique_name(&state, device_name(agent)), added_at: now, seen_at: now };
        self.conn()
            .execute(
                "INSERT INTO devices (id, token_hash, name, added_at, seen_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                params![device.id, token.key().0.as_slice(), device.name, now],
            )
            .map_err(|e| Refused::Failed(self.fail(e)))?;
        state.offer = None;
        state.paired.insert(token.key(), Paired { device: device.clone(), written: now });
        self.publish(&state, now);
        Ok((device, token.hex()))
    }

    /// The paired device whose token a cookie holds, which counts as seen `now`.
    pub fn holder(&self, token: &str, now: i64) -> Option<Device> {
        let key = Token::parse(token)?.key();
        let (device, write) = {
            let mut state = self.state();
            let paired = state.paired.get_mut(&key)?;
            paired.device.seen_at = now;
            let write = now - paired.written >= SEEN_EVERY;
            if write {
                paired.written = now;
            }
            (paired.device.clone(), write)
        };
        if write {
            let _ = self.conn().execute("UPDATE devices SET seen_at = ?1 WHERE id = ?2", params![now, device.id]);
        }
        Some(device)
    }

    /// A grant that serves file `index` of download `id` to whoever has its URL, until it is
    /// dropped. `name` gives the URL a file name a TV can show.
    #[cfg_attr(not(test), expect(dead_code, reason = "the cast pull request mints grants"))]
    pub fn grant(self: &Arc<Self>, id: TorrentId, index: u32, name: &str) -> Result<CastGrant, Error> {
        let token = Token::new()?;
        let path = format!("{CAST}{}/{}", token.hex(), super::http::url_name(name));
        let key = token.key();
        let mut state = self.state();
        state.grants.insert(key, Grant { id, index, path: path.clone() });
        self.publish(&state, super::owner::now());
        Ok(CastGrant { devices: Arc::downgrade(self), key, path })
    }

    /// The file a live grant serves at exactly `path`.
    pub fn granted(&self, path: &str) -> Option<Granted> {
        let token = path.strip_prefix(CAST)?.split('/').next()?;
        let key = Token::parse(token)?.key();
        let state = self.state();
        let grant = state.grants.get(&key).filter(|grant| grant.path == path)?;
        Some(Granted { id: grant.id, index: grant.index, key })
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "the cast pull request mints grants"))]
    fn drop_grant(&self, key: Key) {
        let mut state = self.state();
        if state.grants.remove(&key).is_some() {
            self.publish(&state, super::owner::now());
            self.revoke();
        }
    }

    /// Returns once `asker` loses its access: its device removed, or its grant dropped. Never for
    /// this Mac or a stranger.
    pub async fn revoked(&self, asker: &Asker) {
        let holds = |state: &State| match asker {
            Asker::Device(id) => state.device(id).is_some(),
            Asker::Cast(granted) => state.grants.contains_key(&granted.key),
            Asker::Local | Asker::Stranger { .. } => true,
        };
        if matches!(asker, Asker::Local | Asker::Stranger { .. }) {
            return std::future::pending().await;
        }
        let mut revocations = self.revocations.subscribe();
        while holds(&self.state()) {
            if revocations.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Lets whoever has its URL fetch one file, until it is dropped. A TV plays a cast from one.
#[cfg_attr(not(test), expect(dead_code, reason = "the cast pull request mints grants"))]
pub struct CastGrant {
    devices: Weak<Devices>,
    key: Key,
    path: String,
}

impl CastGrant {
    /// The path the file is served at, such as `/cast/5f0c…/Sintel.mkv`, to put after the address
    /// the TV reaches this Mac by.
    #[cfg_attr(not(test), expect(dead_code, reason = "the cast pull request mints grants"))]
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for CastGrant {
    fn drop(&mut self) {
        if let Some(devices) = self.devices.upgrade() {
            devices.drop_grant(self.key);
        }
    }
}

/// Compares in the same time wherever the first difference is.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |differ, (x, y)| differ | (x ^ y)) == 0
}

/// What a browser's User-Agent says it runs on, the first match winning. TVs come first, since
/// their browsers also name Linux or Android.
const NAMES: [(&str, &str); 14] = [
    ("web0s", "LG TV"),
    ("webos", "LG TV"),
    ("tizen", "Samsung TV"),
    ("bravia", "Sony TV"),
    ("crkey", "Chromecast"),
    ("; aft", "Fire TV"),
    ("android tv", "Android TV"),
    ("smart-tv", "TV"),
    ("iphone", "iPhone"),
    ("ipad", "iPad"),
    ("android", "Android"),
    ("macintosh", "Mac"),
    ("windows", "Windows PC"),
    ("linux", "Linux computer"),
];

fn device_name(agent: Option<&str>) -> String {
    let agent = agent.unwrap_or_default().to_ascii_lowercase();
    NAMES.iter().find(|(mark, _)| agent.contains(mark)).map_or("Browser", |(_, name)| name).to_string()
}

/// `name`, or `name 2` and on when a paired device has it.
fn unique_name(state: &State, name: String) -> String {
    let taken = |candidate: &str| state.paired.values().any(|paired| paired.device.name == candidate);
    if !taken(&name) {
        return name;
    }
    (2..).map(|n| format!("{name} {n}")).find(|candidate| !taken(candidate)).expect("some number is free")
}

/// The modules of a QR code that holds `text`, one string of `1` and `0` for each row, without the
/// quiet zone around it.
pub fn qr(text: &str) -> Vec<String> {
    let Ok(code) = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium) else { return Vec::new() };
    (0..code.size())
        .map(|y| (0..code.size()).map(|x| if code.get_module(x, y) { '1' } else { '0' }).collect())
        .collect()
}

#[cfg(test)]
mod tests;
