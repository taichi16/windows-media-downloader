//! 平台頁面 adapter 與直接 HLS 目標的安全解析。
//!
//! 小鴨影音與歐樂影院的頁面會以 JavaScript 變數攜帶播放資訊。本模組只
//! 解析該變數後的第一個 JSON object，不執行 JavaScript，也不讓頁面內容
//! 改寫 yt-dlp 參數。所有頁面、媒體與 HLS manifest 請求均使用明確設定的
//! native TLS client、禁止 redirect、禁止 proxy，並再次檢查 DNS 與連線
//! peer 位址。

use std::{
    collections::{HashSet, VecDeque},
    net::IpAddr,
    time::Duration,
};

use reqwest::{Client, Response};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::{
    error::AppError,
    security::{
        has_explicit_port, is_disallowed_ip, resolve_public_dns, validate_url_with_dns,
        DownloadRequest, Platform,
    },
};

const HTML_MAX_BYTES: usize = 1024 * 1024;
const HLS_MAX_BYTES: usize = 1024 * 1024;
const HLS_MAX_URIS: usize = 5000;
const HLS_MAX_LAYERS: usize = 3;
const MAX_URI_LENGTH: usize = 4096;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const PLATFORM_USER_AGENT: &str = "WindowsMediaDownloader/0.1";

const LITTLE_DUCK_MARKER: &str = "var player_data";
const OLEVOD_MARKER: &str = "var player_aaaa";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedTarget {
    platform: Platform,
    url: String,
}

impl ResolvedTarget {
    pub(crate) fn platform(&self) -> Platform {
        self.platform
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }
}

/// 將前端提供的來源 URL 解析成 manager 唯一可使用的 verified target。
///
/// YouTube、YouTube Music 與 Facebook 把已正規化的來源 URL 交給 yt-dlp；
/// 兩個直接 HLS 平台則先讀取公開頁面並把播放 URL 及其 HLS manifest 驗證完畢。
pub(crate) async fn resolve_target(request: &DownloadRequest) -> Result<ResolvedTarget, AppError> {
    let source = validate_url_with_dns(request.platform, &request.url)?;
    match request.platform {
        Platform::Youtube | Platform::YoutubeMusic | Platform::Facebook => Ok(ResolvedTarget {
            platform: source.platform,
            url: source.url,
        }),
        Platform::LittleDuck | Platform::Olevod => {
            let source_url = Url::parse(&source.url)?;
            let client = build_client()?;
            let body = bounded_get(&client, &source_url, HTML_MAX_BYTES).await?;
            let html = std::str::from_utf8(&body)
                .map_err(|_| AppError::Security("平台頁面不是 UTF-8 HTML".to_string()))?;
            let marker = match request.platform {
                Platform::LittleDuck => LITTLE_DUCK_MARKER,
                Platform::Olevod => OLEVOD_MARKER,
                Platform::Youtube | Platform::YoutubeMusic | Platform::Facebook => unreachable!(),
            };
            let player = parse_player_object(html.as_bytes(), marker)?;
            let target = parse_player_target(request.platform, &player)?;
            let target_url = target.to_string();
            inspect_hls(&client, request.platform, target).await?;
            Ok(ResolvedTarget {
                platform: request.platform,
                url: target_url,
            })
        }
    }
}

fn build_client() -> Result<Client, AppError> {
    Client::builder()
        // Do not inherit a machine proxy: the peer address check below must
        // describe the destination resolved by this client, not a proxy hop.
        .no_proxy()
        .use_native_tls()
        .user_agent(PLATFORM_USER_AGENT)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(HTTP_TIMEOUT)
        .read_timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|error| AppError::Security(format!("無法建立平台 TLS client：{error}")))
}

async fn bounded_get(client: &Client, url: &Url, max_bytes: usize) -> Result<Vec<u8>, AppError> {
    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|error| AppError::Security(format!("平台 HTTPS 請求失敗：{error}")))?;
    validate_response_peer(&response)?;
    if !response.status().is_success() {
        return Err(AppError::Security(format!(
            "平台回應不是 2xx：{}",
            response.status()
        )));
    }
    read_response_bounded(response, max_bytes).await
}

fn validate_response_peer(response: &Response) -> Result<(), AppError> {
    let peer = response
        .remote_addr()
        .ok_or_else(|| AppError::Security("HTTPS response 缺少 remote_addr".to_string()))?;
    if is_disallowed_ip(peer.ip()) {
        return Err(AppError::Security(
            "HTTPS peer 是私有、loopback、link-local、multicast 或保留位址".to_string(),
        ));
    }
    Ok(())
}

async fn read_response_bounded(
    mut response: Response,
    max_bytes: usize,
) -> Result<Vec<u8>, AppError> {
    let mut body = Vec::with_capacity(max_bytes.min(64 * 1024));
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| AppError::Security(format!("讀取 HTTPS response 失敗：{error}")))?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(AppError::Security(format!(
                "HTTPS response 超過 {} bytes 上限",
                max_bytes
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_player_object(html: &[u8], marker: &str) -> Result<Value, AppError> {
    let marker = marker.as_bytes();
    let offset = html
        .windows(marker.len())
        .position(|window| window == marker)
        .ok_or_else(|| AppError::Security("平台頁面缺少預期播放資料 marker".to_string()))?;
    let mut cursor = offset + marker.len();
    while html.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    if html.get(cursor) != Some(&b'=') {
        return Err(AppError::Security("播放資料 marker 格式錯誤".to_string()));
    }
    cursor += 1;
    while html.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    let mut deserializer = serde_json::Deserializer::from_slice(&html[cursor..]);
    let value = Value::deserialize(&mut deserializer)
        .map_err(|error| AppError::Security(format!("播放資料 JSON 無效：{error}")))?;
    if !value.is_object() {
        return Err(AppError::Security("播放資料必須是 JSON object".to_string()));
    }
    Ok(value)
}

fn parse_player_target(platform: Platform, player: &Value) -> Result<Url, AppError> {
    for field in ["encrypt", "trysee", "points"] {
        let value = player
            .get(field)
            .ok_or_else(|| AppError::Security(format!("播放資料缺少 {field}")))?;
        if !is_json_number_zero(value) {
            return Err(AppError::Security(format!(
                "播放資料 {field} 不是明確數字 0"
            )));
        }
    }
    let raw_url = player
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Security("播放資料缺少 url 字串".to_string()))?;
    validate_media_url(platform, raw_url)
}

fn is_json_number_zero(value: &Value) -> bool {
    value.as_number().is_some_and(|number| {
        number
            .as_f64()
            .is_some_and(|value| value.is_finite() && value == 0.0)
    })
}

fn validate_media_url(platform: Platform, raw: &str) -> Result<Url, AppError> {
    let raw = raw.trim();
    if raw.len() > MAX_URI_LENGTH {
        return Err(AppError::Security("播放 URL 超過長度限制".to_string()));
    }
    if has_explicit_port(raw) {
        return Err(AppError::Security(
            "播放與 HLS URL 不得包含連接埠".to_string(),
        ));
    }
    let parsed = Url::parse(raw)
        .map_err(|error| AppError::Security(format!("播放 URL 無法解析：{error}")))?;
    validate_hls_url(platform, &parsed, true)
}

fn validate_hls_url(platform: Platform, url: &Url, require_m3u8: bool) -> Result<Url, AppError> {
    if url.as_str().len() > MAX_URI_LENGTH {
        return Err(AppError::Security("HLS URL 超過長度限制".to_string()));
    }
    if url.scheme() != "https" {
        return Err(AppError::Security(
            "播放與 HLS URL 只允許 HTTPS".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
        return Err(AppError::Security(
            "播放與 HLS URL 不得包含 userinfo 或連接埠".to_string(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| AppError::Security("播放與 HLS URL 缺少主機名稱".to_string()))?
        .to_ascii_lowercase();
    if host.ends_with('.') || !host.is_ascii() || host.parse::<IpAddr>().is_ok() {
        return Err(AppError::Security(
            "播放與 HLS URL 主機名稱格式不受支援".to_string(),
        ));
    }
    if !media_host_allowed(platform, &host) {
        return Err(AppError::Security(format!(
            "HLS 主機不符合 {} 平台媒體允許清單",
            platform.as_str()
        )));
    }
    if require_m3u8
        && !url
            .path()
            .rsplit('/')
            .next()
            .is_some_and(|part| part.to_ascii_lowercase().ends_with(".m3u8"))
    {
        return Err(AppError::Security("播放 URL 必須是 m3u8".to_string()));
    }
    Ok(url.clone())
}

fn media_host_allowed(platform: Platform, host: &str) -> bool {
    match platform {
        Platform::LittleDuck => matches!(host, "v2.ppqrrs.com" | "v2.adfg8.vip"),
        Platform::Olevod => host == "europe.olemovienews.com",
        Platform::Youtube | Platform::YoutubeMusic | Platform::Facebook => false,
    }
}

#[derive(Debug, Default)]
struct HlsScan {
    uri_count: usize,
    uris: Vec<Url>,
}

async fn inspect_hls(client: &Client, platform: Platform, root: Url) -> Result<(), AppError> {
    let mut queue = VecDeque::from([(root, 0_usize)]);
    let mut visited = HashSet::new();
    let mut dns_hosts = HashSet::new();
    let mut scan = HlsScan::default();
    while let Some((url, depth)) = queue.pop_front() {
        if !visited.insert(url.to_string()) {
            continue;
        }
        let host = url
            .host_str()
            .ok_or_else(|| AppError::Security("HLS URL 缺少主機名稱".to_string()))?;
        resolve_hls_host_once(host, &mut dns_hosts)?;
        let body = bounded_get(client, &url, HLS_MAX_BYTES).await?;
        let first_new_uri = scan.uris.len();
        let parsed = parse_hls_manifest(platform, &url, &body, &mut scan)?;
        for uri in &scan.uris[first_new_uri..] {
            resolve_hls_host_once(
                uri.host_str()
                    .ok_or_else(|| AppError::Security("HLS URI 缺少主機名稱".to_string()))?,
                &mut dns_hosts,
            )?;
        }
        if !parsed.is_empty() && depth + 1 >= HLS_MAX_LAYERS {
            return Err(AppError::Security(
                "HLS manifest 巢狀層數超過 3 層".to_string(),
            ));
        }
        queue.extend(parsed.into_iter().map(|child| (child, depth + 1)));
    }
    Ok(())
}

fn resolve_hls_host_once(host: &str, seen: &mut HashSet<String>) -> Result<(), AppError> {
    let host = host.to_ascii_lowercase();
    if seen.insert(host.clone()) {
        resolve_public_dns(&host, 443)?;
    }
    Ok(())
}

fn parse_hls_manifest(
    platform: Platform,
    base: &Url,
    body: &[u8],
    scan: &mut HlsScan,
) -> Result<Vec<Url>, AppError> {
    let text = std::str::from_utf8(body)
        .map_err(|_| AppError::Security("HLS manifest 不是 UTF-8".to_string()))?;
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("#EXTM3U") {
        return Err(AppError::Security(
            "HLS manifest 必須以 #EXTM3U 開頭".to_string(),
        ));
    }
    let mut children = Vec::new();
    let mut pending_variant = false;
    let mut has_endlist = false;
    for raw_line in lines {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("#EXT-X-KEY") || line.starts_with("#EXT-X-SESSION-KEY") {
            return Err(AppError::Security(
                "HLS manifest 含有加密 key，拒絕處理".to_string(),
            ));
        }
        if line == "#EXT-X-ENDLIST" {
            has_endlist = true;
            continue;
        }
        if line.starts_with("#EXT-X-STREAM-INF") {
            pending_variant = true;
            for raw_uri in uri_attributes(line)? {
                let uri = resolve_hls_uri(platform, base, raw_uri, scan)?;
                children.push(uri);
            }
            continue;
        }
        if line.starts_with('#') {
            let is_child_attribute =
                line.starts_with("#EXT-X-MEDIA") || line.starts_with("#EXT-X-I-FRAMES-ONLY");
            for raw_uri in uri_attributes(line)? {
                let uri = resolve_hls_uri(platform, base, raw_uri, scan)?;
                if is_child_attribute {
                    children.push(uri);
                }
            }
            continue;
        }
        let uri = resolve_hls_uri(platform, base, line, scan)?;
        if pending_variant {
            children.push(uri);
            pending_variant = false;
        }
    }
    if pending_variant {
        return Err(AppError::Security(
            "HLS STREAM-INF 缺少 variant URI".to_string(),
        ));
    }
    if children.is_empty() && !has_endlist {
        return Err(AppError::Security(
            "HLS leaf manifest 必須有 #EXT-X-ENDLIST".to_string(),
        ));
    }
    Ok(children)
}

fn uri_attributes(line: &str) -> Result<Vec<&str>, AppError> {
    let mut result = Vec::new();
    let mut offset = 0;
    while let Some(found) = line[offset..].find("URI=") {
        let start = offset + found + "URI=".len();
        if line.as_bytes().get(start) != Some(&b'"') {
            return Err(AppError::Security(
                "HLS URI attribute 必須使用雙引號".to_string(),
            ));
        }
        let value_start = start + 1;
        let end = line[value_start..]
            .find('"')
            .map(|index| value_start + index)
            .ok_or_else(|| AppError::Security("HLS URI attribute 未封閉".to_string()))?;
        result.push(&line[value_start..end]);
        offset = end + 1;
    }
    Ok(result)
}

fn resolve_hls_uri(
    platform: Platform,
    base: &Url,
    raw: &str,
    scan: &mut HlsScan,
) -> Result<Url, AppError> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_URI_LENGTH {
        return Err(AppError::Security("HLS URI 超過長度限制或為空".to_string()));
    }
    if has_explicit_port(raw) {
        return Err(AppError::Security("HLS URI 不得包含連接埠".to_string()));
    }
    scan.uri_count = scan.uri_count.saturating_add(1);
    if scan.uri_count > HLS_MAX_URIS {
        return Err(AppError::Security(format!(
            "HLS URI 數量超過 {} 上限",
            HLS_MAX_URIS
        )));
    }
    let url = base
        .join(raw)
        .map_err(|error| AppError::Security(format!("HLS URI 無法解析：{error}")))?;
    let url = validate_hls_url(platform, &url, false)?;
    scan.uris.push(url.clone());
    Ok(url)
}

#[cfg(test)]
pub(crate) fn target_for_test(platform: Platform, url: &str) -> ResolvedTarget {
    ResolvedTarget {
        platform,
        url: url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(marker: &str, url: &str) -> String {
        format!(
            "prefix {marker}={{\"encrypt\":0,\"trysee\":0,\"points\":0,\"url\":{url:?}}}; suffix"
        )
    }

    fn media_url(platform: Platform) -> &'static str {
        match platform {
            Platform::LittleDuck => "https://v2.adfg8.vip/live/master.m3u8",
            Platform::Olevod => "https://europe.olemovienews.com/live/master.m3u8",
            Platform::Youtube | Platform::YoutubeMusic | Platform::Facebook => unreachable!(),
        }
    }

    fn scan_manifest(platform: Platform, manifest: &str) -> Result<Vec<Url>, AppError> {
        let base = Url::parse(media_url(platform)).expect("base URL");
        let mut scan = HlsScan::default();
        parse_hls_manifest(platform, &base, manifest.as_bytes(), &mut scan)
    }

    #[test]
    fn parses_both_page_markers_and_first_json_object() {
        for (platform, marker) in [
            (Platform::LittleDuck, LITTLE_DUCK_MARKER),
            (Platform::Olevod, OLEVOD_MARKER),
        ] {
            let html = player(marker, media_url(platform));
            let object = parse_player_object(html.as_bytes(), marker).expect("player JSON");
            let target = parse_player_target(platform, &object).expect("target");
            assert_eq!(target.as_str(), media_url(platform));
        }
    }

    #[test]
    fn rejects_missing_or_malformed_player_json() {
        assert!(parse_player_object(b"var other={};", LITTLE_DUCK_MARKER).is_err());
        assert!(parse_player_object(b"var player_data=not-json;", LITTLE_DUCK_MARKER).is_err());
        assert!(parse_player_object(b"var player_data=[];", LITTLE_DUCK_MARKER).is_err());
        assert!(
            parse_player_object(b"var player_data={\"encrypt\":0};", LITTLE_DUCK_MARKER).is_ok()
        );
        let object = parse_player_object(
            b"var player_data={\"encrypt\":0,\"trysee\":0,\"points\":0};",
            LITTLE_DUCK_MARKER,
        )
        .expect("JSON object");
        assert!(parse_player_target(Platform::LittleDuck, &object).is_err());
    }

    #[test]
    fn rejects_missing_or_nonzero_security_flags() {
        for field in ["encrypt", "trysee", "points"] {
            let html = format!(
                "var player_data={{\"encrypt\":{encrypt},\"trysee\":{trysee},\"points\":{points},\"url\":{url:?}}};",
                encrypt = if field == "encrypt" { 1 } else { 0 },
                trysee = if field == "trysee" { 1 } else { 0 },
                points = if field == "points" { 1 } else { 0 },
                url = media_url(Platform::LittleDuck)
            );
            let object = parse_player_object(html.as_bytes(), LITTLE_DUCK_MARKER).expect("JSON");
            assert!(
                parse_player_target(Platform::LittleDuck, &object).is_err(),
                "{field}"
            );
        }
        let html = player(LITTLE_DUCK_MARKER, "http://v2.adfg8.vip/a.m3u8");
        let object = parse_player_object(html.as_bytes(), LITTLE_DUCK_MARKER).expect("JSON");
        assert!(parse_player_target(Platform::LittleDuck, &object).is_err());
    }

    #[test]
    fn rejects_off_list_ip_and_non_m3u8_media_urls() {
        for url in [
            "https://10.0.0.1/a.m3u8",
            "https://evil.example/a.m3u8",
            "https://v2.adfg8.vip/a.mp4",
            "https://user:v@v2.adfg8.vip/a.m3u8",
            "https://v2.adfg8.vip:443/a.m3u8",
        ] {
            let html = player(LITTLE_DUCK_MARKER, url);
            let object = parse_player_object(html.as_bytes(), LITTLE_DUCK_MARKER).expect("JSON");
            assert!(
                parse_player_target(Platform::LittleDuck, &object).is_err(),
                "{url}"
            );
        }
        let olevod = player(
            OLEVOD_MARKER,
            "https://cdn.europe.olemovienews.com/live/master.m3u8",
        );
        let object = parse_player_object(olevod.as_bytes(), OLEVOD_MARKER).expect("JSON");
        assert!(parse_player_target(Platform::Olevod, &object).is_err());
    }

    #[test]
    fn rejects_oversized_player_and_hls_uris() {
        let long_path = "a".repeat(MAX_URI_LENGTH);
        let player_html = player(
            LITTLE_DUCK_MARKER,
            &format!("https://v2.adfg8.vip/{long_path}.m3u8"),
        );
        let object = parse_player_object(player_html.as_bytes(), LITTLE_DUCK_MARKER).expect("JSON");
        assert!(parse_player_target(Platform::LittleDuck, &object).is_err());

        let base = Url::parse(media_url(Platform::LittleDuck)).expect("base URL");
        let mut scan = HlsScan::default();
        let long_uri = format!("#EXTM3U\n#EXTINF:1,\n{}\n#EXT-X-ENDLIST", "a".repeat(5000));
        assert!(
            parse_hls_manifest(Platform::LittleDuck, &base, long_uri.as_bytes(), &mut scan)
                .is_err()
        );
    }

    #[test]
    fn rejects_hls_key_and_off_list_uri() {
        assert!(scan_manifest(
            Platform::LittleDuck,
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n"
        )
        .is_err());
        assert!(scan_manifest(
            Platform::LittleDuck,
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nhttps://evil.example/a.m3u8\n"
        )
        .is_err());
        assert!(scan_manifest(
            Platform::LittleDuck,
            "#EXTM3U\n#EXTINF:1,\nhttps://evil.example/segment.ts\n"
        )
        .is_err());
        assert!(scan_manifest(
            Platform::LittleDuck,
            "#EXTM3U\n#EXT-X-SESSION-KEY:METHOD=AES-128,URI=\"key\"\n"
        )
        .is_err());
    }

    #[test]
    fn requires_extm3u_and_endlist_for_leaf_manifest() {
        assert!(scan_manifest(Platform::LittleDuck, "#EXTINF:1,\nsegment.ts\n").is_err());
        assert!(scan_manifest(Platform::LittleDuck, "#EXTM3U\n#EXTINF:1,\nsegment.ts\n").is_err());
        assert!(scan_manifest(
            Platform::LittleDuck,
            "#EXTM3U\n#EXTINF:1,\nsegment.ts\n#EXT-X-ENDLIST\n"
        )
        .is_ok());
        assert!(scan_manifest(
            Platform::LittleDuck,
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nvariant.m3u8\n"
        )
        .is_ok());
    }

    #[test]
    fn accepts_relative_variant_and_counts_uri_limit() {
        let children = scan_manifest(
            Platform::LittleDuck,
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nvariant.m3u8\n",
        )
        .expect("relative variant");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].host_str(), Some("v2.adfg8.vip"));

        let mut scan = HlsScan::default();
        let base = Url::parse(media_url(Platform::LittleDuck)).expect("base URL");
        let mut manifest = String::from("#EXTM3U\n");
        for index in 0..=HLS_MAX_URIS {
            manifest.push_str(&format!("segment-{index}.ts\n"));
        }
        assert!(
            parse_hls_manifest(Platform::LittleDuck, &base, manifest.as_bytes(), &mut scan)
                .is_err()
        );
    }

    #[test]
    fn rejects_hls_layers_after_three() {
        let mut scan = HlsScan::default();
        let root = Url::parse(media_url(Platform::LittleDuck)).expect("root");
        let child = parse_hls_manifest(
            Platform::LittleDuck,
            &root,
            b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nchild.m3u8\n",
            &mut scan,
        )
        .expect("child");
        assert_eq!(child.len(), 1);
        let grandchild = parse_hls_manifest(
            Platform::LittleDuck,
            &child[0],
            b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\ngrandchild.m3u8\n",
            &mut scan,
        )
        .expect("grandchild");
        assert_eq!(grandchild.len(), 1);
    }

    fn live_error_summary(error: &crate::error::AppError) -> &'static str {
        match error {
            crate::error::AppError::InvalidInput(_) => "invalid-input",
            crate::error::AppError::Security(_) => "security-check",
            crate::error::AppError::Sidecar(_) => "sidecar-check",
            crate::error::AppError::Io(_) => "io",
            crate::error::AppError::Json(_) => "json",
            crate::error::AppError::Process(_) => "process",
            crate::error::AppError::Cancelled => "cancelled",
            crate::error::AppError::Internal(_) => "internal",
        }
    }

    #[tokio::test]
    #[ignore = "需要明確提供公開 live URL 並以 --ignored 執行"]
    async fn live_adapters_resolve_opt_in_sources() {
        let cases = [
            (Platform::LittleDuck, "WMD_LIVE_LITTLE_DUCK_URL"),
            (Platform::Olevod, "WMD_LIVE_OLEVOD_URL"),
        ];
        for (platform, variable) in cases {
            let source = std::env::var(variable)
                .unwrap_or_else(|_| panic!("{variable} 未設定，live adapter 測試無法執行"));
            let normalized_source = crate::security::validate_url_syntax(platform, &source)
                .unwrap_or_else(|_| panic!("{variable} 來源 URL 無效"))
                .url;
            let request = DownloadRequest {
                platform,
                url: source,
                mode: crate::security::DownloadMode::Video,
            };
            let target = resolve_target(&request).await.unwrap_or_else(|error| {
                panic!(
                    "{variable} live adapter 解析失敗（{}）",
                    live_error_summary(&error)
                )
            });
            assert!(
                target.platform() == platform,
                "live adapter 回傳的平台不一致"
            );
            let target_url = Url::parse(target.url()).expect("live adapter 目標 URL 無效");
            assert!(
                target_url.scheme() == "https",
                "live adapter 目標不是 HTTPS"
            );
            assert!(
                target.url() != normalized_source,
                "live adapter 目標不應等於來源頁 URL"
            );
        }
    }
}
