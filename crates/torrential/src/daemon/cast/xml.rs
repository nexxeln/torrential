//! Just enough XML for UPnP: the text of an element found by its name, whatever namespace prefix
//! the device gives it, and escaping. Device descriptions and SOAP answers nest no element inside
//! one of the same name that torrential reads.

/// The inside of each element named `name`, prefix or not, in document order.
pub fn all<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut found = Vec::new();
    let mut rest = xml;
    while let Some((inside, after)) = next(rest, name) {
        found.push(inside);
        rest = after;
    }
    found
}

/// The inside of the first element named `name`.
pub fn first<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    next(xml, name).map(|(inside, _)| inside)
}

/// The text of the first element named `name`, unescaped and trimmed.
pub fn text(xml: &str, name: &str) -> Option<String> {
    first(xml, name).map(|inside| unescape(inside).trim().to_string())
}

/// The first element named `name` in `xml`, and what follows it.
fn next<'a>(xml: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
    let mut at = 0;
    loop {
        let open = at + xml[at..].find('<')?;
        let tag_end = open + xml[open..].find('>')?;
        let tag = &xml[open + 1..tag_end];
        at = tag_end + 1;
        if tag.starts_with(['/', '?', '!']) {
            continue;
        }
        let qualified = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or_default();
        if local(qualified) != name {
            continue;
        }
        if tag.ends_with('/') {
            return Some(("", &xml[at..]));
        }
        let close = format!("</{qualified}>");
        let end = at + xml[at..].find(&close)?;
        return Some((&xml[at..end], &xml[end + close.len()..]));
    }
}

fn local(qualified: &str) -> &str {
    qualified.rsplit(':').next().unwrap_or(qualified)
}

/// Text with its entities and CDATA sections read.
pub fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(['&', '<']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(cdata) = rest.strip_prefix("<![CDATA[") {
            let end = cdata.find("]]>").unwrap_or(cdata.len());
            out.push_str(&cdata[..end]);
            rest = cdata.get(end + 3..).unwrap_or_default();
            continue;
        }
        let entity = rest.find(';').filter(|&end| end <= 10).and_then(|end| Some((char_of(&rest[1..end])?, end)));
        match entity {
            Some((c, end)) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push_str(&rest[..1]);
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn char_of(entity: &str) -> Option<char> {
    match entity {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let number = entity.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// `text` safe inside an element or a quoted attribute.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_are_found_by_name_whatever_their_prefix() {
        let xml = r#"<?xml version="1.0"?><s:Envelope xmlns:s="x"><s:Body>
            <u:GetTransportInfoResponse xmlns:u="y"><CurrentTransportState>PLAYING</CurrentTransportState>
            <CurrentSpeed>1</CurrentSpeed><Empty/></u:GetTransportInfoResponse></s:Body></s:Envelope>"#;
        assert_eq!(text(xml, "CurrentTransportState").as_deref(), Some("PLAYING"));
        assert_eq!(text(xml, "Empty").as_deref(), Some(""));
        assert_eq!(text(xml, "Missing"), None);
        assert!(first(xml, "GetTransportInfoResponse").unwrap().contains("<CurrentSpeed>1</CurrentSpeed>"));
        let services = "<service><t>a</t></service><service><t>b</t></service>";
        assert_eq!(all(services, "service"), ["<t>a</t>", "<t>b</t>"]);
    }

    #[test]
    fn entities_and_cdata_read_back_as_text_and_escaping_undoes_them() {
        assert_eq!(unescape("Tom &amp; Jerry &lt;3 &#233;t&#xE9; &bogus; &"), "Tom & Jerry <3 été &bogus; &");
        assert_eq!(unescape("<![CDATA[<a href=\"x\">]]> after"), "<a href=\"x\"> after");
        let didl = r#"<DIDL-Lite><item id="0">"Night" & Day's</item></DIDL-Lite>"#;
        assert_eq!(unescape(&escape(didl)), didl);
        assert_eq!(escape("a<b & \"c\""), "a&lt;b &amp; &quot;c&quot;");
    }
}
