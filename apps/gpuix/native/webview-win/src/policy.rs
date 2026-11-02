//! The rules `native/webview.m` applies to URLs and paths, and the small decoders the Windows host needs. Platform
//! independent so they are unit-tested on any host.

use std::path::{Component, Path};

/// The wrapper's origin and path. The daemon's preview `frame-ancestors` already lists `http://tauri.localhost`
/// (`previews/files.rs` `FRAME_ANCESTORS`), the origin Tauri serves its own pages from on Windows. A
/// `WebResourceRequested` filter answers every URL under it, so nothing there reaches the network.
pub const WRAPPER_PREFIX: &str = "http://tauri.localhost/embedded/";

/// `http://tauri.localhost/embedded/<id>`, with the id escaped as a path segment (macOS: `URLPathAllowedCharacterSet`).
pub fn wrapper_url(id: &str) -> String {
    let mut url = String::from(WRAPPER_PREFIX);
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&byte) {
            url.push(byte as char);
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

/// `scheme://host[:port]` of an http(s) URL, lowercased, without a default port; `None` for other schemes and for URLs
/// with credentials (macOS: `-[KybernWebHost allowsURL:]`).
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.contains('@') || authority.is_empty() {
        return None;
    }
    let authority = authority.to_ascii_lowercase();
    let default = if scheme == "http" { ":80" } else { ":443" };
    let authority = authority.strip_suffix(default).unwrap_or(&authority);
    Some(format!("{scheme}://{authority}"))
}

/// The body of a PEM certificate without its armor and whitespace, so two encodings of one certificate compare equal.
pub fn pem_body(pem: &str) -> String {
    pem.lines().filter(|line| !line.starts_with("-----")).flat_map(|line| line.chars()).filter(|c| !c.is_whitespace()).collect()
}

/// Whether a certificate error for `url` is the pinned certificate of a Fast preview (ADE-116): an `https` loopback origin
/// the document may load, presenting exactly one of the `trusted` certificates (PEM bodies, [`pem_body`]). Every other
/// certificate error keeps WebView2's default (the load fails).
pub fn pinned_certificate(url: &str, pem: &str, trusted: &[String], allowed: &[String]) -> bool {
    let Some(origin) = origin_of(url) else { return false };
    let Some(authority) = origin.strip_prefix("https://") else { return false };
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) && !authority.ends_with(']') => host,
        _ => authority,
    };
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    let body = pem_body(pem);
    loopback && !body.is_empty() && trusted.iter().any(|t| *t == body) && allowed.iter().any(|a| a.eq_ignore_ascii_case(&origin))
}

/// Whether a frame may load `url`: an http(s) URL on one of the document's origins, or `about:`.
pub fn frame_allows(url: &str, allowed: &[String]) -> bool {
    if url.starts_with("about:") {
        return true;
    }
    origin_of(url).is_some_and(|origin| allowed.iter().any(|a| a.eq_ignore_ascii_case(&origin)))
}

/// `snapshotRect` writes only PNGs under the temp folder (macOS: `NSTemporaryDirectory()`), never through `..`.
pub fn snapshot_path_allowed(path: &str, temp: &Path) -> bool {
    snapshot_path_allowed_in(path, &[temp])
}

/// `snapshot` (a live visual going back to its picture) writes into the app's image cache, `snapshotRect` into the temp
/// folder: a PNG under one of `roots`, never through `..`.
pub fn snapshot_path_allowed_in(path: &str, roots: &[&Path]) -> bool {
    let candidate = Path::new(path);
    candidate.is_absolute()
        && roots.iter().any(|root| root.is_absolute() && candidate.starts_with(root))
        && !candidate.components().any(|c| matches!(c, Component::ParentDir))
        && candidate.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"))
}

/// The oldest WebView2 Runtime the host works with: `ICoreWebView2_11` (DevTools sessions for the child frame's isolated
/// world, `ContextMenuRequested`) and `ICoreWebView2Environment10` (per-document InPrivate profiles) arrived with SDK
/// 1.0.1245.22, whose runtime floor is this. `kybern_web_available` compares with `CompareBrowserVersions`.
pub const MIN_RUNTIME: &str = "101.0.1210.39";

/// A WebView2 profile name for one document: unique per document, so pages of different documents never share cookies,
/// storage, caches or service workers (macOS: a `nonPersistentDataStore` per document). Profile names allow letters,
/// digits and `#@$()+-_~.` up to 64 characters.
pub fn profile_name(generation: Option<u64>, sequence: u64) -> String {
    match generation {
        Some(generation) => format!("doc-{generation}-{sequence}"),
        None => format!("doc-{sequence}"),
    }
}

/// Environment variables the WebView2 loader reads before its options (`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`,
/// `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER`, `WEBVIEW2_USER_DATA_FOLDER`, `WEBVIEW2_PIPE_FOR_SCRIPT_DEBUGGER`, ...). One
/// inherited from a parent process could open a remote-debugging port or load another browser; the host drops them.
pub fn is_webview2_override(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("WEBVIEW2_") || upper.starts_with("COREWEBVIEW2_")
}

/// Default context-menu items a page keeps: editing and copying (macOS shows those too). Navigation, Save as, Print,
/// Inspect, share and the rest go.
pub fn context_menu_keeps(name: &str) -> bool {
    matches!(
        name,
        "cut"
            | "copy"
            | "paste"
            | "pasteAndMatchStyle"
            | "selectAll"
            | "undo"
            | "redo"
            | "copyLinkLocation"
            | "copyImage"
            | "copyImageLocation"
            | "emoji"
    )
}

/// The child frame's isolated world name and its binding (`frame.js`; macOS: the `KybernPreview` content world and its
/// `kybernFrame` handler). Neither is reachable from the page's own scripts.
pub const FRAME_WORLD: &str = "KybernPreview";
pub const FRAME_BINDING: &str = "__kybernFrameBinding";

/// One message from `frame.js` in the child frame's isolated world, delivered as `Runtime.bindingCalled`.
#[derive(Debug, PartialEq)]
pub enum FrameMessage {
    /// The frame's document or its history entry changed (`location.href`, read in the isolated world).
    Navigation(String),
    /// A trusted Escape the page left alone: give the keyboard back.
    Escape,
    /// A picker message (`src/pick/pickerScript.ts`): page data, re-bounded by the app.
    Body(serde_json::Map<String, serde_json::Value>),
}

/// Decodes a binding payload: `{"kybernFrame": "frame" | "escape" | {...}, "url": "..."}`. At most 64 KB.
pub fn frame_message(payload: &str) -> Option<FrameMessage> {
    if payload.len() > 65536 {
        return None;
    }
    let serde_json::Value::Object(mut data) = serde_json::from_str(payload).ok()? else { return None };
    match data.remove("kybernFrame")? {
        serde_json::Value::String(kind) if kind == "frame" => {
            let url = data.get("url")?.as_str()?;
            // The iframe's initial about:blank document is not a navigation the app shows.
            (!url.starts_with("about:")).then(|| FrameMessage::Navigation(url.to_owned()))
        }
        serde_json::Value::String(kind) if kind == "escape" => Some(FrameMessage::Escape),
        serde_json::Value::Object(body) => Some(FrameMessage::Body(body)),
        _ => None,
    }
}

/// Standard base64 (CDP `Page.captureScreenshot` data). Invalid characters are skipped.
pub fn base64_decode(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let digits: Vec<u8> = text.bytes().filter_map(value).collect();
    let mut out = Vec::with_capacity(digits.len() * 3 / 4);
    for chunk in digits.chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, d)| n | (*d as u32) << (18 - 6 * i));
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..chunk.len().saturating_sub(1)]);
    }
    out
}

/// Width and height from a PNG's IHDR.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((width, height))
}

/// A JSON string literal, which is also a valid JavaScript string literal.
pub fn js_string(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

#[cfg(test)]
mod tests {

    #[test]
    fn only_the_pinned_certificate_of_an_allowed_loopback_origin_is_accepted() {
        let pem = "-----BEGIN CERTIFICATE-----\nMIIBkTCB+wIJ\nAKHH\n-----END CERTIFICATE-----\n";
        let crlf = "-----BEGIN CERTIFICATE-----\r\nMIIBkTCB+wIJAKHH\r\n-----END CERTIFICATE-----";
        let trusted = vec![pem_body(pem)];
        let allowed = vec!["https://localhost:49152".to_string()];
        assert!(pinned_certificate("https://localhost:49152/src/main.ts", crlf, &trusted, &allowed));
        assert!(!pinned_certificate("https://localhost:49153/", pem, &trusted, &allowed), "another origin");
        assert!(!pinned_certificate("https://example.com:49152/", pem, &trusted, &["https://example.com:49152".into()]), "not loopback");
        assert!(!pinned_certificate("http://localhost:49152/", pem, &trusted, &["http://localhost:49152".into()]), "not https");
        assert!(!pinned_certificate(
            "https://localhost:49152/",
            "-----BEGIN CERTIFICATE-----\nOTHER\n-----END CERTIFICATE-----",
            &trusted,
            &allowed
        ));
        assert!(!pinned_certificate("https://localhost:49152/", "", &[String::new()], &allowed), "an empty pin trusts nothing");
        assert!(!pinned_certificate("https://localhost:49152/", pem, &[], &allowed));
    }

    use super::*;

    #[test]
    fn origins_follow_the_macos_rule() {
        assert_eq!(origin_of("http://127.0.0.1:4199/preview-files/x?y#z").as_deref(), Some("http://127.0.0.1:4199"));
        assert_eq!(origin_of("HTTPS://Example.COM:443/").as_deref(), Some("https://example.com"));
        assert_eq!(origin_of("http://example.com:80").as_deref(), Some("http://example.com"));
        assert_eq!(origin_of("http://[::1]:5173/").as_deref(), Some("http://[::1]:5173"));
        assert_eq!(origin_of("http://user:pw@example.com/"), None, "no credentials");
        assert_eq!(origin_of("file:///C:/x"), None);
        assert_eq!(origin_of("javascript:alert(1)"), None);
        let allowed = vec!["http://127.0.0.1:4199".to_owned()];
        assert!(frame_allows("http://127.0.0.1:4199/a", &allowed));
        assert!(frame_allows("about:blank", &allowed));
        assert!(!frame_allows("http://127.0.0.1:4200/a", &allowed));
        assert!(!frame_allows("data:text/html,x", &allowed));
    }

    #[test]
    fn wrapper_ids_are_one_path_segment() {
        assert_eq!(wrapper_url("visual-1"), "http://tauri.localhost/embedded/visual-1");
        assert_eq!(wrapper_url("a/b?c#d e"), "http://tauri.localhost/embedded/a%2Fb%3Fc%23d%20e");
    }

    #[test]
    fn snapshot_paths_stay_in_the_temp_folder() {
        let temp = std::env::temp_dir();
        let inside = temp.join("kybern-pick-1.png");
        assert!(snapshot_path_allowed(inside.to_str().unwrap(), &temp));
        assert!(!snapshot_path_allowed(temp.join("x.jpg").to_str().unwrap(), &temp));
        assert!(!snapshot_path_allowed(temp.join("..").join("x.png").to_str().unwrap(), &temp));
        assert!(!snapshot_path_allowed("relative.png", &temp));
    }

    #[test]
    fn snapshots_write_into_the_image_cache_or_temp() {
        let temp = std::env::temp_dir();
        let cache = temp.join("kybern-gpuix-test").join("images");
        let roots = [temp.as_path(), cache.as_path()];
        assert!(snapshot_path_allowed_in(cache.join("visual-frame-1.png").to_str().unwrap(), &roots));
        assert!(!snapshot_path_allowed_in(cache.join("..").join("..").join("x.png").to_str().unwrap(), &roots));
        assert!(!snapshot_path_allowed_in(cache.join("x.exe").to_str().unwrap(), &roots));
        assert!(!snapshot_path_allowed_in("relative.png", &[Path::new("relative")]), "roots must be absolute");
    }

    #[test]
    fn profiles_and_overrides() {
        let names = [profile_name(Some(7), 1), profile_name(Some(7), 2), profile_name(None, 3)];
        assert_eq!(names[0], "doc-7-1");
        assert_ne!(names[0], names[1], "a recreated document gets a fresh profile");
        for name in names.iter().chain([profile_name(Some(u64::MAX), u64::MAX)].iter()) {
            assert!(name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"#@$()+-_~.".contains(&b)), "{name}");
        }
        assert!(is_webview2_override("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS"));
        assert!(is_webview2_override("webview2_user_data_folder"));
        assert!(is_webview2_override("COREWEBVIEW2_FORCED_HOSTING_MODE"));
        assert!(!is_webview2_override("KYBERN_GPUIX_WEBVIEW2_DATA"));
        assert!(context_menu_keeps("copy") && context_menu_keeps("paste"));
        for gone in ["reload", "back", "print", "saveAs", "inspectElement", "share", "openLinkInNewWindow", "webCapture"] {
            assert!(!context_menu_keeps(gone), "{gone}");
        }
    }

    #[test]
    fn frame_messages() {
        assert_eq!(
            frame_message(r#"{"kybernFrame":"frame","url":"http://127.0.0.1:5173/a#b"}"#),
            Some(FrameMessage::Navigation("http://127.0.0.1:5173/a#b".into()))
        );
        assert_eq!(frame_message(r#"{"kybernFrame":"frame","url":"about:blank"}"#), None);
        assert_eq!(frame_message(r#"{"kybernFrame":"escape","url":"x"}"#), Some(FrameMessage::Escape));
        let Some(FrameMessage::Body(body)) = frame_message(r#"{"kybernFrame":{"kind":"pick","event":"anchor"}}"#) else { panic!() };
        assert_eq!(body.get("event").and_then(|v| v.as_str()), Some("anchor"));
        assert_eq!(frame_message(r#"{"kind":"pick"}"#), None);
        assert_eq!(frame_message(r#"{"kybernFrame":"other"}"#), None);
        assert_eq!(frame_message("not json"), None);
        assert_eq!(frame_message(&format!(r#"{{"kybernFrame":{{"x":"{}"}}}}"#, "y".repeat(70_000))), None);
    }

    #[test]
    fn decoders() {
        assert_eq!(base64_decode("aGVsbG8="), b"hello");
        assert_eq!(base64_decode("aGVsbG8h"), b"hello!");
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&600u32.to_be_bytes());
        png.extend_from_slice(&400u32.to_be_bytes());
        assert_eq!(png_size(&png), Some((600, 400)));
        assert_eq!(js_string("a\"b</script>"), r#""a\"b</script>""#);
    }
}
