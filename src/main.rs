mod api;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use api::{Hit, Playlist, Track, Yt};
use futures::StreamExt;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, ListState, Padding, Paragraph, Row, Table,
    TableState,
};
use ratatui::{DefaultTerminal, Frame};
use unicode_bidi::BidiInfo;

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const HINTS: &str = "1-2 tabs · tab focus · / filter · s sort · r reverse · a add to · l like · d remove · i import · e export · q quit";
const SORTS: [&str; 6] = ["position", "added", "artist", "title", "album", "length"];
const PRIVACY: [&str; 3] = ["PRIVATE", "UNLISTED", "PUBLIC"];

fn is_rtl(c: char) -> bool {
    matches!(c, '\u{0590}'..='\u{08FF}' | '\u{FB1D}'..='\u{FEFC}')
}

fn mirror(c: char) -> char {
    match c {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        '«' => '»',
        '»' => '«',
        c => c,
    }
}

/// Terminals don't do bidi, so emit Hebrew/Arabic in visual order. Truncates first so the start of the text survives.
fn rtl(s: &str, width: usize) -> String {
    let mut s: String = s.chars().filter(|c| *c != '\n').collect();
    if width > 0 && s.chars().count() > width {
        s = s.chars().take(width.saturating_sub(1)).collect::<String>() + "…";
    }
    if !s.chars().any(is_rtl) {
        return s;
    }
    let info = BidiInfo::new(&s, None);
    let mut out = String::with_capacity(s.len());
    for para in &info.paragraphs {
        let (levels, runs) = info.visual_runs(para, para.range.clone());
        for run in runs {
            let text = &s[run.clone()];
            if levels[run.start].is_rtl() {
                out.extend(text.chars().rev().map(mirror));
            } else {
                out.push_str(text);
            }
        }
    }
    out
}

fn secs(d: &str) -> u64 {
    d.split(':')
        .fold(0, |acc, x| acc * 60 + x.parse::<u64>().unwrap_or(0))
}

fn ymd(ts: u64) -> String {
    if ts == 0 {
        return String::new();
    }
    // ponytail: UTC date, use the local timezone if a day off ever matters
    let z = (ts / 86400) as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", yoe + era * 400 + (m <= 2) as i64, m, d)
}

fn csv(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn expand(p: &str) -> PathBuf {
    match p.trim().strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(p.trim()),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Library,
    Search,
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Playlists,
    Tracks,
    Filter,
    Query,
    Results,
}

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Tab(Tab),
    NewPlaylist,
    Import,
    Export,
    Sort,
    Reverse,
    Filter,
    SaveOrder,
    Dedupe,
    AddTo,
    Like,
    Remove,
    DeletePlaylist,
    Refresh,
    Quit,
    Ok,
    Cancel,
    Login,
}

enum Msg {
    Status(String),
    Playlists(Vec<Playlist>),
    Tracks(String, Vec<Track>),
    Results(Vec<Hit>),
    Reload(String),
    Report(String, Vec<String>),
    LoggedIn(Yt),
}

struct Field {
    label: &'static str,
    value: String,
    multiline: bool,
    secret: bool,
    choices: &'static [&'static str],
}

impl Field {
    fn text(label: &'static str, value: &str) -> Self {
        Field {
            label,
            value: value.into(),
            multiline: false,
            secret: false,
            choices: &[],
        }
    }
}

type Then<T> = Box<dyn FnOnce(&mut App, T)>;

enum Modal {
    Confirm(String, Box<dyn FnOnce(&mut App)>),
    Form {
        title: String,
        fields: Vec<Field>,
        focus: usize,
        done: Then<Vec<String>>,
    },
    Pick {
        title: String,
        items: Vec<(String, String)>,
        state: ListState,
        done: Then<String>,
    },
    Info {
        title: String,
        lines: Vec<String>,
        scroll: u16,
    },
}

struct App {
    rt: tokio::runtime::Runtime,
    yt: Option<Yt>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    tab: Tab,
    focus: Focus,
    playlists: Vec<Playlist>,
    pl_state: ListState,
    pid: Option<String>,
    tracks: Vec<Track>,
    view: Vec<usize>,
    tr_state: TableState,
    results: Vec<Hit>,
    res_state: TableState,
    sort: usize,
    desc: bool,
    filter: String,
    filtering: bool,
    query: String,
    status: String,
    modal: Option<Modal>,
    buttons: Vec<(Rect, Act)>,
    areas: [Rect; 3],
    quit: bool,
}

impl App {
    fn new(rt: tokio::runtime::Runtime, yt: Option<Yt>, status: String) -> Self {
        let (tx, rx) = channel();
        App {
            rt,
            yt,
            tx,
            rx,
            tab: Tab::Library,
            focus: Focus::Playlists,
            playlists: vec![],
            pl_state: ListState::default(),
            pid: None,
            tracks: vec![],
            view: vec![],
            tr_state: TableState::default(),
            results: vec![],
            res_state: TableState::default(),
            sort: 0,
            desc: false,
            filter: String::new(),
            filtering: false,
            query: String::new(),
            status,
            modal: None,
            buttons: vec![],
            areas: [Rect::default(); 3],
            quit: false,
        }
    }

    fn spawn<F>(&self, label: &str, fut: F)
    where
        F: std::future::Future<Output = anyhow::Result<Msg>> + Send + 'static,
    {
        let tx = self.tx.clone();
        if !label.is_empty() {
            let _ = tx.send(Msg::Status(label.into()));
        }
        self.rt.spawn(async move {
            let _ = tx.send(match fut.await {
                Ok(m) => m,
                Err(e) => Msg::Status(format!("error: {e:#}")),
            });
        });
    }

    fn yt(&mut self) -> Option<Yt> {
        if self.yt.is_none() && self.modal.is_none() {
            self.status = "not logged in · press L or click [ L login ]".into();
            self.act(Act::Login);
        }
        self.yt.clone()
    }

    fn refresh(&mut self) {
        self.reload(true);
    }

    fn reload(&mut self, show: bool) {
        let Some(yt) = self.yt() else { return };
        self.spawn(if show { "loading playlists…" } else { "" }, async move {
            Ok(Msg::Playlists(api::playlists(&yt).await?))
        });
        if let Some(pid) = self.pid.clone() {
            let Some(yt) = self.yt() else { return };
            self.spawn(if show { "loading tracks…" } else { "" }, async move {
                let t = api::tracks(&yt, &pid).await?;
                Ok(Msg::Tracks(pid, t))
            });
        }
    }

    fn load_tracks(&mut self, pid: String) {
        let Some(yt) = self.yt() else { return };
        self.pid = Some(pid.clone());
        self.spawn("loading tracks…", async move {
            let t = api::tracks(&yt, &pid).await?;
            Ok(Msg::Tracks(pid, t))
        });
    }

    fn mutate<F>(&mut self, label: &str, done: impl Into<String>, f: impl FnOnce(Yt) -> F)
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let Some(yt) = self.yt() else { return };
        let fut = f(yt);
        let done = done.into();
        self.spawn(label, async move {
            fut.await?;
            Ok(Msg::Reload(done))
        });
    }

    fn clear_progress(&mut self) {
        if self.status.ends_with('…') {
            self.status.clear();
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Status(s) => self.status = s,
            Msg::Playlists(p) => {
                self.playlists = p;
                self.clear_progress();
                let i = self
                    .pid
                    .as_ref()
                    .and_then(|pid| self.playlists.iter().position(|p| &p.id == pid));
                self.pl_state.select(
                    i.or(self.pl_state.selected())
                        .or(Some(0))
                        .filter(|_| !self.playlists.is_empty()),
                );
            }
            Msg::Tracks(pid, t) => {
                if self.pid.as_deref() == Some(&pid) {
                    let keep = self.tr_state.selected();
                    self.tracks = t;
                    self.clear_progress();
                    self.rebuild_view();
                    self.tr_state
                        .select(keep.filter(|i| *i < self.view.len()).or(Some(0)));
                }
            }
            Msg::Results(r) => {
                self.results = r;
                self.status.clear();
                self.res_state.select(Some(0));
                self.focus = Focus::Results;
            }
            Msg::Reload(s) => {
                self.status = s;
                self.reload(false);
            }
            Msg::LoggedIn(yt) => {
                self.yt = Some(yt);
                self.modal = None;
                self.refresh();
                self.status = format!("logged in · saved to {}", auth_path().display());
            }
            Msg::Report(pid, lines) => {
                self.pid = Some(pid);
                self.refresh();
                self.status.clear();
                self.modal = Some(Modal::Info {
                    title: "Done".into(),
                    lines,
                    scroll: 0,
                });
            }
        }
    }

    fn rebuild_view(&mut self) {
        let q = self.filter.to_lowercase();
        let t = &self.tracks;
        let mut v: Vec<usize> = (0..t.len())
            .filter(|&i| {
                q.is_empty()
                    || format!("{} {} {}", t[i].title, t[i].artists, t[i].album)
                        .to_lowercase()
                        .contains(&q)
            })
            .collect();
        match SORTS[self.sort] {
            "added" => v.sort_by_key(|&i| (t[i].added, t[i].pos)),
            "artist" => v.sort_by_cached_key(|&i| {
                (
                    t[i].artists.to_lowercase(),
                    t[i].album.to_lowercase(),
                    t[i].title.to_lowercase(),
                )
            }),
            "title" => v.sort_by_cached_key(|&i| t[i].title.to_lowercase()),
            "album" => v.sort_by_cached_key(|&i| (t[i].album.to_lowercase(), t[i].pos)),
            "length" => v.sort_by_key(|&i| secs(&t[i].duration)),
            _ => {}
        }
        if self.desc {
            v.reverse();
        }
        self.view = v;
        if self
            .tr_state
            .selected()
            .is_some_and(|i| i >= self.view.len())
        {
            self.tr_state.select(Some(0));
        }
    }

    fn current(&self) -> Option<&Playlist> {
        self.playlists
            .iter()
            .find(|p| Some(&p.id) == self.pid.as_ref())
    }

    fn selected_track(&self) -> Option<Track> {
        self.tr_state
            .selected()
            .and_then(|i| self.view.get(i))
            .map(|&i| self.tracks[i].clone())
    }

    fn selected_song(&self) -> Option<(String, String)> {
        match self.tab {
            Tab::Library => self.selected_track().map(|t| (t.video_id, t.title)),
            Tab::Search => self
                .res_state
                .selected()
                .and_then(|i| self.results.get(i))
                .map(|h| (h.video_id.clone(), h.title.clone())),
        }
    }

    fn confirm(&mut self, msg: String, then: impl FnOnce(&mut App) + 'static) {
        self.modal = Some(Modal::Confirm(msg, Box::new(then)));
    }

    fn pick(&mut self, title: String, new: bool, done: impl FnOnce(&mut App, String) + 'static) {
        let mut items: Vec<(String, String)> = if new {
            vec![("+".into(), "+ New playlist".into())]
        } else {
            vec![]
        };
        items.extend(
            self.playlists
                .iter()
                .filter(|p| p.id != "LM")
                .map(|p| (p.id.clone(), p.title.clone())),
        );
        self.modal = Some(Modal::Pick {
            title,
            items,
            state: ListState::default().with_selected(Some(0)),
            done: Box::new(done),
        });
    }

    fn form(
        &mut self,
        title: String,
        fields: Vec<Field>,
        done: impl FnOnce(&mut App, Vec<String>) + 'static,
    ) {
        self.modal = Some(Modal::Form {
            title,
            fields,
            focus: 0,
            done: Box::new(done),
        });
    }

    fn act(&mut self, a: Act) {
        let lib = self.tab == Tab::Library;
        match a {
            Act::Tab(t) => {
                self.tab = t;
                self.focus = if t == Tab::Library {
                    Focus::Tracks
                } else {
                    Focus::Query
                };
            }
            Act::Quit => self.quit = true,
            Act::Login => {
                let field = Field {
                    label: "Paste request headers · music.youtube.com → Network → POST /browse → Copy Request Headers",
                    value: String::new(),
                    multiline: true,
                    secret: true,
                    choices: &[],
                };
                self.form("Log in to YouTube Music".into(), vec![field], |app, v| {
                    let raw = v.into_iter().next().unwrap_or_default();
                    let tx = app.tx.clone();
                    app.status = "checking login…".into();
                    app.rt.spawn(async move {
                        let _ = tx.send(match api::login(&raw, &auth_path()).await {
                            Ok(yt) => Msg::LoggedIn(yt),
                            Err(e) => {
                                Msg::Status(format!("login failed: {e:#} · press L to try again"))
                            }
                        });
                    });
                });
            }
            Act::Refresh => self.refresh(),
            Act::Sort if lib => {
                self.sort = (self.sort + 1) % SORTS.len();
                self.rebuild_view();
            }
            Act::Reverse if lib => {
                self.desc = !self.desc;
                self.rebuild_view();
            }
            Act::Filter if lib => {
                self.filtering = true;
                self.focus = Focus::Filter;
            }
            Act::AddTo => {
                let Some((vid, title)) = self.selected_song() else {
                    return;
                };
                self.pick(format!("Add '{title}' to"), false, move |app, pid| {
                    let name = app
                        .playlists
                        .iter()
                        .find(|p| p.id == pid)
                        .map_or(String::new(), |p| p.title.clone());
                    app.mutate(
                        "adding…",
                        format!("✓ added '{title}' to {name}"),
                        move |yt| async move { api::add(&yt, &pid, &[vid]).await },
                    );
                });
            }
            Act::Like => {
                let Some((vid, title)) = self.selected_song() else {
                    return;
                };
                self.mutate(
                    "liking…",
                    format!("✓ liked '{title}'"),
                    move |yt| async move { api::rate(&yt, &vid, true).await },
                );
            }
            Act::Remove if lib => {
                let (Some(t), Some(pid)) = (self.selected_track(), self.pid.clone()) else {
                    return;
                };
                self.confirm(format!("Remove '{}'?", t.title), move |app| {
                    if pid == "LM" {
                        app.mutate(
                            "unliking…",
                            "✓ removed from Liked Music",
                            move |yt| async move { api::rate(&yt, &t.video_id, false).await },
                        );
                    } else {
                        app.mutate("removing…", "✓ removed", move |yt| async move {
                            api::remove(&yt, &pid, &[t.set_id]).await
                        });
                    }
                });
            }
            Act::Dedupe if lib => {
                let Some(pid) = self.pid.clone().filter(|p| p != "LM") else {
                    return;
                };
                let mut by_age: Vec<&Track> = self.tracks.iter().collect();
                by_age.sort_by_key(|t| (t.added, t.pos));
                let mut seen = std::collections::HashSet::new();
                let dups: Vec<String> = by_age
                    .into_iter()
                    .filter(|t| !seen.insert(t.video_id.clone()))
                    .map(|t| t.set_id.clone())
                    .collect();
                if dups.is_empty() {
                    self.status = "no duplicates".into();
                    return;
                }
                self.confirm(
                    format!(
                        "Remove {} duplicate(s), keeping the earliest-added copy?",
                        dups.len()
                    ),
                    move |app| {
                        app.mutate(
                            "removing duplicates…",
                            "✓ duplicates removed",
                            move |yt| async move { api::remove(&yt, &pid, &dups).await },
                        );
                    },
                );
            }
            Act::SaveOrder if lib => {
                let Some(pid) = self.pid.clone() else { return };
                if pid == "LM" {
                    self.status = "YouTube doesn't allow reordering Liked Music".into();
                    return;
                }
                if !self.filter.is_empty() {
                    self.status = "clear the filter first (esc) so every track is included".into();
                    return;
                }
                let ids: Vec<String> = self
                    .view
                    .iter()
                    .map(|&i| self.tracks[i].set_id.clone())
                    .collect();
                let label = format!(
                    "Save this order ({} {}) to YouTube Music?",
                    SORTS[self.sort],
                    if self.desc { "desc" } else { "asc" }
                );
                self.confirm(label, move |app| {
                    app.sort = 0;
                    app.desc = false;
                    app.mutate("saving order…", "✓ order saved", move |yt| async move {
                        api::reorder(&yt, &pid, &ids).await
                    });
                });
            }
            Act::DeletePlaylist if lib => {
                let Some(p) = self.current().cloned().filter(|p| p.id != "LM") else {
                    return;
                };
                self.confirm(
                    format!(
                        "Delete playlist '{}' ({} tracks)? This can't be undone.",
                        p.title,
                        self.tracks.len()
                    ),
                    move |app| {
                        app.pid = None;
                        app.tracks.clear();
                        app.view.clear();
                        app.mutate("deleting…", "✓ playlist deleted", move |yt| async move {
                            api::delete(&yt, &p.id).await
                        });
                    },
                );
            }
            Act::NewPlaylist => {
                let fields = vec![
                    Field::text("Name", ""),
                    Field::text("Description", ""),
                    Field {
                        label: "Privacy · ←/→ changes",
                        value: PRIVACY[0].into(),
                        multiline: false,
                        secret: false,
                        choices: &PRIVACY,
                    },
                ];
                self.form("New playlist".into(), fields, |app, v| {
                    let name = v[0].trim().to_string();
                    if name.is_empty() {
                        return;
                    }
                    let Some(yt) = app.yt() else { return };
                    app.spawn("creating…", async move {
                        let id = api::create(&yt, &name, &v[1], &v[2]).await?;
                        Ok(Msg::Report(
                            id,
                            vec![
                                format!("Created '{name}'."),
                                "It can take a few seconds to show up in the playlist list.".into(),
                            ],
                        ))
                    });
                });
            }
            Act::Import => self.pick("Import into".into(), true, |app, pid| {
                let mut fields = vec![
                    Field::text("…or read lines from a file path (optional)", ""),
                    Field {
                        label: "Paste 'Artist - Title' lines, one per line · ctrl+s imports",
                        value: String::new(),
                        multiline: true,
                        secret: false,
                        choices: &[],
                    },
                ];
                if pid == "+" {
                    fields.insert(0, Field::text("New playlist name", "Imported"));
                }
                app.form("Import songs".into(), fields, move |app, mut v| {
                    let name = if pid == "+" {
                        v.remove(0)
                    } else {
                        String::new()
                    };
                    let text = if v[0].trim().is_empty() {
                        v[1].clone()
                    } else {
                        match std::fs::read_to_string(expand(&v[0])) {
                            Ok(t) => t,
                            Err(e) => {
                                app.status = format!("error: {e}");
                                return;
                            }
                        }
                    };
                    app.run_import(Some(pid).filter(|p| p != "+"), name, text);
                });
            }),
            Act::Export => {
                let Some(p) = self.current().cloned().filter(|_| !self.tracks.is_empty()) else {
                    self.status = "open a playlist first".into();
                    return;
                };
                let safe: String = p
                    .title
                    .chars()
                    .map(|c| {
                        if c.is_alphanumeric() || " .-".contains(c) {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                let fields = vec![Field::text(
                    "Save to · .txt = 'Artist - Title' lines (re-importable) · .csv = spreadsheet",
                    &format!("~/Downloads/{safe}.txt"),
                )];
                let n = self.view.len();
                self.form(
                    format!("Export '{}' ({n} tracks, current sort/filter)", p.title),
                    fields,
                    |app, v| app.export(expand(&v[0])),
                );
            }
            _ => {}
        }
    }

    fn run_import(&mut self, pid: Option<String>, name: String, text: String) {
        let Some(yt) = self.yt() else { return };
        let tx = self.tx.clone();
        let lines: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        self.spawn("importing…", async move {
            let n = lines.len();
            let mut results = futures::stream::iter(lines)
                .map(|line| {
                    let yt = yt.clone();
                    async move { (api::match_line(&yt, &line).await, line) }
                })
                .buffered(8);
            let (mut ids, mut misses, mut suspects) = (Vec::new(), Vec::new(), Vec::new());
            let mut i = 0;
            while let Some((r, line)) = results.next().await {
                i += 1;
                let _ = tx.send(Msg::Status(format!("importing {i}/{n}: {line}")));
                match r? {
                    None => misses.push(line),
                    Some((hit, suspect)) => {
                        if suspect {
                            suspects.push(format!("{line} -> {}", hit.title));
                        }
                        if !ids.contains(&hit.video_id) {
                            ids.push(hit.video_id);
                        }
                    }
                }
            }
            let pid = match pid {
                Some(p) => p,
                None => {
                    api::create(
                        &yt,
                        if name.trim().is_empty() {
                            "Imported"
                        } else {
                            name.trim()
                        },
                        "",
                        "PRIVATE",
                    )
                    .await?
                }
            };
            api::add(&yt, &pid, &ids).await?;
            let mut report = vec![
                format!("Added {} track(s).", ids.len()),
                String::new(),
                format!("Not found ({}):", misses.len()),
            ];
            report.extend(misses);
            report.extend([
                String::new(),
                format!(
                    "Added but the title didn't match, check these ({}):",
                    suspects.len()
                ),
            ]);
            report.extend(suspects);
            Ok(Msg::Report(pid, report))
        });
    }

    fn export(&mut self, path: PathBuf) {
        let rows: Vec<&Track> = self.view.iter().map(|&i| &self.tracks[i]).collect();
        let body: String = if path.extension().is_some_and(|e| e == "csv") {
            std::iter::once("position,title,artist,album,added,duration,videoId\n".to_string())
                .chain(rows.iter().map(|t| {
                    format!(
                        "{},{},{},{},{},{},{}\n",
                        t.pos + 1,
                        csv(&t.title),
                        csv(&t.artists),
                        csv(&t.album),
                        ymd(t.added),
                        t.duration,
                        t.video_id
                    )
                }))
                .collect()
        } else {
            rows.iter()
                .map(|t| format!("{} - {}\n", t.artists, t.title))
                .collect()
        };
        let res = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&path, body));
        self.status = match res {
            Ok(_) => format!("exported {} tracks to {}", rows.len(), path.display()),
            Err(e) => format!("error: {e}"),
        };
    }

    fn key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.modal.is_some() {
            return self.modal_key(k);
        }
        match self.focus {
            Focus::Filter => {
                match k.code {
                    KeyCode::Esc => {
                        self.filter.clear();
                        self.filtering = false;
                        self.focus = Focus::Tracks;
                    }
                    KeyCode::Enter | KeyCode::Down | KeyCode::Tab => self.focus = Focus::Tracks,
                    _ => edit(&mut self.filter, k, false),
                }
                return self.rebuild_view();
            }
            Focus::Query => {
                match k.code {
                    KeyCode::Enter if !self.query.trim().is_empty() => {
                        let Some(yt) = self.yt() else { return };
                        let q = self.query.clone();
                        self.spawn("searching…", async move {
                            Ok(Msg::Results(api::search(&yt, &q).await?))
                        });
                    }
                    KeyCode::Esc | KeyCode::Down | KeyCode::Tab => self.focus = Focus::Results,
                    _ => edit(&mut self.query, k, false),
                }
                return;
            }
            _ => {}
        }
        let act = match k.code {
            KeyCode::Char('1') => Some(Act::Tab(Tab::Library)),
            KeyCode::Char('2') => Some(Act::Tab(Tab::Search)),
            KeyCode::Char('q') => Some(Act::Quit),
            KeyCode::Char('/') if self.tab == Tab::Search => {
                self.focus = Focus::Query;
                None
            }
            KeyCode::Char('/') => Some(Act::Filter),
            KeyCode::Char('s') => Some(Act::Sort),
            KeyCode::Char('r') if k.modifiers.contains(KeyModifiers::CONTROL) => Some(Act::Refresh),
            KeyCode::Char('r') => Some(Act::Reverse),
            KeyCode::Char('w') => Some(Act::SaveOrder),
            KeyCode::Char('x') => Some(Act::Dedupe),
            KeyCode::Char('a') => Some(Act::AddTo),
            KeyCode::Char('l') => Some(Act::Like),
            KeyCode::Char('d') => Some(Act::Remove),
            KeyCode::Char('D') => Some(Act::DeletePlaylist),
            KeyCode::Char('L') => Some(Act::Login),
            KeyCode::Char('n') => Some(Act::NewPlaylist),
            KeyCode::Char('i') => Some(Act::Import),
            KeyCode::Char('e') => Some(Act::Export),
            _ => {
                self.nav(k.code);
                None
            }
        };
        if let Some(a) = act {
            self.act(a);
        }
    }

    fn nav(&mut self, code: KeyCode) {
        let lib = self.tab == Tab::Library;
        match code {
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.filtering = false;
                self.rebuild_view();
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Playlists => Focus::Tracks,
                    Focus::Tracks => Focus::Playlists,
                    Focus::Results => Focus::Query,
                    f => f,
                }
            }
            KeyCode::Left if lib => self.focus = Focus::Playlists,
            KeyCode::Right if lib => self.focus = Focus::Tracks,
            KeyCode::Enter if self.focus == Focus::Playlists => self.open_selected_playlist(),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::PageUp => self.step(-20),
            KeyCode::PageDown => self.step(20),
            KeyCode::Home | KeyCode::Char('g') => self.step(i64::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.step(i64::MAX / 2),
            _ => {}
        }
    }

    fn open_selected_playlist(&mut self) {
        if let Some(p) = self
            .pl_state
            .selected()
            .and_then(|i| self.playlists.get(i))
            .cloned()
        {
            self.filter.clear();
            self.filtering = false;
            self.tracks.clear();
            self.view.clear();
            self.tr_state.select(Some(0));
            self.focus = Focus::Tracks;
            self.load_tracks(p.id);
        }
    }

    fn step(&mut self, d: i64) {
        let (len, state_sel) = match self.focus {
            Focus::Playlists => (self.playlists.len(), self.pl_state.selected()),
            Focus::Results => (self.results.len(), self.res_state.selected()),
            _ => (self.view.len(), self.tr_state.selected()),
        };
        if len == 0 {
            return;
        }
        let i = Some(
            (state_sel.unwrap_or(0) as i64)
                .saturating_add(d)
                .clamp(0, len as i64 - 1) as usize,
        );
        match self.focus {
            Focus::Playlists => self.pl_state.select(i),
            Focus::Results => self.res_state.select(i),
            _ => self.tr_state.select(i),
        }
    }

    fn modal_key(&mut self, k: KeyEvent) {
        let Some(modal) = self.modal.as_mut() else {
            return;
        };
        let mut submit =
            matches!(k.code, KeyCode::Char('s') if k.modifiers.contains(KeyModifiers::CONTROL));
        if k.code == KeyCode::Esc {
            self.modal = None;
            return;
        }
        match modal {
            Modal::Confirm(..) => submit |= matches!(k.code, KeyCode::Enter | KeyCode::Char('y')),
            Modal::Info { scroll, .. } => match k.code {
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *scroll += 1,
                KeyCode::Enter | KeyCode::Char('q') => self.modal = None,
                _ => {}
            },
            Modal::Pick { items, state, .. } => match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    state.select(Some(state.selected().unwrap_or(0).saturating_sub(1)))
                }
                KeyCode::Down | KeyCode::Char('j') => state.select(Some(
                    (state.selected().unwrap_or(0) + 1).min(items.len().saturating_sub(1)),
                )),
                KeyCode::Enter => submit = true,
                _ => {}
            },
            Modal::Form { fields, focus, .. } => {
                let n = fields.len();
                let f = &mut fields[*focus];
                match k.code {
                    KeyCode::Tab => *focus = (*focus + 1) % n,
                    KeyCode::BackTab => *focus = (*focus + n - 1) % n,
                    KeyCode::Down if !f.multiline => *focus = (*focus + 1) % n,
                    KeyCode::Up if !f.multiline => *focus = (*focus + n - 1) % n,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                        if !f.choices.is_empty() =>
                    {
                        let i = f.choices.iter().position(|c| *c == f.value).unwrap_or(0);
                        let m = f.choices.len();
                        f.value = f.choices[if k.code == KeyCode::Left {
                            (i + m - 1) % m
                        } else {
                            (i + 1) % m
                        }]
                        .into();
                    }
                    KeyCode::Enter if !f.multiline => submit = true,
                    _ if f.choices.is_empty() => edit(&mut f.value, k, f.multiline),
                    _ => {}
                }
            }
        }
        if submit {
            self.submit_modal();
        }
    }

    fn submit_modal(&mut self) {
        match self.modal.take() {
            Some(Modal::Confirm(_, then)) => then(self),
            Some(Modal::Pick {
                items, state, done, ..
            }) => {
                if let Some((id, _)) = state.selected().and_then(|i| items.into_iter().nth(i)) {
                    done(self, id);
                }
            }
            Some(Modal::Form { fields, done, .. }) => {
                done(self, fields.into_iter().map(|f| f.value).collect())
            }
            _ => {}
        }
    }

    fn paste(&mut self, s: String) {
        let one_line = s.replace(['\r', '\n'], " ");
        match (&mut self.modal, self.focus) {
            (Some(Modal::Form { fields, focus, .. }), _) => {
                if s.contains('\n')
                    && let Some(i) = fields.iter().position(|f| f.multiline)
                {
                    *focus = i;
                }
                let f = &mut fields[*focus];
                if f.multiline {
                    f.value
                        .push_str(&s.replace("\r\n", "\n").replace('\r', "\n"));
                } else if f.choices.is_empty() {
                    f.value.push_str(&one_line);
                }
            }
            (None, Focus::Filter) => {
                self.filter.push_str(&one_line);
                self.rebuild_view();
            }
            (None, Focus::Query) => self.query.push_str(&one_line),
            _ => {}
        }
    }

    fn mouse(&mut self, kind: MouseEventKind, x: u16, y: u16) {
        let pos = Position::new(x, y);
        match kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let d = if kind == MouseEventKind::ScrollDown {
                    3
                } else {
                    -3
                };
                match &mut self.modal {
                    Some(Modal::Info { scroll, .. }) => {
                        *scroll = (*scroll as i64 + d).max(0) as u16
                    }
                    Some(Modal::Pick { items, state, .. }) => state.select(Some(
                        (state.selected().unwrap_or(0) as i64 + d).clamp(0, items.len() as i64 - 1)
                            as usize,
                    )),
                    Some(_) => {}
                    None => {
                        if let Some(i) = self.areas.iter().position(|a| a.contains(pos)) {
                            self.focus = [Focus::Playlists, Focus::Tracks, Focus::Results][i];
                        }
                        self.step(d);
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(&(_, a)) = self.buttons.iter().find(|(r, _)| r.contains(pos)) {
                    return match a {
                        Act::Ok => self.submit_modal(),
                        Act::Cancel => self.modal = None,
                        a => self.act(a),
                    };
                }
                if let Some(Modal::Pick { state, .. }) = &mut self.modal {
                    if let Some(i) = row_at(self.areas[0], y, state.offset())
                        .filter(|_| self.areas[0].contains(pos))
                    {
                        state.select(Some(i));
                        self.submit_modal();
                    }
                    return;
                }
                if self.modal.is_some() {
                    return;
                }
                if self.areas[0].contains(pos) {
                    self.focus = Focus::Playlists;
                    if let Some(i) = row_at(self.areas[0], y, self.pl_state.offset())
                        .filter(|i| *i < self.playlists.len())
                    {
                        self.pl_state.select(Some(i));
                        self.open_selected_playlist();
                    }
                } else if self.areas[1].contains(pos) {
                    self.focus = Focus::Tracks;
                    if let Some(i) =
                        row_at(self.areas[1], y.saturating_sub(1), self.tr_state.offset())
                            .filter(|i| *i < self.view.len())
                    {
                        self.tr_state.select(Some(i));
                    }
                } else if self.areas[2].contains(pos) {
                    self.focus = Focus::Results;
                    if let Some(i) =
                        row_at(self.areas[2], y.saturating_sub(1), self.res_state.offset())
                            .filter(|i| *i < self.results.len())
                    {
                        self.res_state.select(Some(i));
                    }
                } else if self.tab == Tab::Search && y > 2 {
                    self.focus = Focus::Query;
                }
            }
            _ => {}
        }
    }
}

fn row_at(area: Rect, y: u16, offset: usize) -> Option<usize> {
    (y > area.y && y < area.bottom().saturating_sub(1)).then(|| (y - area.y - 1) as usize + offset)
}

fn edit(s: &mut String, k: KeyEvent, multiline: bool) {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Char('u') if ctrl => s.clear(),
        KeyCode::Char('w') if ctrl => {
            let t = s.trim_end().rfind(' ').map_or(0, |i| i + 1);
            s.truncate(t);
        }
        KeyCode::Char(c) if !ctrl => s.push(c),
        KeyCode::Backspace => {
            s.pop();
        }
        KeyCode::Enter if multiline => s.push('\n'),
        _ => {}
    }
}

fn border(title: String, focused: bool) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }))
}

fn button(
    spans: &mut Vec<Span<'static>>,
    buttons: &mut Vec<(Rect, Act)>,
    x: &mut u16,
    y: u16,
    label: &str,
    act: Act,
) {
    let text = format!("[ {label} ]");
    let w = text.chars().count() as u16;
    buttons.push((Rect::new(*x, y, w, 1), act));
    spans.push(Span::styled(text, Style::default().fg(ACCENT)));
    spans.push(Span::raw(" "));
    *x += w + 1;
}

fn header(cols: &[&'static str]) -> Row<'static> {
    Row::new(cols.to_vec()).style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
}

fn draw(f: &mut Frame, app: &mut App) {
    app.buttons.clear();
    app.areas = [Rect::default(); 3];
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(f.area());
    draw_bar(f, rows[0], app);
    match app.tab {
        Tab::Library => draw_library(f, rows[1], app),
        Tab::Search => draw_search(f, rows[1], app),
    }
    let status = if app.status.is_empty() {
        HINTS.to_string()
    } else {
        format!("{}   {HINTS}", rtl(&app.status, 0))
    };
    f.render_widget(
        Paragraph::new(status).style(Style::default().fg(MUTED)),
        rows[2],
    );
    if app.modal.is_some() {
        draw_modal(f, app);
    }
}

fn draw_bar(f: &mut Frame, area: Rect, app: &mut App) {
    let y = area.y + 1;
    let mut spans = vec![
        Span::raw(" "),
        Span::styled("YTM", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
    ];
    let mut x = area.x + 6;
    for (label, tab) in [("Library", Tab::Library), ("Search", Tab::Search)] {
        let style = if app.tab == tab {
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default()
        };
        let w = label.len() as u16 + 2;
        app.buttons.push((Rect::new(x, y, w, 1), Act::Tab(tab)));
        spans.push(Span::styled(format!(" {label} "), style));
        x += w;
    }
    let right = [
        ("+ playlist", Act::NewPlaylist),
        ("i import", Act::Import),
        ("e export", Act::Export),
        ("L login", Act::Login),
    ];
    let rw: u16 = right.iter().map(|(l, _)| l.len() as u16 + 5).sum();
    let start = area.right().saturating_sub(rw).max(x + 1);
    spans.push(Span::raw(" ".repeat((start - x) as usize)));
    x = start;
    for (label, act) in right {
        button(&mut spans, &mut app.buttons, &mut x, y, label, act);
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(area.x, y, area.width, 1),
    );
}

fn draw_library(f: &mut Frame, area: Rect, app: &mut App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(26), Constraint::Percentage(74)])
        .split(area);
    let items: Vec<ListItem> = app
        .playlists
        .iter()
        .map(|p| {
            ListItem::new(Line::from(vec![
                Span::raw(rtl(&p.title, 0)),
                Span::styled(format!("  {}", p.count), Style::default().fg(MUTED)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(border(
            " Playlists · enter opens ".into(),
            app.focus == Focus::Playlists,
        ))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, cols[0], &mut app.pl_state);
    app.areas[0] = cols[0];

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(if app.filtering { 3 } else { 0 }),
            Constraint::Min(3),
        ])
        .split(cols[1]);
    let mut spans = vec![Span::raw(" ")];
    let mut x = right[0].x + 1;
    let sort = format!("s sort: {}", SORTS[app.sort]);
    let rev = format!("r {}", if app.desc { "desc" } else { "asc" });
    for (label, act) in [
        (sort.as_str(), Act::Sort),
        (rev.as_str(), Act::Reverse),
        ("/ filter", Act::Filter),
        ("w save order", Act::SaveOrder),
        ("x dedupe", Act::Dedupe),
        ("a add to", Act::AddTo),
        ("d remove", Act::Remove),
        ("D delete", Act::DeletePlaylist),
    ] {
        button(&mut spans, &mut app.buttons, &mut x, right[0].y, label, act);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), right[0]);
    if app.filtering {
        let cursor = if app.focus == Focus::Filter {
            "▏"
        } else {
            ""
        };
        f.render_widget(
            Paragraph::new(format!("{}{cursor}", rtl(&app.filter, 0))).block(border(
                " Filter · title / artist / album · esc clears ".into(),
                app.focus == Focus::Filter,
            )),
            right[1],
        );
    }

    let tw = (right[2].width as usize)
        .saturating_sub(4 + 22 + 20 + 10 + 7 + 5 + 2)
        .max(10);
    let rows: Vec<Row> = app
        .view
        .iter()
        .map(|&i| {
            let t = &app.tracks[i];
            Row::new(vec![
                Cell::from((t.pos + 1).to_string()).style(Style::default().fg(MUTED)),
                Cell::from(rtl(&t.title, tw)),
                Cell::from(rtl(&t.artists, 22)),
                Cell::from(rtl(&t.album, 20)),
                Cell::from(ymd(t.added)).style(Style::default().fg(MUTED)),
                Cell::from(t.duration.clone()),
            ])
        })
        .collect();
    let title = match app.current() {
        Some(p) => format!(
            " {} · {}/{} tracks ",
            rtl(&p.title, 0),
            app.view.len(),
            app.tracks.len()
        ),
        None if app.pid.is_some() => format!(" {}/{} tracks ", app.view.len(), app.tracks.len()),
        None => " Tracks ".into(),
    };
    let widths = [
        Constraint::Length(4),
        Constraint::Min(10),
        Constraint::Length(22),
        Constraint::Length(20),
        Constraint::Length(10),
        Constraint::Length(7),
    ];
    let table = Table::new(rows, widths)
        .header(header(&["#", "Title", "Artist", "Album", "Added", "Time"]))
        .block(border(title, app.focus == Focus::Tracks))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(table, right[2], &mut app.tr_state);
    app.areas[1] = right[2];
}

fn draw_search(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(3),
        ])
        .split(area);
    let mut spans = vec![Span::raw(" ")];
    let mut x = rows[1].x + 1;
    button(
        &mut spans,
        &mut app.buttons,
        &mut x,
        rows[1].y,
        "a add to playlist",
        Act::AddTo,
    );
    button(
        &mut spans,
        &mut app.buttons,
        &mut x,
        rows[1].y,
        "l like",
        Act::Like,
    );
    f.render_widget(Paragraph::new(Line::from(spans)), rows[1]);
    let rows = [rows[0], rows[2]];
    let cursor = if app.focus == Focus::Query { "▏" } else { "" };
    f.render_widget(
        Paragraph::new(format!("{}{cursor}", rtl(&app.query, 0))).block(border(
            " Search YouTube Music songs · enter ".into(),
            app.focus == Focus::Query,
        )),
        rows[0],
    );
    let tw = (rows[1].width as usize)
        .saturating_sub(26 + 26 + 6 + 3 + 2)
        .max(10);
    let items: Vec<Row> = app
        .results
        .iter()
        .map(|h| {
            Row::new(vec![
                Cell::from(rtl(&h.title, tw)),
                Cell::from(rtl(&h.artists, 26)),
                Cell::from(rtl(&h.album, 26)),
                Cell::from(h.duration.clone()),
            ])
        })
        .collect();
    let table = Table::new(
        items,
        [
            Constraint::Min(10),
            Constraint::Length(26),
            Constraint::Length(26),
            Constraint::Length(6),
        ],
    )
    .header(header(&["Title", "Artist", "Album", "Time"]))
    .block(border(
        " Results · / new search ".into(),
        app.focus == Focus::Results,
    ))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(table, rows[1], &mut app.res_state);
    app.areas[2] = rows[1];
}

fn draw_modal(f: &mut Frame, app: &mut App) {
    let full = f.area();
    let w = full.width.saturating_sub(4).min(96);
    let inner_w = w.saturating_sub(4) as usize;
    let title_style = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = Vec::new();
    let mut ok = "enter ok";
    let title = match app.modal.as_mut().unwrap() {
        Modal::Confirm(msg, _) => {
            lines.push(Line::raw(rtl(msg, 0)));
            ok = "enter yes";
            "Confirm".to_string()
        }
        Modal::Info {
            title,
            lines: body,
            scroll,
        } => {
            let h = full.height.saturating_sub(10) as usize;
            *scroll = (*scroll).min(body.len().saturating_sub(h) as u16);
            lines.extend(
                body.iter()
                    .skip(*scroll as usize)
                    .take(h)
                    .map(|l| Line::raw(rtl(l, inner_w))),
            );
            ok = "";
            format!("{title} · ↑/↓ scroll")
        }
        Modal::Pick {
            title,
            items,
            state,
            ..
        } => {
            let h = (full.height.saturating_sub(8) as usize)
                .min(items.len())
                .max(1);
            let list = List::new(
                items
                    .iter()
                    .map(|(_, t)| ListItem::new(rtl(t, inner_w)))
                    .collect::<Vec<_>>(),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            let area = centered(full, w, h as u16 + 2);
            f.render_widget(Clear, area);
            f.render_stateful_widget(
                list.block(
                    border(
                        format!(" {} · enter picks · esc cancels ", rtl(title, 0)),
                        true,
                    )
                    .title_style(title_style),
                ),
                area,
                state,
            );
            app.areas[0] = area;
            return;
        }
        Modal::Form {
            title,
            fields,
            focus,
            ..
        } => {
            for (i, fl) in fields.iter().enumerate() {
                let on = i == *focus;
                let cursor = if on { "▏" } else { "" };
                lines.push(Line::styled(
                    fl.label,
                    Style::default().fg(if on { ACCENT } else { MUTED }),
                ));
                if fl.secret {
                    let n = fl.value.lines().filter(|l| !l.trim().is_empty()).count();
                    let shown = if n == 0 {
                        "waiting for paste (cmd+v)…".to_string()
                    } else {
                        format!("{n} header lines pasted (hidden)")
                    };
                    lines.push(Line::raw(format!("  {shown}{cursor}")));
                    lines.push(Line::styled("  Saved only on this Mac, readable by you alone. Nothing is sent anywhere but YouTube.", Style::default().fg(MUTED)));
                    ok = "ctrl+s log in";
                } else if fl.multiline {
                    let all: Vec<&str> = fl.value.split('\n').collect();
                    let skip = all.len().saturating_sub(12);
                    for (j, l) in all.iter().enumerate().skip(skip) {
                        lines.push(Line::raw(format!(
                            "  {}{}",
                            rtl(l, inner_w - 3),
                            if j == all.len() - 1 { cursor } else { "" }
                        )));
                    }
                    for _ in all.len() - skip..6 {
                        lines.push(Line::raw(""));
                    }
                    lines.push(Line::styled(
                        format!(
                            "  {} line(s)",
                            fl.value.lines().filter(|l| !l.trim().is_empty()).count()
                        ),
                        Style::default().fg(MUTED),
                    ));
                    ok = "ctrl+s ok";
                } else if !fl.choices.is_empty() {
                    let style = |c: &str| {
                        if c == fl.value {
                            Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED)
                        } else {
                            Style::default()
                        }
                    };
                    lines.push(Line::from(
                        fl.choices
                            .iter()
                            .map(|c| Span::styled(format!(" {c} "), style(c)))
                            .collect::<Vec<_>>(),
                    ));
                } else {
                    lines.push(Line::raw(format!(
                        "> {}{cursor}",
                        rtl(&fl.value, inner_w - 3)
                    )));
                }
                lines.push(Line::raw(""));
            }
            title.clone()
        }
    };
    lines.push(Line::raw(""));
    let area = centered(full, w, lines.len() as u16 + 3);
    let y = area.bottom().saturating_sub(2);
    let mut spans = vec![];
    let mut x = area.x + 2;
    if !ok.is_empty() {
        button(&mut spans, &mut app.buttons, &mut x, y, ok, Act::Ok);
    }
    button(
        &mut spans,
        &mut app.buttons,
        &mut x,
        y,
        if ok.is_empty() {
            "esc close"
        } else {
            "esc cancel"
        },
        Act::Cancel,
    );
    lines.push(Line::from(spans));
    f.render_widget(Clear, area);
    let block = border(format!(" {} ", rtl(&title, 0)), true)
        .title_style(title_style)
        .padding(Padding::horizontal(1));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn centered(full: Rect, w: u16, h: u16) -> Rect {
    let h = h.min(full.height);
    Rect::new(
        full.x + full.width.saturating_sub(w) / 2,
        full.y + full.height.saturating_sub(h) / 2,
        w,
        h,
    )
}

fn run(term: &mut DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    app.refresh();
    while !app.quit {
        while let Ok(m) = app.rx.try_recv() {
            app.handle(m);
        }
        term.draw(|f| draw(f, app))?;
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => app.key(k),
                Event::Paste(s) => app.paste(s),
                Event::Mouse(m) => app.mouse(m.kind, m.column, m.row),
                _ => {}
            }
        }
    }
    Ok(())
}

fn auth_path() -> PathBuf {
    if let Ok(p) = std::env::var("YTM_AUTH") {
        return PathBuf::from(p);
    }
    let config = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        });
    config.join("yt-music-tui").join("browser.json")
}

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let (yt, status) = match rt.block_on(api::connect(&auth_path())) {
        Ok(yt) => (Some(yt), String::new()),
        Err(_) if !auth_path().exists() => (None, String::new()),
        Err(e) => (None, format!("saved login didn't work ({e:#})")),
    };
    let mut app = App::new(rt, yt, status);
    let mut term = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let res = run(&mut term, &mut app);
    execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    ratatui::restore();
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bidi_dates_and_csv() {
        assert_eq!(rtl("Hail to the King", 0), "Hail to the King");
        assert_eq!(rtl("ישראל", 0), "לארשי");
        assert_eq!(rtl("שלום (עולם)", 0), "(םלוע) םולש");
        assert_eq!(rtl("Remove 'שלום'?", 0), "Remove 'םולש'?");
        assert_eq!(rtl("פאפי", 3), "…אפ");
        assert_eq!(ymd(1791740196), "2026-10-11");
        assert_eq!(ymd(951782400), "2000-02-29");
        assert_eq!(secs("1:02:03"), 3723);
        assert_eq!(csv("a, \"b\""), "\"a, \"\"b\"\"\"");
    }

    #[test]
    fn logged_out_opens_login_and_hides_headers() {
        let mut app = App::new(tokio::runtime::Runtime::new().unwrap(), None, String::new());
        app.refresh();
        assert!(
            matches!(&app.modal, Some(Modal::Form { title, .. }) if title.starts_with("Log in"))
        );
        app.paste("Cookie: SAPISID=supersecret\nX-Goog-AuthUser: 0\n".into());
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        t.draw(|f| draw(f, &mut app)).unwrap();
        let screen: String = t
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            screen.contains("2 header lines pasted (hidden)") && !screen.contains("supersecret")
        );
    }
}

#[cfg(test)]
mod live {
    use super::*;
    use ratatui::backend::TestBackend;

    fn pump(app: &mut App, until: impl Fn(&App) -> bool) {
        for _ in 0..600 {
            while let Ok(m) = app.rx.try_recv() {
                app.handle(m);
            }
            if until(app) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timeout: {}", app.status);
    }

    fn screen(app: &mut App, name: &str) {
        let mut t = ratatui::Terminal::new(TestBackend::new(170, 40)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        let buf = t.backend().buffer();
        let text: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        std::fs::write(std::env::temp_dir().join(format!("ytm-{name}.txt")), text).unwrap();
    }

    fn key(app: &mut App, c: KeyCode) {
        app.key(KeyEvent::new(c, KeyModifiers::NONE));
    }

    fn wait_ok(app: &mut App, label: &str) {
        pump(app, |a| a.status != label && !a.status.ends_with('…'));
        assert!(!app.status.starts_with("error"), "{}", app.status);
    }

    #[test]
    #[ignore]
    fn live_mutations() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let yt = rt.block_on(api::connect(&auth_path())).unwrap();
        let mut app = App::new(rt, Some(yt.clone()), String::new());
        app.refresh();
        pump(&mut app, |a| !a.playlists.is_empty());
        key(&mut app, KeyCode::Char('i'));
        key(&mut app, KeyCode::Enter);
        if let Some(Modal::Form { fields, .. }) = &mut app.modal {
            fields[0].value = "zz-ytm-rust-test".into();
        }
        app.paste("Deftones - Rosemary\nKorn - Freak On a Leash\nDeftones - Rosemary\n✝✝✝ (Crosses) - Bitches Brew\nzzqx nonexistent - qqqzzz blorp 99999".into());
        let t = std::time::Instant::now();
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        pump(&mut app, |a| matches!(a.modal, Some(Modal::Info { .. })));
        println!("import took {:?}", t.elapsed());
        if let Some(Modal::Info { lines, .. }) = &app.modal {
            println!("{}", lines.join("\n"));
        }
        let pid = app.pid.clone().unwrap();
        key(&mut app, KeyCode::Esc);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pump(&mut app, |a| a.tracks.len() >= 3);
            let titles = |a: &App| {
                a.view
                    .iter()
                    .map(|&i| a.tracks[i].title.clone())
                    .collect::<Vec<_>>()
            };
            println!("imported {:?}", titles(&app));
            app.rt
                .block_on(api::add(&yt, &pid, &[app.tracks[0].video_id.clone()]))
                .unwrap();
            let n = app.tracks.len();
            std::thread::sleep(Duration::from_secs(2));
            app.load_tracks(pid.clone());
            pump(&mut app, |a| a.tracks.len() == n + 1);
            key(&mut app, KeyCode::Char('x'));
            key(&mut app, KeyCode::Enter);
            wait_ok(&mut app, "removing duplicates…");
            pump(&mut app, |a| a.tracks.len() == n);
            println!("after dedupe {:?}", titles(&app));
            let before = titles(&app);
            app.focus = Focus::Tracks;
            key(&mut app, KeyCode::Char('r'));
            key(&mut app, KeyCode::Char('w'));
            key(&mut app, KeyCode::Enter);
            wait_ok(&mut app, "saving order…");
            std::thread::sleep(Duration::from_secs(2));
            app.load_tracks(pid.clone());
            pump(&mut app, |a| titles(a) != before);
            println!("after reverse+save {:?}", titles(&app));
            key(&mut app, KeyCode::Char('d'));
            key(&mut app, KeyCode::Enter);
            wait_ok(&mut app, "removing…");
            pump(&mut app, |a| a.tracks.len() == n - 1);
            println!("after remove {:?}", titles(&app));
        }));
        app.rt.block_on(api::delete(&yt, &pid)).unwrap();
        println!("deleted test playlist");
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    #[ignore]
    fn live_add_from_search() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let yt = rt.block_on(api::connect(&auth_path())).unwrap();
        let pid = rt
            .block_on(api::create(&yt, "zz-ytm-add-test", "", "PRIVATE"))
            .unwrap();
        let mut app = App::new(rt, Some(yt.clone()), String::new());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for _ in 0..20 {
                app.refresh();
                pump(&mut app, |a| {
                    !a.playlists.is_empty() && !a.status.ends_with('…')
                });
                if app.playlists.iter().any(|p| p.id == pid) {
                    break;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
            key(&mut app, KeyCode::Char('2'));
            for c in "deftones rosemary".chars() {
                key(&mut app, KeyCode::Char(c));
            }
            key(&mut app, KeyCode::Enter);
            pump(&mut app, |a| !a.results.is_empty());
            println!("focus results: {}", app.focus == Focus::Results);
            key(&mut app, KeyCode::Char('a'));
            let Some(Modal::Pick { items, state, .. }) = &mut app.modal else {
                panic!("no picker opened")
            };
            state.select(items.iter().position(|(id, _)| *id == pid));
            key(&mut app, KeyCode::Enter);
            let mut seen = vec![];
            for _ in 0..200 {
                while let Ok(m) = app.rx.try_recv() {
                    app.handle(m);
                    seen.push(app.status.clone());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            println!("status sequence: {seen:?}");
            assert!(
                app.status
                    .starts_with("✓ added 'Rosemary' to zz-ytm-add-test"),
                "{}",
                app.status
            );
            let t = app.rt.block_on(api::tracks(&yt, &pid)).unwrap();
            println!(
                "playlist now has {:?}",
                t.iter().map(|t| &t.title).collect::<Vec<_>>()
            );
            assert_eq!(t.len(), 1);
        }));
        app.rt.block_on(api::delete(&yt, &pid)).unwrap();
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    #[ignore]
    fn live_read_only() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let yt = rt.block_on(api::connect(&auth_path())).unwrap();
        let mut app = App::new(rt, Some(yt), String::new());
        let t = std::time::Instant::now();
        app.refresh();
        pump(&mut app, |a| !a.playlists.is_empty());
        println!("playlists: {} in {:?}", app.playlists.len(), t.elapsed());
        let i = app
            .playlists
            .iter()
            .position(|p| p.title == "Spicy Rock")
            .unwrap();
        app.pl_state.select(Some(i));
        let t = std::time::Instant::now();
        key(&mut app, KeyCode::Enter);
        pump(&mut app, |a| a.tracks.len() > 200);
        println!("spicy: {} tracks in {:?}", app.tracks.len(), t.elapsed());
        screen(&mut app, "r1");
        key(&mut app, KeyCode::Char('s'));
        key(&mut app, KeyCode::Char('s'));
        key(&mut app, KeyCode::Char('r'));
        println!(
            "sort {} desc {} first {:?}",
            SORTS[app.sort],
            app.desc,
            app.view
                .iter()
                .take(3)
                .map(|&i| &app.tracks[i].artists)
                .collect::<Vec<_>>()
        );
        screen(&mut app, "r2");
        key(&mut app, KeyCode::Char('/'));
        for c in "deftones".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        println!("filter deftones -> {}", app.view.len());
        key(&mut app, KeyCode::Esc);
        app.tracks.clear();
        app.load_tracks("LM".into());
        pump(&mut app, |a| {
            a.pid.as_deref() == Some("LM") && a.tracks.len() > 100
        });
        app.sort = 1;
        app.desc = true;
        app.rebuild_view();
        screen(&mut app, "r3");
        key(&mut app, KeyCode::Char('e'));
        if let Some(Modal::Form { fields, .. }) = &mut app.modal {
            fields[0].value = std::env::temp_dir()
                .join("ytm-liked.csv")
                .display()
                .to_string();
        }
        key(&mut app, KeyCode::Enter);
        println!("{}", app.status);
        key(&mut app, KeyCode::Char('i'));
        screen(&mut app, "r4");
        key(&mut app, KeyCode::Enter);
        app.paste("Deftones - Rosemary\nשלום חנוך - מחכה".into());
        screen(&mut app, "r5");
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Char('2'));
        for c in "sleep token".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        pump(&mut app, |a| !a.results.is_empty());
        screen(&mut app, "r6");
        println!(
            "results {:?}",
            app.results
                .iter()
                .take(3)
                .map(|h| &h.title)
                .collect::<Vec<_>>()
        );
        let rt = &app.rt;
        let yt = app.yt.clone().unwrap();
        for line in [
            "✝✝✝ (Crosses) - Bitches Brew",
            "Highly Suspect - My Name Is Human",
            "Korn - K@#*%!",
            "שלום חנוך - מחכה",
        ] {
            let r = rt
                .block_on(api::match_line(&yt, line))
                .unwrap()
                .map(|(h, s)| (h.title, s));
            println!("match {line} -> {r:?}");
        }
    }
}
