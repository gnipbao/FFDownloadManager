//! Classify content pages, not entire platform domains. CDN files and arbitrary
//! HTTP endpoints belong to the normal downloader regardless of their suffix.
use reqwest::Url;

/// An explicit parser request may use a supported site's nonstandard page URL.
/// Resource subdomains are deliberately not included in this list.
pub(crate) fn parser_platform(url: &Url) -> Option<&'static str> {
    if crate::qishui::is_track_page(url) {
        return Some("Qishui Music");
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
        _ => None,
    }
}

pub(crate) fn platform(raw: &str) -> Option<&'static str> {
    let url = crate::filename::parse_url(raw).ok()?;
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
        _ => false,
    };
    content_page.then_some(platform)
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
