//! The pairing page a stranger on the home network gets, and the API this Mac adds and removes
//! devices with.

use std::io;
use std::net::{IpAddr, Ipv4Addr};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use torrential_core::{Device, DeviceList, Error, ErrorCode, PairingOffer};

use super::super::devices::{Asker, COOKIE, qr};
use super::super::http::{Connection, Method, Peer, Request};
use super::super::owner::now;
use super::super::server::Ctx;
use super::{body, value};

/// The page that pairs a phone or TV.
pub const PAIR: &str = "/pair";
const PAGE: &str = include_str!("pair.html");
const LEAD: &str = "Enter the code that Add a device shows in torrential on the Mac.";
const REMOVED_LEAD: &str = "This device was removed, so enter a new code from Add a device in torrential on the Mac.";
const NOT_PAIRED: &str =
    "this device is not paired with the Mac, so open /pair and enter the code that Add a device shows";
const REMOVED: &str = "this device was removed on the Mac, so open /pair and enter a new code from Add a device";
/// Browsers keep a cookie at most 400 days.
const COOKIE_SECS: u32 = 400 * 86_400;
const PAGE_HEADERS: &str = "Cache-Control: no-store\r\n\
    Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
    connect-src 'self'; img-src data:; base-uri 'none'; form-action 'self'; frame-ancestors 'none'\r\n\
    Referrer-Policy: no-referrer\r\n";

/// The header that gives a device its token, which only requests to this Mac carry.
pub fn set_cookie(token: &str) -> String {
    format!("Set-Cookie: {COOKIE}={token}; Path=/; Max-Age={COOKIE_SECS}; HttpOnly; SameSite=Strict\r\n")
}

/// What a browser that is not paired may reach: the pairing page, and the request that pairs it.
pub async fn stranger(conn: &mut Connection<'_>, request: &Request, removed: bool) -> io::Result<bool> {
    match (request.method, request.path.as_str()) {
        (Method::Post, "/api/pair") => pair(conn, request).await,
        (_, path) if path.starts_with("/api/") => {
            let error = Error::new(ErrorCode::NotPaired, if removed { REMOVED } else { NOT_PAIRED });
            conn.json("401 Unauthorized", &json!({ "error": error }), request).await
        }
        (Method::Get | Method::Head, PAIR) => {
            let page = PAGE.replace("{{lead}}", if removed { REMOVED_LEAD } else { LEAD });
            conn.send("200 OK", "text/html; charset=utf-8", PAGE_HEADERS, page.as_bytes(), request).await
        }
        (Method::Get | Method::Head, _) => {
            let headers = format!("Location: {PAIR}\r\nCache-Control: no-store\r\n");
            conn.send("303 See Other", "text/plain; charset=utf-8", &headers, b"", request).await
        }
        (Method::Post, _) => conn.send("405 Method Not Allowed", "text/plain; charset=utf-8", "", b"", request).await,
    }
}

#[derive(Deserialize)]
struct Pair {
    code: String,
}

async fn pair(conn: &mut Connection<'_>, request: &Request) -> io::Result<bool> {
    let code = match body::<Pair>(request) {
        Ok(Pair { code }) => code,
        Err(error) => return conn.json("400 Bad Request", &json!({ "error": error }), request).await,
    };
    let from = match conn.peer {
        Peer::Network { remote, .. } => remote,
        Peer::Loopback => IpAddr::V4(Ipv4Addr::LOCALHOST),
    };
    match conn.ctx.devices.pair(&code, request.agent.as_deref(), from, now()) {
        Ok((device, token)) => {
            let body = serde_json::to_vec(&json!({ "device": device })).map_err(io::Error::other)?;
            let headers = format!("Cache-Control: no-store\r\n{}", set_cookie(&token));
            conn.send("200 OK", "application/json", &headers, &body, request).await
        }
        Err(refused) => {
            let status = refused.status();
            conn.json(status, &json!({ "error": refused.error() }), request).await
        }
    }
}

/// `GET /api/me`: whether the browser asking is this Mac or a paired device.
pub fn me(ctx: &Ctx, asker: &Asker) -> Result<Value, Error> {
    Ok(match asker {
        Asker::Device(id) => json!({ "asker": "device", "device": ctx.devices.device(id) }),
        _ => json!({ "asker": "mac" }),
    })
}

/// `GET /api/devices`.
pub fn list(ctx: &Ctx, asker: &Asker) -> Result<Value, Error> {
    local_only(asker)?;
    shown(ctx.devices.list(ctx.http_port, now()))
}

/// `POST /api/devices/add`: opens a pairing offer.
pub fn add(ctx: &Ctx, asker: &Asker) -> Result<Value, Error> {
    local_only(asker)?;
    shown(ctx.devices.add(ctx.http_port, now())?)
}

#[derive(Deserialize)]
pub struct Remove {
    id: String,
}

/// `POST /api/devices/remove`.
pub fn remove(ctx: &Ctx, asker: &Asker, Remove { id }: Remove) -> Result<Value, Error> {
    local_only(asker)?;
    shown(ctx.devices.remove(&id, ctx.http_port, now())?)
}

fn local_only(asker: &Asker) -> Result<(), Error> {
    match asker {
        Asker::Local => Ok(()),
        _ => Err(Error::new(ErrorCode::LocalOnly, "only the Mac running torrential can add or remove devices")
            .with_hint("open torrential on the Mac to do it")),
    }
}

/// The list as the web app shows it, with the offer's QR code.
fn shown(DeviceList { devices, offer }: DeviceList) -> Result<Value, Error> {
    #[derive(Serialize)]
    struct Shown {
        devices: Vec<Device>,
        offer: Option<Offer>,
    }
    #[derive(Serialize)]
    struct Offer {
        #[serde(flatten)]
        offer: PairingOffer,
        /// Rows of `1` and `0`, without the quiet zone.
        qr: Vec<String>,
    }
    let offer = offer.map(|offer| Offer { qr: qr(&offer.url), offer });
    value(Shown { devices, offer })
}
