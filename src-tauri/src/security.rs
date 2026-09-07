//! URL、平台及 DNS 啟動前安全閘門。
//!
//! 這裡刻意使用固定的允許清單，不接受前端傳入任意主機或 yt-dlp 參數。
//! DNS 解析只會在開始下載前再次執行；yt-dlp 可能自行處理 HTTP redirect，
//! 本 MVP 沒有可攔截其每一次 redirect 的獨立代理層，因此該限制必須對使用者
//! 文件化，並以啟動前解析驗證與允許主機清單降低風險。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    Youtube,
    YoutubeMusic,
    LittleDuck,
    Olevod,
    Facebook,
    Mmov,
}

impl Platform {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Youtube => "youtube",
            Self::YoutubeMusic => "youtube-music",
            Self::LittleDuck => "little-duck",
            Self::Olevod => "olevod",
            Self::Facebook => "facebook",
            Self::Mmov => "mmov",
        }
    }

    fn allows_host(self, host: &str) -> bool {
        match self {
            Self::Youtube => matches!(
                host,
                "youtube.com" | "www.youtube.com" | "m.youtube.com" | "youtu.be"
            ),
            Self::YoutubeMusic => matches!(host, "music.youtube.com" | "www.music.youtube.com"),
            Self::LittleDuck => host == "play.777tv.ai",
            Self::Olevod => matches!(host, "olevod.com" | "www.olevod.com"),
            Self::Facebook => host == "www.facebook.com",
            Self::Mmov => host == "hk.mmov.io",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatedUrl {
    pub platform: Platform,
    pub url: String,
    pub host: String,
}

/// 驗證不需要網路的 URL 結構。啟動前仍須呼叫 [`validate_url_with_dns`]。
pub fn validate_url_syntax(platform: Platform, raw: &str) -> Result<ValidatedUrl, AppError> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 4096 {
        return Err(AppError::InvalidInput("URL 為空或長度超過限制".to_string()));
    }
    if platform == Platform::Mmov
        && (!raw.is_ascii()
            || raw
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte == b'\\')
            || raw.contains('%')
            || has_mmov_dot_segment(raw))
    {
        return Err(AppError::Security(
            "MMOV URL 含有不允許的 encoding 或 path 字元".to_string(),
        ));
    }
    if has_explicit_port(raw) {
        return Err(AppError::Security("URL 不得指定連接埠".to_string()));
    }

    let mut parsed = Url::parse(raw)?;
    if parsed.scheme() != "https" {
        return Err(AppError::Security("只允許 HTTPS URL".to_string()));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(AppError::Security("URL 不得包含 userinfo".to_string()));
    }
    if parsed.port().is_some() {
        return Err(AppError::Security("URL 不得指定非標準連接埠".to_string()));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| AppError::Security("URL 缺少主機名稱".to_string()))?
        .to_ascii_lowercase();
    if host.ends_with('.') || !host.is_ascii() {
        return Err(AppError::Security("主機名稱格式不受支援".to_string()));
    }
    if host.parse::<IpAddr>().is_ok() {
        return Err(AppError::Security("不允許 IP literal".to_string()));
    }
    if !platform.allows_host(&host) {
        return Err(AppError::Security(format!(
            "主機名稱不符合 {} 平台允許清單",
            platform.as_str()
        )));
    }

    if platform == Platform::Facebook {
        return normalize_facebook_url(parsed);
    }
    if platform == Platform::Mmov {
        return normalize_mmov_url(parsed);
    }

    // Fragments are client-side navigation state and are not sent to the
    // origin server.  Remove them before the URL becomes a job identity so
    // equivalent resources cannot bypass active-job deduplication.
    parsed.set_fragment(None);

    Ok(ValidatedUrl {
        platform,
        url: parsed.to_string(),
        host,
    })
}

const FACEBOOK_HOST: &str = "www.facebook.com";
const FACEBOOK_ID_MAX_DIGITS: usize = 32;
const MMOV_HOST: &str = "hk.mmov.io";
const MMOV_ID_MAX_DIGITS: usize = 32;

fn normalize_facebook_url(mut parsed: Url) -> Result<ValidatedUrl, AppError> {
    // The approved PCB form carries `pcb.<post-id>` in the path.  Query
    // parameters can contain tracking, signed or access-control material,
    // so Facebook sources are deliberately query-free rather than attempting
    // to maintain a query allowlist.
    if parsed.query().is_some() {
        return Err(AppError::Security(
            "Facebook URL 不得包含 query 參數".to_string(),
        ));
    }

    let segments = parsed
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    let canonical_path =
        if segments.len() == 2 && segments[0] == "reel" && is_facebook_id(segments[1]) {
            format!("/reel/{}", segments[1])
        } else if segments.len() == 3
            && is_facebook_id(segments[0])
            && segments[1] == "videos"
            && is_facebook_id(segments[2])
        {
            format!("/{}/videos/{}", segments[0], segments[2])
        } else if segments.len() == 4
            && is_facebook_id(segments[0])
            && segments[1] == "videos"
            && segments[2].strip_prefix("pcb.").is_some_and(is_facebook_id)
            && is_facebook_id(segments[3])
        {
            format!("/{}/videos/{}", segments[0], segments[3])
        } else {
            return Err(AppError::Security(
                "Facebook URL 路徑不是允許的公開影片格式".to_string(),
            ));
        };

    parsed.set_path(&canonical_path);
    parsed.set_query(None);
    parsed.set_fragment(None);
    Ok(ValidatedUrl {
        platform: Platform::Facebook,
        url: parsed.to_string(),
        host: FACEBOOK_HOST.to_string(),
    })
}

fn is_facebook_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= FACEBOOK_ID_MAX_DIGITS
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn normalize_mmov_url(parsed: Url) -> Result<ValidatedUrl, AppError> {
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(AppError::Security(
            "MMOV URL 不得包含 query 或 fragment".to_string(),
        ));
    }
    // Reject encoded separators or encoded control material before
    // path-segment matching; canonical MMOV identities are plain ASCII IDs.
    if parsed.as_str().contains('%') {
        return Err(AppError::Security(
            "MMOV URL 不接受 percent encoding".to_string(),
        ));
    }
    let segments = parsed
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    let Some(work_id) = segments.get(1) else {
        return Err(AppError::Security(
            "MMOV URL 路徑不是允許的 VOD 格式".to_string(),
        ));
    };
    let Some(episode_stem) = segments
        .get(2)
        .and_then(|value| value.strip_suffix(".html"))
    else {
        return Err(AppError::Security(
            "MMOV URL 路徑不是允許的 VOD 格式".to_string(),
        ));
    };
    let Some((line_id, episode_id)) = episode_stem.split_once('-') else {
        return Err(AppError::Security("MMOV URL 缺少線路或集數 ID".to_string()));
    };
    if segments.len() != 3
        || segments[0] != "vodplay"
        || !is_mmov_id(work_id)
        || !is_mmov_id(line_id)
        || !is_mmov_id(episode_id)
    {
        return Err(AppError::Security(
            "MMOV URL 路徑不是允許的 VOD 格式".to_string(),
        ));
    }
    Ok(ValidatedUrl {
        platform: Platform::Mmov,
        url: format!("https://{MMOV_HOST}/vodplay/{work_id}/{line_id}-{episode_id}.html"),
        host: MMOV_HOST.to_string(),
    })
}

fn is_mmov_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MMOV_ID_MAX_DIGITS
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn has_mmov_dot_segment(raw: &str) -> bool {
    let path_end = raw.find(['?', '#']).unwrap_or(raw.len());
    raw[..path_end]
        .split('/')
        .any(|segment| matches!(segment, "." | ".."))
}

/// `url::Url` 會正規化 HTTPS 的顯式預設連接埠 443，故在 parse 前檢查
/// authority，確保「不得包含 port」也涵蓋 `https://host:443/`。
pub(crate) fn has_explicit_port(raw: &str) -> bool {
    let authority_start = raw
        .find("://")
        .map(|index| index + 3)
        .or_else(|| raw.strip_prefix("//").map(|_| 2));
    let Some(authority_start) = authority_start else {
        return false;
    };
    let authority_end = raw[authority_start..]
        .find(['/', '?', '#'])
        .map_or(raw.len(), |index| authority_start + index);
    let authority = &raw[authority_start..authority_end];
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, value)| value);
    if let Some(rest) = host_port.strip_prefix('[') {
        return rest
            .find(']')
            .and_then(|index| rest.get(index + 1..))
            .is_some_and(|suffix| suffix.starts_with(':'));
    }
    host_port.rsplit_once(':').is_some_and(|(_, suffix)| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// 驗證允許主機的 DNS 結果不能指向本機、私有、保留或特殊位址。
pub fn validate_url_with_dns(platform: Platform, raw: &str) -> Result<ValidatedUrl, AppError> {
    let validated = validate_url_syntax(platform, raw)?;
    resolve_public_dns(&validated.host, 443)?;
    Ok(validated)
}

/// 解析指定主機並拒絕所有非公開位址。
///
/// 這個 helper 同時供頁面 adapter 驗證媒體主機與 HTTP response 的
/// `remote_addr` 檢查使用；不會使用代理或接受空 DNS 結果。
pub fn resolve_public_dns(host: &str, port: u16) -> Result<Vec<SocketAddr>, AppError> {
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|err| AppError::Security(format!("DNS 解析失敗：{err}")))?
        .collect::<Vec<SocketAddr>>();
    if addresses.is_empty() {
        return Err(AppError::Security("主機沒有可用 DNS 結果".to_string()));
    }
    if addresses
        .iter()
        .any(|address| is_disallowed_ip(address.ip()))
    {
        return Err(AppError::Security(
            "DNS 結果包含私有、loopback、link-local、multicast 或保留位址".to_string(),
        ));
    }
    Ok(addresses)
}

pub fn is_disallowed_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(value) => is_disallowed_ipv4(value),
        IpAddr::V6(value) => is_disallowed_ipv6(value),
    }
}

fn is_disallowed_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    let first = octets[0];
    let second = octets[1];
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || first == 0 // 0.0.0.0/8
        || (first == 100 && (64..=127).contains(&second)) // RFC 6598
        || (first == 192 && second == 0) // IETF protocol assignments
        || (first == 192 && second == 0 && octets[2] == 2) // TEST-NET-1
        || (first == 192 && second == 0 && octets[2] == 9)
        || (first == 192 && second == 0 && octets[2] == 10)
        || (first == 192 && second == 0 && octets[2] == 170)
        || (first == 192 && second == 0 && octets[2] == 171)
        || (first == 198 && (18..=19).contains(&second)) // benchmark
        || (first == 198 && second == 51 && octets[2] == 100) // TEST-NET-2
        || (first == 203 && second == 0 && octets[2] == 113) // TEST-NET-3
        || first >= 240
}

fn is_disallowed_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    let first = segments[0];
    let second = segments[1];
    let is_documentation = first == 0x2001 && segments[1] == 0x0db8;
    let is_benchmark = first == 0x2001 && second == 0x0002;
    let is_unique_local = (first & 0xfe00) == 0xfc00;
    let is_link_local = (first & 0xffc0) == 0xfe80;
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || is_unique_local
        || is_link_local
        || is_documentation
        || is_benchmark
        || ip.to_ipv4().is_some_and(is_disallowed_ipv4)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DownloadMode {
    Audio,
    Video,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub platform: Platform,
    pub url: String,
    pub mode: DownloadMode,
}

pub fn validate_batch(requests: &[DownloadRequest]) -> Result<(), AppError> {
    if requests.is_empty() {
        return Err(AppError::InvalidInput("至少需要一筆下載工作".to_string()));
    }
    if requests.len() > 5 {
        return Err(AppError::InvalidInput("單批最多 5 筆下載工作".to_string()));
    }
    for request in requests {
        validate_url_syntax(request.platform, &request.url)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(url: &str) -> DownloadRequest {
        DownloadRequest {
            platform: Platform::Youtube,
            url: url.to_string(),
            mode: DownloadMode::Video,
        }
    }

    #[test]
    fn allows_expected_hosts_and_https_only() {
        assert!(
            validate_url_syntax(Platform::Youtube, "https://www.youtube.com/watch?v=x").is_ok()
        );
        assert!(validate_url_syntax(
            Platform::YoutubeMusic,
            "https://music.youtube.com/watch?v=x"
        )
        .is_ok());
        assert!(validate_url_syntax(Platform::LittleDuck, "https://play.777tv.ai/v/1").is_ok());
        assert!(validate_url_syntax(Platform::Olevod, "https://olevod.com/v/1").is_ok());
        assert!(validate_url_syntax(Platform::Olevod, "https://www.olevod.com/v/1").is_ok());
        assert!(validate_url_syntax(Platform::Olevod, "https://cdn.olevod.com/v/1").is_err());
        assert!(
            validate_url_syntax(Platform::Mmov, "https://hk.mmov.io/vodplay/123456/1-2.html")
                .is_ok()
        );
        assert!(
            validate_url_syntax(Platform::Youtube, "http://www.youtube.com/watch?v=x").is_err()
        );
    }

    #[test]
    fn mmov_normalizes_only_public_vod_paths() {
        let normalized = validate_url_syntax(
            Platform::Mmov,
            "  HTTPS://HK.MMOV.IO/vodplay/123456/01-002.html  ",
        )
        .expect("MMOV VOD URL");
        assert_eq!(normalized.host, "hk.mmov.io");
        assert_eq!(
            normalized.url,
            "https://hk.mmov.io/vodplay/123456/01-002.html"
        );
    }

    #[test]
    fn mmov_rejects_confusion_encoding_and_non_vod_urls() {
        for url in [
            "http://hk.mmov.io/vodplay/123/1-2.html",
            "//hk.mmov.io/vodplay/123/1-2.html",
            "https://hk.mmov.io:443/vodplay/123/1-2.html",
            "https://hk.mmov.io:65/vodplay/123/1-2.html",
            "https://hk.mmov.io./vodplay/123/1-2.html",
            "https://evil.hk.mmov.io/vodplay/123/1-2.html",
            "https://mmov.io/vodplay/123/1-2.html",
            "https://127.0.0.1/vodplay/123/1-2.html",
            "https://[::1]/vodplay/123/1-2.html",
            "https://hk.mmov.io/vodplay/123/1-2.html?token=secret",
            "https://hk.mmov.io/vodplay/123/1-2.html#fragment",
            "https://hk.mmov.io/vodplay%2F123/1-2.html",
            "https://%68k.mmov.io/vodplay/123/1-2.html",
            "https://hk.mmov.io/vodplay/123%2F456/1-2.html",
            "https://hk.mmov.io/vodplay/123/./1-2.html",
            "https://hk.mmov.io/vodplay/123/../1-2.html",
            "https://hk.mmov.io/vodplay/123/1-2.html\\suffix",
            "https://hk.mmov.io/vodplay/123/1-２.html",
            "https://hk.mmov.io/vodplay/123/1-2.HTML",
            "https://hk.mmov.io/vodplay/123/1-2.html/extra",
            "https://hk.mmov.io/vodplay/not-numeric/1-2.html",
            "https://hk.mmov.io/vodplay/123/not-numeric-2.html",
            "https://hk.mmov.io/vodplay/123/1-2-3.html",
            "https://hk.mmov.io/watch/123",
            "https://hk.mmov.io/vodplay/123/1-2.html/",
            "https://hk.mmov.io/vodplay/123456789012345678901234567890123/1-2.html",
        ] {
            assert!(
                validate_url_syntax(Platform::Mmov, url).is_err(),
                "must reject {url}"
            );
        }
    }

    #[test]
    fn facebook_accepts_public_numeric_shapes_and_drops_fragment() {
        let reel = validate_url_syntax(
            Platform::Facebook,
            " HTTPS://WWW.FACEBOOK.COM/reel/1584857163015601#client-only ",
        )
        .expect("Facebook reel URL");
        assert_eq!(reel.host, "www.facebook.com");
        assert_eq!(reel.url, "https://www.facebook.com/reel/1584857163015601");

        let pcb = validate_url_syntax(
            Platform::Facebook,
            "https://www.facebook.com/100064322940906/videos/pcb.1529534909200593/2093187601588241",
        )
        .expect("Facebook pcb URL");
        assert_eq!(
            pcb.url,
            "https://www.facebook.com/100064322940906/videos/2093187601588241"
        );

        let canonical = validate_url_syntax(Platform::Facebook, &pcb.url).expect("canonical URL");
        assert_eq!(canonical.url, pcb.url);
    }

    #[test]
    fn facebook_rejects_non_public_shapes_and_unknown_or_sensitive_queries() {
        for url in [
            "http://www.facebook.com/reel/123",
            "https://facebook.com/reel/123",
            "https://www.facebook.com.evil.example/reel/123",
            "https://user:pass@www.facebook.com/reel/123",
            "https://www.facebook.com:443/reel/123",
            "https://www.facebook.com/groups/123",
            "https://www.facebook.com/private/123",
            "https://www.facebook.com/live/123",
            "https://www.facebook.com/watch/?v=123",
            "https://www.facebook.com/profile.php?id=123",
            "https://www.facebook.com/reel/not-numeric",
            "https://www.facebook.com/reel/123/extra",
            "https://www.facebook.com/123/videos/not-a-video",
            "https://www.facebook.com/123/videos/pcb.not-numeric/456",
            "https://www.facebook.com/123/videos/pcb.456/789/extra",
            "https://www.facebook.com/reel/123?foo=bar",
            "https://www.facebook.com/reel/123?pcb=safe&foo=bar",
            "https://www.facebook.com/reel/123?pcb=one&pcb=two",
            "https://www.facebook.com/reel/123?pcb=%2Funsafe",
            "https://www.facebook.com/reel/123456789012345678901234567890123",
            "https://www.facebook.com/reel/123?pcb=feed",
            "https://www.facebook.com/reel/123?utm_source=share",
            "https://www.facebook.com/reel/123?access_token=secret",
        ] {
            assert!(
                validate_url_syntax(Platform::Facebook, url).is_err(),
                "must reject {url}"
            );
        }
    }

    #[test]
    fn rejects_host_confusion_and_userinfo() {
        assert!(
            validate_url_syntax(Platform::Youtube, "https://youtube.com.evil.example/v").is_err()
        );
        assert!(validate_url_syntax(Platform::Youtube, "https://user:pass@youtube.com/v").is_err());
        assert!(validate_url_syntax(Platform::Youtube, "https://127.0.0.1/v").is_err());
        assert!(validate_url_syntax(Platform::Youtube, "https://www.youtube.com:444/v").is_err());
        assert!(validate_url_syntax(Platform::Youtube, "https://www.youtube.com:443/v").is_err());
    }

    #[test]
    fn normalized_identity_ignores_host_case_outer_whitespace_and_fragment() {
        let with_fragment = validate_url_syntax(
            Platform::Youtube,
            "  HTTPS://WWW.YouTube.com/watch?v=identity&part=1#client-only  ",
        )
        .expect("normalized URL");
        let without_fragment = validate_url_syntax(
            Platform::Youtube,
            "https://www.youtube.com/watch?v=identity&part=1",
        )
        .expect("normalized URL");
        assert_eq!(with_fragment.url, without_fragment.url);
        assert_eq!(with_fragment.host, "www.youtube.com");
        assert_eq!(
            with_fragment.url,
            "https://www.youtube.com/watch?v=identity&part=1"
        );

        let query_order = validate_url_syntax(
            Platform::Youtube,
            "https://www.youtube.com/watch?b=2&a=1#ignored",
        )
        .expect("query order");
        assert_eq!(query_order.url, "https://www.youtube.com/watch?b=2&a=1");
    }

    #[test]
    fn rejects_special_ip_ranges() {
        for value in [
            "10.0.0.1",
            "172.16.1.1",
            "192.168.1.1",
            "100.64.0.1",
            "198.51.100.1",
            "2001:db8::1",
            "fc00::1",
            "fe80::1",
        ] {
            let ip = value.parse().expect("test IP");
            assert!(is_disallowed_ip(ip), "{value} must be blocked");
        }
    }

    #[test]
    fn caps_batch_at_five() {
        let five = (0..5)
            .map(|_| request("https://www.youtube.com/watch?v=x"))
            .collect::<Vec<_>>();
        assert!(validate_batch(&five).is_ok());
        let six = (0..6)
            .map(|_| request("https://www.youtube.com/watch?v=x"))
            .collect::<Vec<_>>();
        assert!(validate_batch(&six).is_err());
    }
}
