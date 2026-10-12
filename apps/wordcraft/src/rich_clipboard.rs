//! Windows rich clipboard formats (#820), bounded before allocating.

#[cfg(any(windows, test))]
const MAX_BYTES: usize = 16 * 1024 * 1024;

#[cfg(any(windows, test))]
/// CF_HTML offsets are bytes from the buffer start, not Unicode character positions.
/// https://learn.microsoft.com/en-us/windows/win32/dataxchg/html-clipboard-format
fn cf_html(bytes: &[u8]) -> Option<&str> {
    if bytes.len() > MAX_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let header_end = text.find('<')?;
    let header = text.get(..header_end)?;
    let mut start = None;
    let mut end = None;
    for line in header.split(['\r', '\n']) {
        let Some((key, value)) = line.split_once(':') else { continue };
        let slot = match key {
            "StartFragment" => &mut start,
            "EndFragment" => &mut end,
            _ => continue,
        };
        if slot.is_some() {
            return None;
        }
        *slot = Some(value.trim().parse::<usize>().ok()?);
    }
    let (start, end) = (start?, end?);
    if start < header_end || start >= end {
        return None;
    }
    let fragment = text.get(start..end)?;
    (!fragment.contains('\0')).then_some(fragment)
}

#[cfg(any(windows, test))]
fn rtf_text(bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_BYTES {
        return None;
    }
    // shortcut: binary RTF falls back to plain; add byte transport if needed.
    if bytes.windows(4).any(|b| b == b"\\bin") {
        return None;
    }
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let bytes = bytes.get(..end)?;
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Some(text.to_owned());
    }
    let size = bytes.iter().try_fold(0usize, |size, b| size.checked_add(if b.is_ascii() { 1 } else { 4 }))?;
    if size > MAX_BYTES {
        return None;
    }
    let mut text = String::new();
    text.try_reserve_exact(size).ok()?;
    for byte in bytes {
        if byte.is_ascii() {
            text.push(char::from(*byte));
        } else {
            use std::fmt::Write;
            write!(text, "\\'{byte:02x}").ok()?;
        }
    }
    Some(text)
}

#[cfg(windows)]
fn bytes(format: u32) -> Option<Vec<u8>> {
    let size = clipboard_win::raw::size(format)?.get();
    if size > MAX_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).ok()?;
    bytes.resize(size, 0);
    let copied = clipboard_win::raw::get(format, &mut bytes).ok()?;
    (copied == size).then_some(bytes)
}

#[cfg(windows)]
fn plain_text(bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_BYTES || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().copied().map(u16::from_le_bytes).take_while(|u| *u != 0).collect();
    String::from_utf16(&units).ok()
}

#[cfg(windows)]
pub fn read() -> Option<serde_json::Value> {
    // One guard keeps rich and plain formats from different clipboard owners from mixing.
    let _clipboard = clipboard_win::Clipboard::new_attempts(3).ok()?;
    let mut value = serde_json::Map::new();
    if let Some(text) = bytes(clipboard_win::formats::CF_UNICODETEXT).and_then(|b| plain_text(&b)) {
        value.insert("text".into(), text.into());
    }
    if let Some(html) = clipboard_win::raw::register_format("HTML Format").and_then(|f| bytes(f.get()))
        && let Some(fragment) = cf_html(&html)
    {
        value.insert("html".into(), fragment.into());
    }
    if let Some(rtf) = clipboard_win::raw::register_format("Rich Text Format").and_then(|f| bytes(f.get()))
        && let Some(rtf) = rtf_text(&rtf)
    {
        value.insert("rtf".into(), rtf.into());
    }
    (!value.is_empty()).then_some(serde_json::Value::Object(value))
}

#[cfg(not(windows))]
pub fn read() -> Option<serde_json::Value> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(fragment: &str) -> Vec<u8> {
        let header = "Version:1.0\r\nStartHTML:0000000105\r\nEndHTML:0000000000\r\nStartFragment:0000000137\r\nEndFragment:0000000000\r\n";
        let prefix = "<html><body><!--StartFragment-->";
        let suffix = "<!--EndFragment--></body></html>";
        let start = header.len() + prefix.len();
        let end = start + fragment.len();
        let all_end = end + suffix.len();
        format!("Version:1.0\r\nStartHTML:{:010}\r\nEndHTML:{all_end:010}\r\nStartFragment:{start:010}\r\nEndFragment:{end:010}\r\n{prefix}{fragment}{suffix}\0", header.len()).into_bytes()
    }

    #[test]
    fn fragment_offsets_are_utf8_bytes() {
        let bytes = html("<b>café 🦀</b>");
        assert_eq!(cf_html(&bytes), Some("<b>café 🦀</b>"));
    }

    #[test]
    fn malformed_html_is_rejected() {
        for bytes in [
            b"<b>missing header</b>".as_slice(),
            b"StartFragment:9\nEndFragment:1\n<b>x</b>",
            b"StartFragment:999999999999999999999999\nEndFragment:1\n<b>x</b>",
            b"StartFragment:-1\nEndFragment:1\n<b>x</b>",
        ] {
            assert!(cf_html(bytes).is_none());
        }
        let mut bytes = html("é");
        let position = bytes.iter().position(|b| *b == 0xc3).unwrap();
        bytes[position] = 0xff;
        assert!(cf_html(&bytes).is_none());
        let bytes = html("é");
        let start = bytes.iter().position(|b| *b == 0xc3).unwrap() + 1;
        let bad = String::from_utf8(bytes).unwrap().replace(&format!("StartFragment:{:010}", start - 1), &format!("StartFragment:{start:010}"));
        assert!(cf_html(bad.as_bytes()).is_none());
    }

    #[test]
    fn clipboard_payload_is_capped() {
        let mut bytes = html("<b>small</b>");
        bytes.resize(MAX_BYTES + 1, b' ');
        assert!(cf_html(&bytes).is_none());
        assert!(rtf_text(&bytes).is_none());
    }

    #[test]
    fn ansi_rtf_bytes_are_preserved_as_codepage_escapes() {
        assert_eq!(rtf_text(b"{\\rtf1\\ansi\\ansicpg1252 caf\xe9}\0"), Some("{\\rtf1\\ansi\\ansicpg1252 caf\\'e9}".into()));
        assert_eq!(rtf_text(b"{\\rtf1\\bin1 \xff}"), None);
        assert_eq!(rtf_text(b"{\\rtf1\\bin2 \0\xff}"), None);
        assert_eq!(rtf_text(b"{\\rtf1\\bin1 \0}"), None);
        assert_eq!(rtf_text(b"{\\rtf1 hello}"), Some("{\\rtf1 hello}".into()));
    }

    #[cfg(windows)]
    #[test]
    fn plain_utf16_keeps_unicode_and_rejects_malformed_input() {
        let bytes: Vec<u8> = "café 🦀\0padding".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(plain_text(&bytes), Some("café 🦀".into()));
        assert_eq!(plain_text(&[0]), None);
        assert_eq!(plain_text(&[0x00, 0xd8]), None);
    }
}
