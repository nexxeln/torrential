//! QR codes, which the pairing screens show.

/// The modules of a QR code that holds `text`, one string of `1` and `0` for each row, without the
/// quiet zone around it.
pub fn qr(text: &str) -> Vec<String> {
    let Ok(code) = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium) else { return Vec::new() };
    (0..code.size())
        .map(|y| (0..code.size()).map(|x| if code.get_module(x, y) { '1' } else { '0' }).collect())
        .collect()
}
