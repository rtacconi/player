//! Free metadata: [MusicBrainz](https://musicbrainz.org/doc/MusicBrainz_API) +
//! [Cover Art Archive](https://coverartarchive.org/) (no API keys).
//! Anonymous MusicBrainz clients should send ≤ ~1 request/s.

use anyhow::{Context, Result};
use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const USER_AGENT: &str = "AudiophilePlayer/0.1 (local-audio-player)";

static MB_THROTTLE: Mutex<Option<Instant>> = Mutex::new(None);

fn throttle_musicbrainz() {
    let mut last = MB_THROTTLE.lock().unwrap();
    let now = Instant::now();
    if let Some(t) = *last {
        let elapsed = now.saturating_duration_since(t);
        if elapsed < Duration::from_millis(1100) {
            std::thread::sleep(Duration::from_millis(1100) - elapsed);
        }
    }
    *last = Some(Instant::now());
}

fn blocking_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(25))
        .build()
        .context("reqwest client")
}

fn escape_lucene(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Best-effort release MBID from artist + album tags.
pub fn lookup_release_mbid(artist: &str, album: &str) -> Result<Option<String>> {
    let artist = artist.trim();
    let album = album.trim();
    if artist.is_empty() || album.is_empty() {
        return Ok(None);
    }

    throttle_musicbrainz();

    let q = format!(
        r#"artist:"{}" AND release:"{}""#,
        escape_lucene(artist),
        escape_lucene(album)
    );
    let url = format!(
        "https://musicbrainz.org/ws/2/release?query={}&fmt=json&limit=5",
        urlencoding::encode(&q)
    );

    let client = blocking_client()?;
    let resp = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .context("MusicBrainz search")?;

    if !resp.status().is_success() {
        log::warn!("MusicBrainz search HTTP {}", resp.status());
        return Ok(None);
    }

    let v: Value = resp.json().context("MusicBrainz JSON")?;
    let mbid = v["releases"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|r| r["id"].as_str())
        .map(String::from);
    Ok(mbid)
}

/// Title, credited artist, date (YYYY-MM-DD or year) when available.
pub fn fetch_release_summary(release_mbid: &str) -> Result<Option<(String, String, String)>> {
    throttle_musicbrainz();

    let url = format!(
        "https://musicbrainz.org/ws/2/release/{release_mbid}?inc=artist-credits&fmt=json",
    );

    let client = blocking_client()?;
    let resp = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .context("MusicBrainz release")?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let v: Value = resp.json().context("release JSON")?;
    let title = v["title"].as_str().unwrap_or("").to_string();
    let date = v["date"].as_str().unwrap_or("").to_string();
    let artist = v["artist-credit"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|ac| ac["name"].as_str())
        .unwrap_or("")
        .to_string();
    Ok(Some((title, artist, date)))
}

/// Front cover image bytes from Cover Art Archive (`front-500` JPEG/WebP).
pub fn fetch_cover_art_bytes(release_mbid: &str) -> Result<Option<Vec<u8>>> {
    let url = format!("https://coverartarchive.org/release/{release_mbid}/front-500");

    let client = blocking_client()?;
    let resp = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .context("Cover Art Archive")?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        log::warn!("CAA HTTP {}", resp.status());
        return Ok(None);
    }

    let bytes = resp.bytes().context("CAA body")?.to_vec();
    if bytes.len() < 100 {
        return Ok(None);
    }
    Ok(Some(bytes))
}
