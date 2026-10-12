use std::borrow::Cow;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures::TryStreamExt;
use serde_json::{Value, json};
use ytmapi_rs::auth::BrowserToken;
use ytmapi_rs::common::{LikeStatus, PlaylistID, SetVideoID, VideoID, YoutubeID};
use ytmapi_rs::parse::{ParseFrom, SearchResultVideo};
use ytmapi_rs::query::playlist::{DuplicateHandlingMode, PrivacyStatus};
use ytmapi_rs::query::{
    AddPlaylistItemsQuery, CreatePlaylistQuery, GetPlaylistTracksQuery, PostMethod, PostQuery,
    Query,
};
use ytmapi_rs::{ProcessedResult, YtMusic};

pub type Yt = Arc<YtMusic<BrowserToken>>;

#[derive(Clone, Debug)]
pub struct Playlist {
    pub id: String,
    pub title: String,
    pub count: String,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub pos: usize,
    pub video_id: String,
    pub set_id: String,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub duration: String,
    pub added: u64,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub video_id: String,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub duration: String,
}

pub async fn connect(auth: &std::path::Path) -> Result<Yt> {
    let headers: Value = serde_json::from_str(
        &std::fs::read_to_string(auth)
            .with_context(|| format!("no login at {}", auth.display()))?,
    )?;
    let cookie = headers["cookie"]
        .as_str()
        .context("browser.json has no cookie header")?;
    Ok(Arc::new(YtMusic::from_cookie(cookie).await?))
}

/// Parse request headers copied from Firefox ("Copy Request Headers") or Chrome, the way ytmusicapi's `setup_browser` does.
pub fn parse_headers(raw: &str) -> Result<serde_json::Map<String, Value>> {
    let mut h = serde_json::Map::new();
    let mut pending: Option<String> = None;
    for line in raw.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(key) = pending.take() {
            h.insert(key, Value::String(line.into()));
        } else if let Some((k, v)) = line
            .split_once(": ")
            .filter(|(k, _)| !k.is_empty() && !k.starts_with(':'))
        {
            h.insert(k.to_lowercase(), Value::String(v.into()));
        } else if let Some(k) = line
            .strip_suffix(':')
            .filter(|k| !k.starts_with(':') && !k.contains(' '))
        {
            pending = Some(k.to_lowercase());
        }
    }
    let missing: Vec<&str> = ["cookie", "x-goog-authuser"]
        .into_iter()
        .filter(|k| !h.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        bail!(
            "missing {} in the pasted headers; copy them from a POST /browse request on music.youtube.com while logged in",
            missing.join(", ")
        );
    }
    h.retain(|k, _| {
        !k.starts_with("sec")
            && !["host", "content-length", "accept-encoding"].contains(&k.as_str())
    });
    for (k, v) in [
        (
            "user-agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:88.0) Gecko/20100101 Firefox/88.0",
        ),
        ("accept", "*/*"),
        ("accept-encoding", "gzip, deflate"),
        ("content-type", "application/json"),
        ("content-encoding", "gzip"),
        ("origin", "https://music.youtube.com"),
    ] {
        h.insert(k.into(), Value::String(v.into()));
    }
    Ok(h)
}

/// Check pasted headers against YouTube Music, then save them (owner-only) where both the TUI and ytm.py read them.
pub async fn login(raw: &str, auth: &std::path::Path) -> Result<Yt> {
    use std::os::unix::fs::OpenOptionsExt;
    let headers = parse_headers(raw)?;
    let yt: Yt = Arc::new(
        YtMusic::from_cookie(headers["cookie"].as_str().unwrap_or_default())
            .await
            .context("YouTube Music rejected the cookie")?,
    );
    yt.get_library_playlists()
        .await
        .context("logged in, but couldn't read your library; the cookie may be expired")?;
    if let Some(dir) = auth.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(auth)?;
    std::io::Write::write_all(&mut f, serde_json::to_string_pretty(&headers)?.as_bytes())?;
    Ok(yt)
}

fn pid(id: &str) -> PlaylistID<'_> {
    PlaylistID::from_raw(id)
}

pub async fn playlists(yt: &Yt) -> Result<Vec<Playlist>> {
    let mut out: Vec<Playlist> = yt
        .get_library_playlists()
        .await?
        .into_iter()
        .map(|p| {
            let raw = p.playlist_id.get_raw();
            Playlist {
                id: raw.strip_prefix("VL").unwrap_or(raw).to_string(),
                title: p.title,
                count: p
                    .tracks
                    .split_whitespace()
                    .next()
                    .filter(|n| n.chars().all(|c| c.is_ascii_digit() || c == ','))
                    .unwrap_or("?")
                    .to_string(),
            }
        })
        .collect();
    if !out.iter().any(|p| p.id == "LM") {
        out.insert(
            0,
            Playlist {
                id: "LM".into(),
                title: "Liked Music".into(),
                count: "?".into(),
            },
        );
    }
    Ok(out)
}

fn renderers<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if k == "musicResponsiveListItemRenderer" {
                    out.push(x);
                } else {
                    renderers(x, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| renderers(x, out)),
        _ => {}
    }
}

fn runs(v: &Value) -> String {
    match v["runs"].as_array() {
        Some(r) => r.iter().filter_map(|x| x["text"].as_str()).collect(),
        None => v["simpleText"].as_str().unwrap_or_default().to_string(),
    }
}

fn parse_tracks(pages: &[String]) -> Result<Vec<Track>> {
    let mut out = Vec::new();
    for page in pages {
        let json: Value = serde_json::from_str(page)?;
        let mut rows = Vec::new();
        renderers(&json, &mut rows);
        for r in rows {
            let data = &r["playlistItemData"];
            let Some(video_id) = data["videoId"].as_str() else {
                continue;
            };
            let col = |i: usize| {
                runs(&r["flexColumns"][i]["musicResponsiveListItemFlexColumnRenderer"]["text"])
            };
            out.push(Track {
                pos: out.len(),
                video_id: video_id.into(),
                set_id: data["playlistSetVideoId"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                title: col(0),
                artists: col(1),
                album: col(2),
                duration: runs(
                    &r["fixedColumns"][0]["musicResponsiveListItemFixedColumnRenderer"]["text"],
                ),
                added: data["voteSortValue"]
                    .as_u64()
                    .or_else(|| data["voteSortValue"].as_str()?.parse().ok())
                    .unwrap_or(0),
            });
        }
    }
    Ok(out)
}

pub async fn tracks(yt: &Yt, id: &str) -> Result<Vec<Track>> {
    let browse = format!("VL{id}");
    let q = GetPlaylistTracksQuery::new(pid(&browse));
    let pages: Vec<String> = yt.raw_json_stream(&q).try_collect().await?;
    parse_tracks(&pages)
}

pub async fn search(yt: &Yt, q: &str) -> Result<Vec<Hit>> {
    Ok(yt
        .search_songs(q)
        .await?
        .into_iter()
        .map(|s| Hit {
            video_id: s.video_id.get_raw().into(),
            title: s.title,
            artists: s.artist,
            album: s.album.map(|a| a.name).unwrap_or_default(),
            duration: s.duration,
        })
        .collect())
}

async fn search_videos(yt: &Yt, q: &str) -> Result<Vec<Hit>> {
    Ok(yt
        .search_videos(q)
        .await?
        .into_iter()
        .filter_map(|v| match v {
            SearchResultVideo::Video {
                title,
                channel_name,
                video_id,
                length,
                ..
            } => Some(Hit {
                video_id: video_id.get_raw().into(),
                title,
                artists: channel_name,
                album: String::new(),
                duration: length,
            }),
            _ => None,
        })
        .collect())
}

pub async fn create(yt: &Yt, title: &str, description: &str, privacy: &str) -> Result<String> {
    let privacy = match privacy {
        "PUBLIC" => PrivacyStatus::Public,
        "UNLISTED" => PrivacyStatus::Unlisted,
        _ => PrivacyStatus::Private,
    };
    let desc = Some(description).filter(|d| !d.is_empty());
    let id = yt
        .create_playlist(CreatePlaylistQuery::new(title, desc, privacy))
        .await?;
    let raw = id.get_raw();
    Ok(raw.strip_prefix("VL").unwrap_or(raw).to_string())
}

pub async fn add(yt: &Yt, id: &str, video_ids: &[String]) -> Result<()> {
    for chunk in video_ids.chunks(50) {
        let q = AddPlaylistItemsQuery::new_from_videos(
            pid(id),
            chunk.iter().map(|v| VideoID::from_raw(v.as_str())),
            DuplicateHandlingMode::Unhandled,
        );
        yt.query(q).await?;
    }
    Ok(())
}

pub async fn remove(yt: &Yt, id: &str, set_ids: &[String]) -> Result<()> {
    yt.remove_playlist_items(
        pid(id),
        set_ids.iter().map(|s| SetVideoID::from_raw(s.as_str())),
    )
    .await?;
    Ok(())
}

pub async fn delete(yt: &Yt, id: &str) -> Result<()> {
    yt.delete_playlist(pid(id)).await?;
    Ok(())
}

pub async fn rate(yt: &Yt, video_id: &str, like: bool) -> Result<()> {
    let status = if like {
        LikeStatus::Liked
    } else {
        LikeStatus::Indifferent
    };
    yt.rate_song(VideoID::from_raw(video_id), status).await?;
    Ok(())
}

struct Reorder {
    id: String,
    set_ids: Vec<String>,
}

#[derive(Debug)]
struct Done;

impl ParseFrom<Reorder> for Done {
    fn parse_from(_: ProcessedResult<Reorder>) -> ytmapi_rs::Result<Self> {
        Ok(Done)
    }
}

impl Query<BrowserToken> for Reorder {
    type Output = Done;
    type Method = PostMethod;
}

impl PostQuery for Reorder {
    fn header(&self) -> serde_json::Map<String, Value> {
        let actions: Vec<Value> = self
            .set_ids
            .iter()
            .map(|s| json!({"action": "ACTION_MOVE_VIDEO_BEFORE", "setVideoId": s}))
            .collect();
        let Value::Object(m) = json!({"playlistId": self.id, "actions": actions}) else {
            unreachable!()
        };
        m
    }
    fn params(&self) -> Vec<(&str, Cow<'_, str>)> {
        vec![]
    }
    fn path(&self) -> &str {
        "browse/edit_playlist"
    }
}

pub async fn reorder(yt: &Yt, id: &str, set_ids: &[String]) -> Result<()> {
    for chunk in set_ids.chunks(100) {
        let raw = yt
            .raw_json_query(Reorder {
                id: id.into(),
                set_ids: chunk.to_vec(),
            })
            .await?;
        if !raw.contains("STATUS_SUCCEEDED") {
            bail!("YouTube Music rejected the reorder");
        }
    }
    Ok(())
}

pub fn parse_line(line: &str) -> (String, String) {
    let (artists, title) = line.split_once(" - ").unwrap_or((line, ""));
    (
        artists.split(',').next().unwrap_or_default().trim().into(),
        title.trim().into(),
    )
}

pub fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Best hit for an "Artist - Title" line, and whether it's suspect (no title match found).
pub async fn match_line(yt: &Yt, line: &str) -> Result<Option<(Hit, bool)>> {
    let (artist, song) = parse_line(line);
    let core = song
        .split(" - ")
        .next()
        .unwrap_or_default()
        .split(" (")
        .next()
        .unwrap_or_default();
    let want = if norm(core).is_empty() {
        norm(&song)
    } else {
        norm(core)
    };
    let query = format!("{artist} {song}");
    let cleaned: String = query
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() || c == '\'' || c == '_' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let mut first = None;
    for (i, q) in [query.as_str(), cleaned.as_str(), query.as_str()]
        .into_iter()
        .enumerate()
    {
        if i == 1 && cleaned == query {
            continue;
        }
        let hits = if i < 2 {
            search(yt, q).await?
        } else {
            search_videos(yt, q).await?
        };
        if let Some(h) = hits.iter().take(3).find(|h| norm(&h.title).contains(&want)) {
            return Ok(Some((h.clone(), false)));
        }
        first = first.or_else(|| hits.into_iter().next());
    }
    Ok(first.map(|h| (h, true)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines_and_rows() {
        assert_eq!(
            parse_line("Korn, Fred Durst - All In the Family (feat. Fred Durst)"),
            ("Korn".into(), "All In the Family (feat. Fred Durst)".into())
        );
        assert_eq!(
            parse_line("Nine Inch Nails - Head Like A Hole - Remastered").1,
            "Head Like A Hole - Remastered"
        );
        assert_eq!(norm("K@#*%!"), "k");
        assert_eq!(norm("שיר נחמה!"), "שירנחמה");
        let page = json!({"contents": [{"musicResponsiveListItemRenderer": {
            "playlistItemData": {"videoId": "v1", "playlistSetVideoId": "s1", "voteSortValue": 1791740196u64},
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Hatefuck"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "A"}, {"text": " & "}, {"text": "B"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {}}}],
            "fixedColumns": [{"musicResponsiveListItemFixedColumnRenderer": {"text": {"simpleText": "3:11"}}}]}}]});
        let firefox = parse_headers("POST /youtubei/v1/browse HTTP/2\nHost: music.youtube.com\nCookie: SAPISID=x; a=b\nX-Goog-AuthUser: 0\nSec-Fetch-Mode: cors").unwrap();
        assert_eq!(
            (
                firefox["cookie"].as_str(),
                firefox["x-goog-authuser"].as_str()
            ),
            (Some("SAPISID=x; a=b"), Some("0"))
        );
        assert!(!firefox.contains_key("host") && !firefox.contains_key("sec-fetch-mode"));
        let chrome = parse_headers(
            ":authority:\nmusic.youtube.com\ncookie:\nSAPISID=y\nx-goog-authuser:\n1",
        )
        .unwrap();
        assert_eq!(
            (
                chrome["cookie"].as_str(),
                chrome["x-goog-authuser"].as_str()
            ),
            (Some("SAPISID=y"), Some("1"))
        );
        assert!(parse_headers("Accept: */*").is_err());
        let t = parse_tracks(&[page.to_string()]).unwrap();
        assert_eq!(
            (
                t[0].title.as_str(),
                t[0].artists.as_str(),
                t[0].album.as_str(),
                t[0].duration.as_str(),
                t[0].added
            ),
            ("Hatefuck", "A & B", "", "3:11", 1791740196)
        );
    }
}

#[cfg(test)]
mod live {
    #[tokio::test]
    #[ignore]
    async fn login_roundtrip() {
        let src = std::env::var("YTM_HEADERS_FROM").unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(src).unwrap()).unwrap();
        let raw: String = json
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| format!("{k}: {}\n", v.as_str().unwrap()))
            .collect();
        let out = std::env::temp_dir().join("ytm-login-test.json");
        let yt = super::login(&raw, &out).await.unwrap();
        let n = super::playlists(&yt).await.unwrap().len();
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
        std::fs::remove_file(&out).unwrap();
        println!("login ok · {n} playlists · file mode {mode:o}");
        assert!(
            super::login("Cookie: SAPISID=bogus\nX-Goog-AuthUser: 0", &out)
                .await
                .is_err()
                && !out.exists()
        );
        println!("bogus cookie rejected, nothing written");
    }
}
