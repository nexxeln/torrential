use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::daemon::store::Store;

const PORT: u16 = 47919;
const NOW: i64 = 1_800_000_000;
const PHONE: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 40));
const IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148";

struct Home {
    dir: tempfile::TempDir,
    devices: Arc<Devices>,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        drop(Store::open(&dir.path().join("state.db")).unwrap());
        let devices = Arc::new(Devices::open(&dir.path().join("state.db"), NOW).unwrap());
        Self { dir, devices }
    }

    fn reopened(&self) -> Devices {
        Devices::open(&self.dir.path().join("state.db"), NOW).unwrap()
    }

    fn code(&self) -> String {
        self.devices.add(PORT, NOW).unwrap().offer.unwrap().code
    }

    fn pair(&self, code: &str, agent: &str, now: i64) -> Result<(Device, String), Refused> {
        self.devices.pair(code, Some(agent), PHONE, now)
    }
}

fn refusal(result: Result<(Device, String), Refused>) -> String {
    result.map(|_| "paired".to_string()).unwrap_or_else(|refused| format!("{refused:?}"))
}

#[tokio::test]
async fn a_phone_pairs_with_the_code_and_its_cookie_lets_it_in_from_then_on() {
    let home = Home::new();
    let offer = home.devices.add(PORT, NOW).unwrap().offer.unwrap();
    assert_eq!(offer.code.len(), 6);
    assert!(offer.code.bytes().all(|b| b.is_ascii_digit()));
    assert_eq!(offer.url, format!("http://{}/pair#{}", offer.address, offer.code));
    assert_eq!(offer.expires_at, NOW + 300);

    let typed = format!("{} {}", &offer.code[..3], &offer.code[3..]);
    let (device, token) = home.pair(&typed, IPHONE, NOW + 10).unwrap();
    assert_eq!((device.name.as_str(), device.added_at, device.seen_at), ("iPhone", NOW + 10, NOW + 10));
    assert_eq!(device.id.len(), 8);
    assert_eq!(token.len(), 64);
    assert_eq!(home.devices.holder(&token, NOW + 20), Some(Device { seen_at: NOW + 20, ..device.clone() }));
    assert_eq!(home.devices.list(PORT, NOW + 20).offer, None, "a code lets one device in");
    assert_eq!(refusal(home.pair(&offer.code, IPHONE, NOW + 30)), "NoOffer");

    let restarted = home.reopened();
    assert_eq!(restarted.holder(&token, NOW + 90).map(|d| d.id), Some(device.id), "pairing outlives the engine");
    assert_eq!(home.devices.holder(&"0".repeat(64), NOW), None);
    assert_eq!(home.devices.holder("not a token", NOW), None);
}

#[test]
fn the_database_keeps_only_a_hash_of_the_token() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let home = Home::new();
    let code = runtime.block_on(async { home.code() });
    let (_, token) = home.pair(&code, IPHONE, NOW).unwrap();
    let conn = rusqlite::Connection::open(home.dir.path().join("state.db")).unwrap();
    let stored: Vec<u8> = conn.query_row("SELECT token_hash FROM devices", [], |r| r.get(0)).unwrap();
    assert_eq!(hex(&stored), hex(&Sha256::digest(Token::parse(&token).unwrap().0)));
    let file = std::fs::read(home.dir.path().join("state.db-wal")).unwrap_or_default();
    let file = [std::fs::read(home.dir.path().join("state.db")).unwrap(), file].concat();
    let raw = Token::parse(&token).unwrap().0;
    assert!(!file.windows(32).any(|w| w == raw), "the token's bytes are nowhere in the database");
    assert!(!file.windows(64).any(|w| w == token.as_bytes()), "nor its hex");
}

#[tokio::test]
async fn a_code_stops_working_after_five_minutes() {
    let home = Home::new();
    let code = home.code();
    assert_eq!(refusal(home.pair(&code, IPHONE, NOW + 300)), "NoOffer");
    home.devices.expire(NOW + 299);
    assert!(home.devices.list(PORT, NOW + 299).offer.is_some(), "not expired yet");
    home.devices.expire(NOW + 300);
    assert_eq!(home.devices.list(PORT, NOW).offer, None);
}

#[tokio::test]
async fn five_wrong_codes_close_the_offer_so_guessing_six_digits_fails() {
    let home = Home::new();
    let code = home.code();
    let wrong = format!("{:06}", (code.parse::<u32>().unwrap() + 1) % 1_000_000);
    for _ in 0..4 {
        assert_eq!(refusal(home.pair(&wrong, IPHONE, NOW)), "WrongCode");
    }
    assert_eq!(refusal(home.pair("", IPHONE, NOW)), "Burned");
    assert_eq!(refusal(home.pair(&code, IPHONE, NOW)), "NoOffer", "the right code no longer works either");
}

#[tokio::test]
async fn one_address_gets_ten_pairing_tries_a_minute() {
    let home = Home::new();
    for _ in 0..10 {
        assert_eq!(refusal(home.devices.pair("123456", None, PHONE, NOW)), "NoOffer");
    }
    let code = home.code();
    assert_eq!(refusal(home.devices.pair(&code, None, PHONE, NOW)), "TooManyTries", "even with the right code");
    let next_minute = NOW - NOW.rem_euclid(60) + 60;
    assert_eq!(
        refusal(home.devices.pair(&code, None, PHONE, next_minute)),
        "paired",
        "the count starts again each minute"
    );
}

#[tokio::test]
async fn the_engine_listens_on_the_network_while_a_device_is_paired_a_code_is_open_or_a_cast_plays() {
    let home = Home::new();
    let reach = home.devices.reach();
    assert_eq!(*reach.borrow(), Reach::Loopback);
    home.code();
    assert_eq!(*reach.borrow(), Reach::Network, "an open code");
    home.devices.expire(NOW + 300);
    assert_eq!(*reach.borrow(), Reach::Loopback, "the code expired");

    let code_again = home.code();
    let (device, _) = home.pair(&code_again, IPHONE, NOW).unwrap();
    home.devices.expire(NOW + 300);
    assert_eq!(*reach.borrow(), Reach::Network, "a paired device");
    home.devices.remove(&device.id, PORT, NOW).unwrap();
    assert_eq!(*reach.borrow(), Reach::Loopback, "the last device removed");

    let grant = home.devices.grant(TorrentId::from_bytes([7; 20]), 0, "Sintel.mkv").unwrap();
    assert_eq!(*reach.borrow(), Reach::Network, "a cast");
    drop(grant);
    assert_eq!(*reach.borrow(), Reach::Loopback, "the cast ended");
    assert_eq!(*home.reopened().reach().borrow(), Reach::Loopback);
}

#[tokio::test]
async fn a_removed_device_is_locked_out_and_its_open_requests_end() {
    let home = Home::new();
    let code = home.code();
    let (device, token) = home.pair(&code, IPHONE, NOW).unwrap();
    let asker = Asker::Device(device.id.clone());
    let devices = home.devices.clone();
    let waiting = tokio::spawn(async move { devices.revoked(&asker).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished(), "a paired device keeps its requests");
    let left = home.devices.remove(&device.id, PORT, NOW).unwrap();
    assert_eq!(left.devices, []);
    tokio::time::timeout(Duration::from_secs(1), waiting).await.expect("removing ends its requests").unwrap();
    assert_eq!(home.devices.holder(&token, NOW), None);
    assert_eq!(home.reopened().holder(&token, NOW), None);
    let error = home.devices.remove(&device.id, PORT, NOW).unwrap_err();
    assert_eq!(
        (error.code, error.message, error.hint),
        (
            ErrorCode::NotFound,
            format!("no device has the ID {}", device.id),
            Some("run torrential devices to list them".into())
        )
    );
}

#[tokio::test]
async fn a_grant_serves_its_one_path_until_it_is_dropped() {
    let home = Home::new();
    let film = TorrentId::from_bytes([7; 20]);
    let grant = home.devices.grant(film, 2, "Big Buck Bunny.mp4").unwrap();
    let path = grant.path().to_string();
    let token = path.strip_prefix("/cast/").unwrap().split('/').next().unwrap().to_string();
    assert_eq!(path, format!("/cast/{token}/Big%20Buck%20Bunny.mp4"));
    let granted = home.devices.granted(&path).unwrap();
    assert_eq!((granted.id, granted.index), (film, 2));
    for other in [
        format!("/cast/{token}/other.mp4"),
        format!("/cast/{token}/"),
        format!("/cast/{token}"),
        format!("/cast/{token}/Big%20Buck%20Bunny.mp4/"),
        format!("/cast/{token}/../Big%20Buck%20Bunny.mp4"),
        format!("/cast/{}/Big%20Buck%20Bunny.mp4", token.to_uppercase()),
        format!("/cast/{}0/Big%20Buck%20Bunny.mp4", &token[..63]),
        format!("/{}/2/Big%20Buck%20Bunny.mp4", film.to_hex()),
    ] {
        assert_eq!(home.devices.granted(&other), None, "{other}");
    }
    let asker = Asker::Cast(granted);
    let devices = home.devices.clone();
    let waiting = tokio::spawn(async move { devices.revoked(&asker).await });
    drop(grant);
    tokio::time::timeout(Duration::from_secs(1), waiting).await.expect("dropping ends the cast's requests").unwrap();
    assert_eq!(home.devices.granted(&path), None);
}

#[test]
fn a_device_is_named_by_what_its_browser_runs_on() {
    let cases = [
        (IPHONE, "iPhone"),
        ("Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X)", "iPad"),
        ("Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 Chrome/120 Mobile Safari/537.36", "Android"),
        ("Mozilla/5.0 (Web0S; Linux/SmartTV) AppleWebKit/537.36 Chrome/94 Safari/537.36 WebAppManager", "LG TV"),
        (
            "Mozilla/5.0 (SMART-TV; LINUX; Tizen 6.5) AppleWebKit/537.36 SamsungBrowser/4.0 TV Safari/537.36",
            "Samsung TV",
        ),
        ("Mozilla/5.0 (Linux; Android 9; AFTMM Build/PS7233) AppleWebKit/537.36 Chrome/112 Safari/537.36", "Fire TV"),
        ("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 Version/18.0 Safari/605.1.15", "Mac"),
        ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/120 Safari/537.36", "Windows PC"),
        ("curl/8.7.1", "Browser"),
    ];
    for (agent, name) in cases {
        assert_eq!(device_name(Some(agent)), name, "{agent}");
    }
    assert_eq!(device_name(None), "Browser");
}

#[tokio::test]
async fn a_second_iphone_is_named_iphone_2() {
    let home = Home::new();
    let first = home.code();
    home.pair(&first, IPHONE, NOW).unwrap();
    let second = home.code();
    let (device, _) =
        home.devices.pair(&second, Some(IPHONE), IpAddr::V4(Ipv4Addr::new(192, 168, 1, 41)), NOW).unwrap();
    assert_eq!(device.name, "iPhone 2");
    let names: Vec<String> = home.devices.list(PORT, NOW).devices.into_iter().map(|d| d.name).collect();
    assert_eq!(names.len(), 2);
}

#[test]
fn a_qr_code_holds_the_pairing_address() {
    let rows = qr("http://studio-mac.local:47919/pair#482913");
    assert_eq!(rows.len(), 29, "version 3 at medium error correction");
    assert!(rows.iter().all(|row| row.len() == 29 && row.bytes().all(|b| b == b'0' || b == b'1')));
    assert_eq!(&rows[0][..7], "1111111", "a finder pattern in the corner");
}
