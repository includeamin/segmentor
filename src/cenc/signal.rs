//! DRM signalling: HLS `EXT-X-KEY` lines and DASH `ContentProtection` elements (TDD 0009).

use std::fmt::Write;

use base64::Engine;

use super::keys::{
    CLEARKEY, ContentKey, DrmSystem, Encryption, FAIRPLAY, PLAYREADY, WIDEVINE, hex_string,
    pssh_data, uuid_string,
};

fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// One `#{tag}` line per system HLS can name. Systems with no standard HLS form are DASH-only.
pub(crate) fn hls_key_lines(encryption: &Encryption, key: &ContentKey, tag: &str) -> String {
    let mut lines = String::new();
    for system in &encryption.systems {
        if let Some(attributes) = hls_attributes(system, Some(&key.key_id)) {
            push_key_line(&mut lines, tag, &attributes);
        }
    }
    lines
}

/// `EXT-X-SESSION-KEY` lines for every distinct key, so players can request licences early.
///
/// RFC 8216 4.3.4.5 forbids two session keys with the same `METHOD`, `URI`, `IV`, `KEYFORMAT`,
/// and `KEYFORMATVERSIONS`; `KEYID` does not tell them apart. Only Widevine's line depends on
/// the key, and only through `KEYID`, so each system's line is given once, for the first key.
pub(crate) fn hls_session_keys(encryption: &Encryption) -> String {
    hls_session_keys_for_sequence(std::iter::once(encryption))
}

/// Like [`hls_session_keys`], for every encryption used by any clip of a sequence (TDD 0009,
/// "Different keys per clip"), deduped by RFC 8216 identity across the whole sequence rather than
/// within one clip, so a clip that repeats an earlier clip's key and system does not repeat its
/// line, and a player sees every licence the sequence might need from the master playlist alone.
pub(crate) fn hls_session_keys_for_sequence<'a>(
    encryptions: impl Iterator<Item = &'a Encryption>,
) -> String {
    let mut lines = String::new();
    let mut seen: Vec<String> = Vec::new();
    for encryption in encryptions.flat_map(Encryption::encrypted_periods) {
        for key in encryption.distinct_keys() {
            for system in &encryption.systems {
                let Some(identity) = hls_attributes(system, None) else {
                    continue;
                };
                if seen.contains(&identity) {
                    continue;
                }
                seen.push(identity);
                if let Some(attributes) = hls_attributes(system, Some(&key.key_id)) {
                    push_key_line(&mut lines, "EXT-X-SESSION-KEY", &attributes);
                }
            }
        }
    }
    lines
}

/// `#EXT-X-KEY:METHOD=NONE`, RFC 8216's way to say that the Media Segments from here on are not
/// encrypted, needed when a sequence's clips move from encrypted to clear (TDD 0009, "Different
/// keys per clip"): an `EXT-X-KEY` tag otherwise applies to everything until the next one, so
/// without this a player would keep decrypting clear segments with the previous clip's key.
pub(crate) fn hls_key_none(tag: &str) -> String {
    format!("#{tag}:METHOD=NONE\n")
}

fn push_key_line(lines: &mut String, tag: &str, attributes: &str) {
    writeln!(lines, "#{tag}:METHOD=SAMPLE-AES,{attributes}")
        .expect("writing to a String cannot fail");
}

/// A system's HLS key attributes after `METHOD`, or `None` if HLS cannot name it. `KEYID` is
/// written only when `key_id` is given.
fn hls_attributes(system: &DrmSystem, key_id: Option<&[u8; 16]>) -> Option<String> {
    match system.system_id {
        FAIRPLAY => system.hls_uri.as_ref().map(|uri| {
            format!(r#"URI="{uri}",KEYFORMAT="com.apple.streamingkeydelivery",KEYFORMATVERSIONS="1""#)
        }),
        WIDEVINE => system.pssh.as_ref().map(|pssh| {
            let key_id = key_id
                .map(|key_id| format!(",KEYID=0x{}", hex_string(key_id)))
                .unwrap_or_default();
            format!(
                r#"URI="data:text/plain;base64,{}"{key_id},KEYFORMAT="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed",KEYFORMATVERSIONS="1""#,
                base64(pssh),
            )
        }),
        PLAYREADY => system.pssh.as_deref().and_then(pssh_data).map(|object| {
            format!(
                r#"URI="data:text/plain;charset=UTF-16;base64,{}",KEYFORMAT="com.microsoft.playready",KEYFORMATVERSIONS="1""#,
                base64(object)
            )
        }),
        CLEARKEY => system.license_url.as_ref().map(|url| {
            format!(r#"URI="{url}",KEYFORMAT="org.w3.clearkey",KEYFORMATVERSIONS="1""#)
        }),
        _ => None,
    }
}

/// The `ContentProtection` elements of one adaptation set, indented for its children.
pub(crate) fn dash_content_protection(encryption: &Encryption, key: &ContentKey) -> String {
    let mut xml = format!(
        "      <ContentProtection schemeIdUri=\"urn:mpeg:dash:mp4protection:2011\" value=\"cbcs\" cenc:default_KID=\"{}\" />\n",
        uuid_string(&key.key_id)
    );
    for system in &encryption.systems {
        writeln!(
            xml,
            "      <ContentProtection schemeIdUri=\"urn:uuid:{}\">",
            uuid_string(&system.system_id)
        )
        .expect("writing to a String cannot fail");
        if let Some(pssh) = &system.pssh {
            writeln!(xml, "        <cenc:pssh>{}</cenc:pssh>", base64(pssh))
                .expect("writing to a String cannot fail");
        }
        if let Some(url) = &system.license_url {
            writeln!(
                xml,
                "        <dashif:Laurl>{}</dashif:Laurl>",
                url.replace('&', "&amp;").replace('<', "&lt;")
            )
            .expect("writing to a String cannot fail");
        }
        xml.push_str("      </ContentProtection>\n");
    }
    xml
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cenc::WireEncryption;
    use crate::cenc::keys::{CLEARKEY, FAIRPLAY, PLAYREADY, WIDEVINE};

    fn pssh(system: &[u8; 16], data: &[u8]) -> String {
        use base64::Engine;
        let mut boxed = u32::try_from(32 + data.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        boxed.extend_from_slice(b"pssh");
        boxed.extend_from_slice(&[0; 4]);
        boxed.extend_from_slice(system);
        boxed.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        boxed.extend_from_slice(data);
        base64::engine::general_purpose::STANDARD.encode(boxed)
    }

    fn encryption() -> Encryption {
        let json = format!(
            r#"{{"scheme":"cbcs","keys":[{{"key_id":"0123456789abcdef0123456789abcdef","key":"00112233445566778899aabbccddeeff"}}],
            "systems":[
              {{"system_id":"edef8ba9-79d6-4ace-a3c8-27dcd51d21ed","pssh":"{}","license_url":"https://l.example.net/wv?a=1&b=2"}},
              {{"system_id":"94ce86fb-07ff-4f43-adb8-93d2fa968ca2","hls_uri":"skd://asset-1"}},
              {{"system_id":"9a04f079-9840-4286-ab92-e65be0885f95","pssh":"{}"}},
              {{"system_id":"e2719d58-a985-b3c9-781a-b030af78d30e","license_url":"https://l.example.net/ck"}},
              {{"system_id":"11111111-2222-3333-4444-555555555555"}}
            ]}}"#,
            pssh(&WIDEVINE, b"wv"),
            pssh(&PLAYREADY, b"pr")
        );
        serde_json::from_str::<WireEncryption>(&json)
            .unwrap()
            .validate()
            .unwrap()
    }

    #[test]
    fn hls_names_each_system_by_its_key_format() {
        let encryption = encryption();
        let key = encryption.key_for(crate::media::TrackKind::Video);

        let lines = hls_key_lines(&encryption, key, "EXT-X-KEY");

        assert_eq!(
            lines.lines().count(),
            4,
            "the unknown system has no HLS form: {lines}"
        );
        assert!(lines.contains(r#"#EXT-X-KEY:METHOD=SAMPLE-AES,URI="skd://asset-1",KEYFORMAT="com.apple.streamingkeydelivery",KEYFORMATVERSIONS="1""#), "{lines}");
        assert!(lines.contains(&format!(r#"URI="data:text/plain;base64,{}",KEYID=0x0123456789abcdef0123456789abcdef,KEYFORMAT="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed""#, pssh(&WIDEVINE, b"wv"))), "{lines}");
        assert!(lines.contains(r#"URI="data:text/plain;charset=UTF-16;base64,cHI=",KEYFORMAT="com.microsoft.playready""#), "the PlayReady object, not the box: {lines}");
        assert!(
            lines.contains(r#"URI="https://l.example.net/ck",KEYFORMAT="org.w3.clearkey""#),
            "{lines}"
        );
        let _ = (FAIRPLAY, CLEARKEY);
    }

    /// RFC 8216 4.3.4.5: no two `EXT-X-SESSION-KEY` lines may share `METHOD`, `URI`, `IV`,
    /// `KEYFORMAT`, and `KEYFORMATVERSIONS`, so split keys must not repeat the per-system lines.
    #[test]
    fn split_keys_give_each_session_key_line_once() {
        let json = format!(
            r#"{{"scheme":"cbcs","keys":[
              {{"tracks":"video","key_id":"0123456789abcdef0123456789abcdef","key":"00112233445566778899aabbccddeeff"}},
              {{"tracks":"audio","key_id":"fedcba9876543210fedcba9876543210","key":"ffeeddccbbaa99887766554433221100"}}],
            "systems":[
              {{"system_id":"edef8ba9-79d6-4ace-a3c8-27dcd51d21ed","pssh":"{}"}},
              {{"system_id":"9a04f079-9840-4286-ab92-e65be0885f95","pssh":"{}"}},
              {{"system_id":"e2719d58-a985-b3c9-781a-b030af78d30e","license_url":"https://l.example.net/ck"}}
            ]}}"#,
            pssh(&WIDEVINE, b"wv"),
            pssh(&PLAYREADY, b"pr")
        );
        let encryption = serde_json::from_str::<WireEncryption>(&json)
            .unwrap()
            .validate()
            .unwrap();

        let lines = hls_session_keys(&encryption);

        let identities: Vec<String> = lines
            .lines()
            .map(|line| {
                line.split(',')
                    .filter(|attribute| !attribute.starts_with("KEYID="))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect();
        let mut unique = identities.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), identities.len(), "{lines}");
        assert_eq!(identities.len(), 3, "one line per system: {lines}");
        assert!(
            lines.contains("KEYID=0x0123456789abcdef0123456789abcdef"),
            "Widevine keeps the first (video) key's line: {lines}"
        );
    }

    #[test]
    fn identical_split_keys_are_one_session_key() {
        let key = r#""key_id":"0123456789abcdef0123456789abcdef","key":"00112233445566778899aabbccddeeff""#;
        let json = format!(
            r#"{{"scheme":"cbcs","keys":[{{"tracks":"video",{key}}},{{"tracks":"audio",{key}}}]}}"#
        );
        let encryption = serde_json::from_str::<WireEncryption>(&json)
            .unwrap()
            .validate()
            .unwrap();

        assert_eq!(encryption.distinct_keys().len(), 1);
        assert_eq!(encryption.key_ids().len(), 1);
    }

    #[test]
    fn dash_lists_the_scheme_then_every_system() {
        let encryption = encryption();
        let key = encryption.key_for(crate::media::TrackKind::Video);

        let xml = dash_content_protection(&encryption, key);

        assert!(xml.contains(r#"<ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cbcs" cenc:default_KID="01234567-89ab-cdef-0123-456789abcdef" />"#), "{xml}");
        assert_eq!(
            xml.matches("<ContentProtection schemeIdUri=\"urn:uuid:")
                .count(),
            5,
            "{xml}"
        );
        assert!(
            xml.contains("<dashif:Laurl>https://l.example.net/wv?a=1&amp;b=2</dashif:Laurl>"),
            "escaped: {xml}"
        );
        assert!(
            xml.contains(&format!(
                "<cenc:pssh>{}</cenc:pssh>",
                pssh(&WIDEVINE, b"wv")
            )),
            "{xml}"
        );
    }
}
