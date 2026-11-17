//! torrential on one machine driving the engine on another: two homes on this machine, the
//! second's engine reached by its address on the home network, as another machine reaches it.

use std::time::Duration;

use serde_json::Value;
use torrential_core::protocol::{Request, WIRE_VERSION};
use torrential_daemon::wire::{self, Address, Answer, Client};

mod common;
use common::{Home, hooks};

const ARCH: &str = "9b521219bc48550140a666f74a29048ed78e933b";

/// The IDs of the downloads `home`'s torrential lists.
fn ids(home: &Home) -> Vec<String> {
    let jobs = home.json(&["status", "--json"])["jobs"].as_array().unwrap().clone();
    jobs.iter().map(|job| job["id"].as_str().unwrap().to_string()).collect()
}

/// The code and message of a command that fails.
fn refused(home: &Home, args: &[&str]) -> (i32, String, String) {
    let run = home.run(&[args, &["--json"]].concat());
    let doc = run.json();
    let error = &doc["error"];
    (run.code, error["code"].as_str().unwrap().into(), error["message"].as_str().unwrap().into())
}

#[test]
fn connect_drives_the_engine_on_another_machine_and_never_starts_one_here() {
    let server = Home::new().with(hooks::ENGINE, "fake").with(hooks::FAKE_RATE, "100GiB");
    let laptop = Home::new().with(hooks::ENGINE, "fake");
    let offer = server.json(&["devices", "add", "--json"]);
    let lan = offer["offer"]["lan_address"].as_str().expect("this test needs an address on a home network");
    let link = format!("http://{lan}/pair#{}", offer["offer"]["code"].as_str().unwrap());
    let address = format!("http://{lan}");

    let connected = laptop.json(&["connect", &link, "--json"]);
    assert_eq!((&connected["engine"], &connected["address"]), (&Value::from("remote"), &Value::from(address.clone())));
    let device = connected["device"].as_str().unwrap().to_string();
    assert_eq!(server.json(&["devices", "--json"])["devices"][0]["id"], device);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let saved = std::fs::metadata(laptop.path("th/state/remotes.json")).unwrap();
        assert_eq!(saved.permissions().mode() & 0o777, 0o600, "only this user reads the device token");
    }

    let search = laptop.run(&["search", "arch", "linux"]);
    let first = search.stdout.lines().nth(1).and_then(|row| row.split('\t').next());
    assert_eq!(first, Some("9b5212"), "{}", search.stderr);
    let got = laptop.json(&["get", "9b5212", "--json"]);
    assert_eq!(got["reply"]["job"]["id"], ARCH);
    assert!(ids(&server).contains(&ARCH.to_string()), "the download runs on the server");
    assert_eq!(laptop.json(&["status", "9b5212", "--json"])["jobs"][0]["id"], ARCH);
    let engine = laptop.json(&["engine", "status", "--json"]);
    assert_eq!(
        (&engine["engine"], &engine["address"], &engine["device"], engine.get("log")),
        (&Value::from("running"), &Value::from(address.clone()), &Value::from(device.clone()), None)
    );

    let elsewhere = laptop.path("elsewhere").display().to_string();
    let cases: [(&[&str], &str); 4] = [
        (
            &["play", "9b5212"],
            "torrential play opens a player on this machine, which plays from this machine's engine alone",
        ),
        (&["engine", "stop"], "torrential engine stop stops the engine on this machine"),
        (&["devices"], "only the machine that runs the engine can list, add, or remove devices"),
        (
            &["get", "9b5212", "--dir", &elsewhere],
            "only the machine that runs the engine can choose the folder a download goes to",
        ),
    ];
    for (args, message) in cases {
        let (code, kind, said) = refused(&laptop, args);
        assert_eq!((code, kind.as_str()), (6, "local_only"), "{args:?}");
        assert!(said.starts_with(message), "{args:?}: {said}");
    }
    let removed = laptop.json(&["rm", "9b5212", "--json"]);
    assert_eq!(removed["changes"][0]["id"], ARCH, "{removed:#}");
    assert!(!ids(&server).contains(&ARCH.to_string()), "the server's engine removed it");

    let (ip, port) = lan.rsplit_once(':').unwrap();
    let address = Address { tls: false, host: ip.to_string(), port: port.parse().unwrap() };
    let client = Client { version: "0.1.0", wire: WIRE_VERSION };
    let stranger = wire::post(&address, &"7".repeat(64), client, &Request::Hello, Duration::from_secs(10)).unwrap();
    assert!(matches!(stranger, Answer::Status(401)), "a stranger without a paired token gets nothing");

    assert_eq!(server.run(&["engine", "stop"]).code, 0);
    let (code, kind, said) = refused(&laptop, &["status"]);
    assert_eq!((code, kind.as_str()), (6, "daemon_unavailable"));
    assert!(said.starts_with(&format!("cannot reach the engine at http://{lan}")), "{said}");
    assert!(!laptop.path("th/state/engine-token").exists(), "no engine started on the laptop");

    assert_eq!(laptop.json(&["connect", "--local", "--json"])["engine"], "local");
    assert!(!ids(&laptop).contains(&ARCH.to_string()), "the laptop's own engine never had it");
    assert!(laptop.path("th/state/engine-token").exists());
}
