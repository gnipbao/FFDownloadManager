#!/usr/bin/env python3
"""Build an offline reference inventory, never an extraction runtime.

Usage: python3 scripts/update-yt-dlp-coverage.py /path/supportedsites.md COMMIT_SHA
Obtain the input from the same pinned official yt-dlp commit first.
"""
import argparse
import hashlib
import json
import pathlib
import re

# A mapping means comparable single-item functionality, not extractor parity.
# Playlists, search, authentication and live variants remain separate pending rows.
PARTIAL = {
    "youtube": "YouTube", "YoutubeYtBe": "YouTube", "BiliBili": "Bilibili",
    "Douyin": "Douyin", "XiaoHongShu": "Xiaohongshu", "TikTok": "TikTok",
    "Instagram": "Instagram", "twitter": "X", "Reddit": "Reddit",
    "Weibo": "Weibo", "WeiboVideo": "Weibo", "AcFunVideo": "AcFun",
    "vimeo": "Vimeo", "dailymotion": "Dailymotion", "Streamable": "Streamable",
    "Imgur": "Imgur", "CCTV": "CCTV", "Ixigua": "Ixigua",
    "PearVideo": "PearVideo", "archive.org": "Archive.org",
    "TedTalk": "TED", "TedEmbed": "TED", "wikimedia.org": "WikimediaCommons",
    "Bluesky": "Bluesky", "PeerTube": "PeerTube", "Sohu": "Sohu",
    "SohuV": "Sohu", "MedalTV": "Medal", "9gag": "9GAG",
    "Tumblr": "Tumblr", "Bandcamp": "Bandcamp", "soundcloud": "SoundCloud",
    "huya:video": "HuyaVideo", "Rumble": "Rumble", "RumbleEmbed": "Rumble",
    "lbry": "Odysee", "Odnoklassniki": "OK.ru", "Clyp": "Clyp",
    "AudioBoom": "AudioBoom", "Audius": "Audius", "generic": "Generic",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=pathlib.Path)
    parser.add_argument("commit")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.commit):
        parser.error("commit must be an exact 40-character SHA")
    raw = args.source.read_bytes()
    names = re.findall(r"^\s*- \*\*([^*]+)\*\*", raw.decode("utf-8"), re.MULTILINE)
    if len(names) < 1000 or len(set(names)) != len(names):
        parser.error("input is not a complete, unique official extractor inventory")
    unknown = PARTIAL.keys() - set(names)
    if unknown:
        parser.error(f"mapped extractor names absent from this snapshot: {sorted(unknown)}")
    entries = [
        {"extractor": name, "status": "partial" if name in PARTIAL else "pending",
         **({"rust_adapter": PARTIAL[name]} if name in PARTIAL else {})}
        for name in names
    ]
    data = {
        "reference_commit": args.commit,
        "reference_url": f"https://github.com/yt-dlp/yt-dlp/blob/{args.commit}/supportedsites.md",
        "source_sha256": hashlib.sha256(raw).hexdigest(),
        "scope": "Reference inventory only. Partial means limited Rust single-item functionality, not yt-dlp parity or a successful live download. Pending is not implemented. Counts are extractors, not platforms.",
        "extractor_count": len(entries),
        "partial_count": sum(row["status"] == "partial" for row in entries),
        "pending_count": sum(row["status"] == "pending" for row in entries),
        "entries": entries,
    }
    out = pathlib.Path(__file__).resolve().parents[1] / "docs/yt-dlp-coverage.json"
    out.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
    print(f"{len(entries)} extractors: {data['partial_count']} partial, {data['pending_count']} pending")


if __name__ == "__main__":
    main()
