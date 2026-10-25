//! Classify content pages, not entire platform domains. CDN files and arbitrary
//! HTTP endpoints belong to the normal downloader regardless of their suffix.
use base64::Engine;
use reqwest::Url;

/// An explicit parser request may use a supported site's nonstandard page URL.
/// Resource subdomains are deliberately not included in this list.
pub(crate) fn parser_platform(url: &Url) -> Option<&'static str> {
    if crate::qishui::is_track_page(url) {
        return Some("Qishui Music");
    }
    let host = url.host_str()?;
    if cctv_host(host) && matches!(url.path().rsplit('.').next(), Some("shtml" | "html")) {
        return Some("CCTV");
    }
    if peertube_host(host) && is_peertube_page(url) {
        return Some("PeerTube");
    }
    if mastodon_host(host) && is_mastodon_page(url) {
        return Some("Mastodon");
    }
    if crate::platforms::tumblr_post(url).is_some() {
        return Some("Tumblr");
    }
    if bandcamp_artist(host) && bandcamp_track(url) {
        return Some("Bandcamp");
    }
    match url.host_str()? {
        "youtube.com"
        | "www.youtube.com"
        | "m.youtube.com"
        | "music.youtube.com"
        | "youtu.be"
        | "www.youtu.be"
        | "youtube-nocookie.com"
        | "www.youtube-nocookie.com" => Some("YouTube"),
        "bilibili.com" | "www.bilibili.com" | "m.bilibili.com" | "b23.tv" | "www.b23.tv"
        | "bilibili.tv" | "www.bilibili.tv" => Some("Bilibili"),
        "douyin.com" | "www.douyin.com" | "m.douyin.com" | "v.douyin.com" | "iesdouyin.com"
        | "www.iesdouyin.com" => Some("Douyin"),
        "xiaohongshu.com"
        | "www.xiaohongshu.com"
        | "m.xiaohongshu.com"
        | "xhslink.com"
        | "www.xhslink.com"
        | "xhslink.cn"
        | "www.xhslink.cn" => Some("Xiaohongshu"),
        "weixin.qq.com" | "channels.weixin.qq.com" => Some("WeChat Channels"),
        "tiktok.com" | "www.tiktok.com" | "m.tiktok.com" | "vm.tiktok.com" | "vt.tiktok.com" => {
            Some("TikTok")
        }
        "instagram.com" | "www.instagram.com" | "m.instagram.com" => Some("Instagram"),
        "x.com" | "www.x.com" | "mobile.x.com" | "m.x.com" | "twitter.com" | "www.twitter.com"
        | "mobile.twitter.com" | "m.twitter.com" => Some("X"),
        "reddit.com" | "www.reddit.com" | "old.reddit.com" | "new.reddit.com" | "m.reddit.com"
        | "redd.it" | "www.redd.it" => Some("Reddit"),
        "kuaishou.com"
        | "www.kuaishou.com"
        | "m.kuaishou.com"
        | "v.kuaishou.com"
        | "chenzhongtech.com"
        | "www.chenzhongtech.com"
        | "v.m.chenzhongtech.com" => Some("Kuaishou"),
        "weibo.com" | "www.weibo.com" | "m.weibo.cn" | "video.weibo.com" => Some("Weibo"),
        "acfun.cn" | "www.acfun.cn" | "m.acfun.cn" => Some("AcFun"),
        "vimeo.com" | "www.vimeo.com" | "player.vimeo.com" => Some("Vimeo"),
        "dailymotion.com" | "www.dailymotion.com" | "dai.ly" => Some("Dailymotion"),
        "streamable.com" | "www.streamable.com" => Some("Streamable"),
        "imgur.com" | "www.imgur.com" | "m.imgur.com" => Some("Imgur"),
        "i.imgur.com" if url.path().ends_with(".gifv") => Some("Imgur"),
        "ixigua.com" | "www.ixigua.com" | "m.ixigua.com" => Some("Ixigua"),
        "pearvideo.com" | "www.pearvideo.com" => Some("PearVideo"),
        "archive.org" | "www.archive.org"
            if url.path().starts_with("/details/") || url.path().starts_with("/embed/") =>
        {
            Some("Archive.org")
        }
        "ted.com" | "www.ted.com" | "embed.ted.com" | "embed-ssl.ted.com"
            if url.path().starts_with("/talks/") =>
        {
            Some("TED")
        }
        "commons.wikimedia.org" if wikimedia_file(url).is_some() => Some("WikimediaCommons"),
        "bsky.app" | "www.bsky.app" | "main.bsky.dev" => Some("Bluesky"),
        "medal.tv" | "www.medal.tv" => Some("Medal"),
        "9gag.com" | "www.9gag.com" => Some("9GAG"),
        "soundcloud.com" | "www.soundcloud.com" | "m.soundcloud.com" => Some("SoundCloud"),
        "tv.sohu.com" | "my.tv.sohu.com" if sohu_page(url) => Some("Sohu"),
        "haokan.baidu.com" => Some("Haokan"),
        "clyp.it" | "www.clyp.it" => Some("Clyp"),
        "audioboom.com" | "www.audioboom.com" => Some("AudioBoom"),
        "audius.co" | "www.audius.co" => Some("Audius"),
        "suno.com" | "www.suno.com" => Some("Suno"),
        "huya.com" | "www.huya.com" => Some("HuyaVideo"),
        "163.com" | "www.163.com" | "m.163.com" => Some("NetEaseNews"),
        "rumble.com" | "www.rumble.com" => Some("Rumble"),
        "odysee.com" | "www.odysee.com" | "lbry.tv" | "www.lbry.tv" => Some("Odysee"),
        "ok.ru" | "www.ok.ru" | "m.ok.ru" | "odnoklassniki.ru" | "www.odnoklassniki.ru" => {
            Some("OK.ru")
        }
        _ => None,
    }
}

pub fn platform(raw: &str) -> Option<&'static str> {
    let url = crate::validation::parse_url(raw).ok()?;
    if url.path().to_ascii_lowercase().ends_with(".m3u8") {
        return Some("HLS");
    }
    let platform = parser_platform(&url)?;
    let host = url.host_str()?;
    let path = url.path().strip_prefix('/').unwrap_or(url.path());
    let path: Vec<_> = path.strip_suffix('/').unwrap_or(path).split('/').collect();
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    };
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let content_page = match platform {
        "Qishui Music" => true,
        "Suno" => crate::platforms::suno_song_page(&url),
        "Clyp" => {
            matches!(path.as_slice(), [id] if id.len() == 8 && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
        }
        "AudioBoom" => matches!(path.as_slice(), ["posts" | "boos", slug] if audioboom_post(slug)),
        "Audius" => {
            matches!(path.as_slice(), [owner, slug] if token(owner) && owner.len() <= 100 && audius_track(slug))
        }
        "HuyaVideo" => {
            matches!(path.as_slice(), ["video", "play", file] if file.strip_suffix(".html").is_some_and(|id| id.len() <= 24 && number(id)))
        }
        "NetEaseNews" => {
            matches!(path.as_slice(), ["v", "video", file] if file.strip_suffix(".html").is_some_and(|id| id.starts_with('V') && id.len() > 1 && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric())))
        }
        "Rumble" | "Odysee" | "OK.ru" => crate::platforms::video_batch4_page(&url, platform),
        "CCTV" => true,
        "Ixigua" => matches!(path.as_slice(), [id] | ["video",id] if number(id)),
        "PearVideo" => {
            matches!(path.as_slice(), [id] if id.strip_prefix("video_").is_some_and(number))
        }
        "Archive.org" => matches!(path.as_slice(), ["details"|"embed",id,..] if token(id)),
        "TED" => matches!(path.as_slice(), ["talks",slug] | ["talks","lang",_,slug] if token(slug)),
        "WikimediaCommons" => wikimedia_file(&url).is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            [
                ".mp4", ".webm", ".ogv", ".ogg", ".oga", ".wav", ".flac", ".mp3", ".m4a", ".opus",
                ".mov", ".avi", ".mpeg", ".mpg",
            ]
            .iter()
            .any(|extension| lower.ends_with(extension))
        }),
        "Bluesky" => {
            matches!(path.as_slice(), ["profile",user,"post",id] if !user.is_empty() && token(id))
        }
        "PeerTube" => is_peertube_page(&url),
        "Mastodon" => is_mastodon_page(&url),
        "Tumblr" => crate::platforms::tumblr_post(&url).is_some(),
        "Medal" => {
            matches!(path.as_slice(), ["games",game,"clips",id] if token(game) && game.len() <= 128 && token(id) && id.len() <= 128)
        }
        "9GAG" => matches!(path.as_slice(), ["gag",id] if token(id) && id.len() <= 128),
        "Bandcamp" => bandcamp_track(&url),
        "SoundCloud" => {
            matches!(path.as_slice(), [user,slug] if token(user) && token(slug)
            && user.len() <= 200 && slug.len() <= 200
            && !matches!(*user,"search"|"discover"|"you"|"stations"|"settings")
            && !matches!(*slug,"tracks"|"sets"|"albums"|"reposts"|"likes"|"spotlight"|"comments"))
                && !url
                    .query_pairs()
                    .any(|(key, _)| matches!(key.as_ref(), "secret_token" | "oauth_token"))
        }
        "Sohu" => sohu_page(&url),
        "Haokan" => {
            url.path().trim_end_matches('/') == "/v"
                && url
                    .query_pairs()
                    .any(|(key, value)| key == "vid" && value.len() <= 24 && number(&value))
        }
        "YouTube" => match host {
            "youtu.be" | "www.youtu.be" => matches!(path.as_slice(), [id] if token(id)),
            "youtube-nocookie.com" | "www.youtube-nocookie.com" => {
                matches!(path.as_slice(), ["embed", id] if token(id))
            }
            _ => {
                url.path() == "/watch" && url.query_pairs().any(|(k, v)| k == "v" && !v.is_empty())
                    || matches!(path.as_slice(), ["shorts" | "embed" | "live", id] if token(id))
            }
        },
        "Bilibili" => match host {
            "b23.tv" | "www.b23.tv" => matches!(path.as_slice(), [id] if token(id)),
            "bilibili.tv" | "www.bilibili.tv" => {
                matches!(path.as_slice(), ["video", id] if token(id))
                    || matches!(path.as_slice(), [language, "video", id]
                        if language.len() == 2 && language.bytes().all(|b| b.is_ascii_lowercase()) && token(id))
            }
            _ => matches!(path.as_slice(), ["video", id] | ["bangumi", "play", id] if token(id)),
        },
        "Douyin" => match host {
            "v.douyin.com" => matches!(path.as_slice(), [id] if token(id)),
            "iesdouyin.com" | "www.iesdouyin.com" => {
                matches!(path.as_slice(), ["share", "video", id] if number(id))
            }
            _ => {
                matches!(path.as_slice(), ["video", id] if number(id))
                    || url
                        .query_pairs()
                        .any(|(k, v)| k == "modal_id" && number(&v))
            }
        },
        "Xiaohongshu" => match host {
            "xhslink.com" | "www.xhslink.com" | "xhslink.cn" | "www.xhslink.cn" => {
                matches!(path.as_slice(), [id] | ["a" | "o", id] if token(id))
            }
            _ => {
                matches!(path.as_slice(), ["explore", id] | ["discovery", "item", id] if token(id))
            }
        },
        "WeChat Channels" => match host {
            "weixin.qq.com" => matches!(path.as_slice(), ["sph", id] if token(id)),
            _ => url.path().starts_with("/finder-preview/pages/"),
        },
        "TikTok" => match host {
            "vm.tiktok.com" | "vt.tiktok.com" => matches!(path.as_slice(), [id] if token(id)),
            _ => {
                matches!(path.as_slice(), [user, "video", id] if user.starts_with('@') && user.len() > 1 && number(id))
                    || matches!(path.as_slice(), ["t", id] if token(id))
            }
        },
        "Instagram" => {
            matches!(path.as_slice(), ["p" | "reel" | "reels" | "tv", id] if token(id))
        }
        "X" => match path.as_slice() {
            [user, "status", id] => !user.is_empty() && number(id),
            ["i", "web", "status", id] => number(id),
            [user, "status", id, "video" | "photo", index] => {
                !user.is_empty() && number(id) && number(index)
            }
            ["i", "web", "status", id, "video" | "photo", index] => number(id) && number(index),
            _ => false,
        },
        "Reddit" => match host {
            "redd.it" | "www.redd.it" => matches!(path.as_slice(), [id] if token(id)),
            _ => {
                matches!(path.as_slice(), ["r", sub, "comments", id, ..] if !sub.is_empty() && token(id))
                    || matches!(path.as_slice(), ["comments", id, ..] if token(id))
            }
        },
        "Kuaishou" => match host {
            "v.kuaishou.com" => matches!(path.as_slice(), [id] if token(id)),
            _ => {
                matches!(path.as_slice(), ["short-video" | "photo" | "video", id] | ["fw", "photo", id] if token(id))
            }
        },
        "Weibo" => match host {
            "m.weibo.cn" => matches!(path.as_slice(), ["status" | "detail", id] if token(id)),
            "video.weibo.com" => {
                url.path().trim_end_matches('/') == "/show"
                    && url
                        .query_pairs()
                        .any(|(k, v)| k == "fid" && weibo_video_id(&v))
            }
            _ => match path.as_slice() {
                [user, id] => number(user) && token(id),
                ["tv", "show", id] => weibo_video_id(id),
                _ => false,
            },
        },
        "AcFun" => matches!(path.as_slice(), ["v", id] if id.strip_prefix("ac").is_some_and(|id| {
            let mut pieces = id.split('_');
            pieces.next().is_some_and(number) && pieces.next().is_none_or(number) && pieces.next().is_none()
        })),
        "Vimeo" => match path.as_slice() {
            [id] | ["video", id] => number(id),
            [id, hash] => {
                number(id) && hash.len() == 10 && hash.bytes().all(|b| b.is_ascii_hexdigit())
            }
            ["channels", channel, id] => token(channel) && number(id),
            ["groups", group, "videos", id] => token(group) && number(id),
            _ => false,
        },
        "Dailymotion" => match host {
            "dai.ly" => matches!(path.as_slice(), [id] if token(id)),
            _ => matches!(path.as_slice(), ["video", id] | ["embed", "video", id] if token(id)),
        },
        "Streamable" => {
            matches!(path.as_slice(), [id] | ["e" | "s", id] if token(id))
                || matches!(path.as_slice(), ["s", id, slug] if token(id) && token(slug))
        }
        "Imgur" => matches!(path.as_slice(), [id] if {
            let id = id.strip_suffix(".gifv").or_else(|| id.strip_suffix(".mp4")).unwrap_or(id);
            let id = id.rsplit('-').next().unwrap_or(id);
            id.len() >= 5 && id.len() <= 10 && id.bytes().all(|b| b.is_ascii_alphanumeric())
        }),
        _ => false,
    };
    content_page.then_some(platform)
}

fn cctv_host(host: &str) -> bool {
    ["cctv.com", "cntv.cn", "cntv.com"]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

pub(crate) fn mastodon_host(host: &str) -> bool {
    matches!(
        host,
        "mastodon.social"
            | "mastodon.online"
            | "mstdn.social"
            | "mas.to"
            | "fosstodon.org"
            | "infosec.exchange"
            | "tech.lgbt"
    )
}

/// Unknown instances require explicit HTML inspection before API discovery.
pub(crate) fn is_mastodon_page(url: &Url) -> bool {
    crate::platforms::mastodon_post(url).is_some()
}

fn bandcamp_artist(host: &str) -> bool {
    host.strip_suffix(".bandcamp.com").is_some_and(|artist| {
        !artist.is_empty()
            && artist.len() <= 63
            && !matches!(artist, "www" | "daily" | "discover")
            && artist
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn bandcamp_track(url: &Url) -> bool {
    let Some(slug) = url.path().trim_end_matches('/').strip_prefix("/track/") else {
        return false;
    };
    !slug.is_empty()
        && slug.len() <= 400
        && !slug.contains('/')
        && percent_encoding::percent_decode_str(slug)
            .decode_utf8()
            .is_ok_and(|decoded| !decoded.contains('/') && !decoded.chars().any(char::is_control))
}

fn sohu_page(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default();
    if !matches!(host, "tv.sohu.com" | "my.tv.sohu.com") {
        return false;
    }
    let mut path = url.path().trim_start_matches('/').to_owned();
    let mut mytv = host == "my.tv.sohu.com";
    if host == "tv.sohu.com" && path.starts_with("v/") {
        let Some(token) = path
            .strip_prefix("v/")
            .and_then(|path| path.strip_suffix(".html"))
        else {
            return false;
        };
        if token.len() > 512 {
            return false;
        }
        let Ok(bytes) = base64::engine::general_purpose::URL_SAFE
            .decode(token)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token))
        else {
            return false;
        };
        let Ok(decoded) = String::from_utf8(bytes) else {
            return false;
        };
        mytv = decoded.starts_with("us/") || decoded.starts_with("pl/");
        path = decoded;
    }
    let number = |raw: &str| {
        !raw.is_empty() && raw.len() <= 24 && raw.bytes().all(|byte| byte.is_ascii_digit())
    };
    match path.split('/').collect::<Vec<_>>().as_slice() {
        ["us" | "pl", owner, filename] if mytv && number(owner) => {
            filename.strip_suffix(".shtml").is_some_and(number)
        }
        [date, filename] if !mytv && date.len() == 8 && number(date) => filename
            .strip_prefix('n')
            .and_then(|name| name.strip_suffix(".shtml"))
            .is_some_and(number),
        _ => false,
    }
}

pub(crate) fn peertube_host(host: &str) -> bool {
    matches!(
        host,
        "framatube.org"
            | "video.blender.org"
            | "peertube.tv"
            | "peertube2.cpy.re"
            | "tilvids.com"
            | "diode.zone"
            | "makertube.net"
            | "media.fsfe.org"
            | "peertube.debian.social"
    )
}

/// Discover arbitrary PeerTube instances only after an explicit request fetches an HTML page.
pub(crate) fn is_peertube_page(url: &Url) -> bool {
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    let id = match parts.as_slice() {
        ["w", id] | ["videos", "watch" | "embed", id] => *id,
        _ => return false,
    };
    (id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        || (id.len() == 36
            && id.split('-').map(str::len).eq([8, 4, 4, 4, 12])
            && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
}

fn wikimedia_file(url: &Url) -> Option<String> {
    let title = if let Some(title) = url.path().strip_prefix("/wiki/") {
        percent_encoding::percent_decode_str(title)
            .decode_utf8()
            .ok()?
            .into_owned()
    } else if url.path() == "/w/index.php" {
        url.query_pairs()
            .find(|(key, _)| key == "title")?
            .1
            .into_owned()
    } else {
        return None;
    };
    let (namespace, name) = title.split_once(':')?;
    (namespace.eq_ignore_ascii_case("File") && !name.is_empty()).then(|| name.to_owned())
}

fn audioboom_post(slug: &str) -> bool {
    let count = slug.bytes().take_while(u8::is_ascii_digit).count();
    let rest = &slug[count..];
    (1..=20).contains(&count) && (rest.is_empty() || rest.starts_with('-') || rest == ".mp3")
}

fn audius_track(raw: &str) -> bool {
    percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .ok()
        .is_some_and(|slug| {
            !slug.is_empty()
                && slug.len() <= 500
                && !matches!(slug.as_ref(), "album" | "playlist")
                && !slug
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '/' | '\\'))
        })
}

fn weibo_video_id(id: &str) -> bool {
    id.split_once(':').is_some_and(|(kind, value)| {
        !kind.is_empty()
            && kind.bytes().all(|b| b.is_ascii_digit())
            && !value.is_empty()
            && value.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_and_backend_use_the_same_link_cases() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/link-routing.json")).unwrap();
        for case in cases {
            let url = case["url"].as_str().unwrap();
            assert_eq!(platform(url), case["platform"].as_str(), "{url}");
        }
    }
}
