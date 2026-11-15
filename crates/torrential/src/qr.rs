//! QR codes, which the pairing screens show.

/// The modules of a QR code that holds `text`, one string of `1` and `0` for each row, without the
/// quiet zone around it.
pub fn qr(text: &str) -> Vec<String> {
    let Ok(code) = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium) else { return Vec::new() };
    (0..code.size())
        .map(|y| (0..code.size()).map(|x| if code.get_module(x, y) { '1' } else { '0' }).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_qr_code_holds_the_pairing_address() {
        let rows = qr("http://studio-mac.local:47919/pair#482913");
        assert_eq!(rows.len(), 29, "version 3 at medium error correction");
        assert!(rows.iter().all(|row| row.len() == 29 && row.bytes().all(|b| b == b'0' || b == b'1')));
        assert_eq!(&rows[0][..7], "1111111", "a finder pattern in the corner");
    }
}
