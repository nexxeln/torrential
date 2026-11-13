//! Phones, tablets, and TV browsers that watch from this machine's engine over the home network.

use serde::{Deserialize, Serialize};

/// A browser on the home network that this machine let in. It holds a token in a cookie, which
/// the engine keeps only hashed.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Device {
    /// Eight hex digits, which `torrential devices remove` takes.
    pub id: String,
    /// What the browser said it is, such as "iPhone" or "LG TV".
    pub name: String,
    /// Unix seconds.
    pub added_at: i64,
    /// Unix seconds of its last request, to within a minute.
    pub seen_at: i64,
}

/// A short code that lets one device in, open for a few minutes.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PairingOffer {
    /// Six digits.
    pub code: String,
    /// The page that pairs a phone, with the code in it. The QR code holds this.
    pub url: String,
    /// What to type into a TV's browser: this machine's .local name and the port.
    pub address: String,
    /// This machine's address on the home network and the port, for a browser that cannot open
    /// .local names. `None` when the machine has no such address.
    pub lan_address: Option<String>,
    /// Unix seconds.
    pub expires_at: i64,
}

/// The devices this machine let in, and the pairing offer open now.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct DeviceList {
    /// The most recently added last.
    pub devices: Vec<Device>,
    pub offer: Option<PairingOffer>,
}
