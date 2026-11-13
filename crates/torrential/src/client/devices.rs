//! `torrential devices`: the phones and TVs that watch from this Mac over the home network.

use std::io::Write;

use torrential_core::protocol::{Request, Response};
use torrential_core::units::count;
use torrential_core::{DeviceList, Error, PairingOffer};

use super::connect::{self, Conn};
use super::{answer, emit};
use crate::cli::DevicesCommand;
use crate::daemon::devices::qr;
use crate::daemon::owner::now;
use crate::output::format;
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc, Mode, Typed};

pub fn run(mut conn: Conn, command: Option<DevicesCommand>, mode: Mode) -> Result<u8, Error> {
    let request = match command {
        None | Some(DevicesCommand::List) => Request::Devices,
        Some(DevicesCommand::Add) => Request::AddDevice,
        Some(DevicesCommand::Remove { id }) => Request::RemoveDevice { id },
    };
    let adding = request == Request::AddDevice;
    let list = match answer(conn.call(&request)?)? {
        Response::Devices { list } => list,
        other => return Err(connect::unexpected(&other)),
    };
    let doc = match &list.offer {
        Some(offer) if adding => {
            if mode == Mode::Human {
                let mut stdout = anstream::stdout().lock();
                let _ = stdout.write_all(blocks(&qr(&offer.url)).as_bytes());
            }
            offer_doc(offer, list.unreachable.as_deref(), now())
        }
        _ => list_doc(&list, now()),
    };
    Ok(emit(mode, &doc, &Typed { kind: "devices", body: &list }))
}

fn list_doc(list: &DeviceList, now: i64) -> Doc {
    let rows = list
        .devices
        .iter()
        .map(|device| {
            vec![
                Cell::styled(&device.id, Token::Accent),
                Cell::new(&device.name),
                Cell::new(format::age(Some(device.added_at), now)),
                Cell::new(format::age(Some(device.seen_at), now)),
            ]
        })
        .collect();
    let mut summary = vec![count(list.devices.len(), "device")];
    if let Some(offer) = &list.offer {
        summary.push(format!(
            "code {} is open: open http://{} on the phone or TV and enter it",
            offer.code, offer.address
        ));
    }
    summary.extend(list.unreachable.clone());
    let next = list.devices.is_empty().then(|| "torrential devices add".to_string());
    Doc { body: Body::Table { header: vec!["ID", "NAME", "ADDED", "SEEN"], rows }, summary, next }
}

fn offer_doc(offer: &PairingOffer, unreachable: Option<&str>, now: i64) -> Doc {
    let mut fields = vec![
        ("address", Cell::styled(format!("http://{}", offer.address), Token::Accent)),
        ("code", Cell::styled(&offer.code, Token::Accent)),
    ];
    if let Some(lan) = offer.lan_address.as_ref().filter(|lan| **lan != offer.address) {
        fields.push(("or", Cell::new(format!("http://{lan}"))));
    }
    fields.push(("expires", Cell::new(format!("in {}", format::age(Some(now), offer.expires_at)))));
    let mut summary =
        vec!["scan the QR code with the phone's camera, or open the address on the TV and enter the code".to_string()];
    summary.extend(unreachable.map(String::from));
    Doc { body: Body::Fields { title: Vec::new(), fields }, summary, next: Some("torrential devices".into()) }
}

/// A QR code in half blocks, two rows to a line, dark on light whatever the terminal's colors,
/// inside the four modules of light a camera needs to find it.
fn blocks(rows: &[String]) -> String {
    const MARGIN: isize = 4;
    let size = rows.len() as isize;
    let dark = |x: isize, y: isize| {
        (0..size).contains(&x) && (0..size).contains(&y) && rows[y as usize].as_bytes()[x as usize] == b'1'
    };
    let mut out = String::new();
    for y in (-MARGIN..size + MARGIN).step_by(2) {
        out.push_str("\x1b[30;107m");
        for x in -MARGIN..size + MARGIN {
            out.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrential_core::Device;

    const NOW: i64 = 1_800_000_000;

    fn offer() -> PairingOffer {
        PairingOffer {
            code: "482913".into(),
            url: "http://studio-mac.local:47919/pair#482913".into(),
            address: "studio-mac.local:47919".into(),
            lan_address: Some("192.168.1.23:47919".into()),
            expires_at: NOW + 300,
        }
    }

    #[test]
    fn the_list_shows_each_device_and_the_open_code() {
        let list = DeviceList {
            devices: vec![
                Device { id: "3f9a0c2e".into(), name: "iPhone".into(), added_at: NOW - 3 * 86_400, seen_at: NOW - 120 },
                Device { id: "b71d04aa".into(), name: "LG TV".into(), added_at: NOW - 3_600, seen_at: NOW - 3_600 },
            ],
            offer: Some(offer()),
            unreachable: None,
        };
        assert_eq!(
            list_doc(&list, NOW).plain(),
            "ID\tNAME\tADDED\tSEEN\n\
             3f9a0c2e\tiPhone\t3d\t2m\n\
             b71d04aa\tLG TV\t1h\t1h\n\
             # 2 devices\n\
             # code 482913 is open: open http://studio-mac.local:47919 on the phone or TV and enter it\n"
        );
        let empty = list_doc(&DeviceList::default(), NOW).plain();
        assert_eq!(empty, "ID\tNAME\tADDED\tSEEN\n# 0 devices\nnext: torrential devices add\n");
    }

    #[test]
    fn the_list_says_why_no_phone_or_tv_can_reach_the_mac() {
        let why = "no phone or TV can reach this Mac yet, because torrential cannot listen on the home network on port 47919 (address in use) and keeps trying";
        let list = DeviceList { unreachable: Some(why.into()), ..DeviceList::default() };
        assert_eq!(
            list_doc(&list, NOW).plain(),
            format!("ID\tNAME\tADDED\tSEEN\n# 0 devices\n# {why}\nnext: torrential devices add\n")
        );
    }

    #[test]
    fn adding_shows_the_address_and_the_code_to_type() {
        assert_eq!(
            offer_doc(&offer(), None, NOW).plain(),
            "address: http://studio-mac.local:47919\n\
             code:    482913\n\
             or:      http://192.168.1.23:47919\n\
             expires: in 5m\n\
             # scan the QR code with the phone's camera, or open the address on the TV and enter the code\n\
             next:    torrential devices\n"
        );
    }

    #[test]
    fn a_qr_code_takes_two_rows_a_line_inside_a_light_margin() {
        let rows = qr("http://studio-mac.local:47919/pair#482913");
        let text = blocks(&rows);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), (rows.len() + 8).div_ceil(2));
        let blank = format!("\x1b[30;107m{}\x1b[0m", " ".repeat(rows.len() + 8));
        assert_eq!(lines[..2], [blank.as_str(), blank.as_str()], "the margin above is light");
        assert!(lines[2].starts_with("\x1b[30;107m    █▀▀▀▀▀█"), "a finder pattern after the margin: {}", lines[2]);
    }
}
