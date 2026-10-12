# yt-music-tui

A fast terminal UI for managing your YouTube Music library. It does the library chores the YouTube Music website doesn't: bulk-import songs from plain text, export playlists, sort by added date / artist / title / album / length (reversible) and save that order back, and remove duplicates. There's no player.

It talks to YouTube Music directly from your machine using your own browser session. No third-party service, no OAuth app, and your Google password is never involved.

## Features

- **Library**: browse your playlists, including Liked Music, with added dates.
- **Sort**: by position, added date, artist, title, album or length. `r` reverses (newest first instead of oldest first). `w` writes the current order to the playlist on YouTube Music.
- **Filter**: by title, artist or album.
- **Import**: paste `Artist - Title` lines (e.g. a Spotify export) or point at a file. Songs are matched in parallel, and you get a report of songs that weren't found and matches worth double-checking.
- **Export**: `.txt` (`Artist - Title`, re-importable) or `.csv` (with added date and video ID), using the current sort and filter.
- **Dedupe**: removes repeated songs, keeping the earliest-added copy.
- **Search**: find songs, add them to a playlist, like them.
- Create, delete, add to and remove from playlists.
- Hebrew and Arabic text displays correctly, even in terminals without right-to-left support.
- Follows your terminal's color theme. Every button is clickable, and the mouse wheel scrolls.

## Install

You need Rust (1.88 or newer):

```sh
brew install rust            # or: curl https://sh.rustup.rs -sSf | sh
```

Then build and install the `yt-music-tui` command from this folder:

```sh
git clone git@github.com:MatanMoradian/yt-music-tui.git
cd yt-music-tui
cargo install --path .
```

This puts the binary in `~/.cargo/bin`. If `yt-music-tui` isn't found afterwards, add that folder to your PATH:

```sh
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc && source ~/.zshrc
```

To update, pull and run `cargo install --path .` again. To uninstall, run `cargo uninstall yt-music-tui`.

## Log in

The first time you run `yt-music-tui`, a login window opens. It needs the request headers from a logged-in YouTube Music tab:

1. Open [music.youtube.com](https://music.youtube.com) in your browser and make sure you're logged in.
2. Open the developer tools' **Network** tab (`Cmd+Option+E` in Firefox/Zen, `Cmd+Option+I` then **Network** in Chrome).
3. Type `browse` in the filter box, then click around (for example, open **Library**) so requests appear.
4. Pick a **POST** request with status **200**.
   - **Firefox / Zen**: right-click it → **Copy Value → Copy Request Headers**.
   - **Chrome / Edge**: click it, go to **Headers → Request Headers**, and copy everything in that section.
5. Paste into the login window (`Cmd+V`), then press `Ctrl+S`.

The pasted text isn't shown on screen. The login is checked against YouTube Music, and only saved if it works, to `~/.config/yt-music-tui/browser.json`, readable only by your user.

The login lasts until you sign out of YouTube Music in that browser. When it stops working, press `L` (or click **[ L login ]**) and paste fresh headers.

> **Treat `browser.json` like a password.** It contains your session cookie. Don't share it or commit it. This repo's `.gitignore` excludes it.

To keep the login somewhere else, for example to share it with [ytmusicapi](https://github.com/sigma67/ytmusicapi), which uses the same file format, set `YTM_AUTH`:

```sh
YTM_AUTH=~/path/to/browser.json yt-music-tui
```

## Keys

| Key | Action |
|---|---|
| `1` / `2` | Library / Search tab |
| `Tab`, `←` `→` | Move focus between playlists and tracks |
| `↑` `↓` / `j` `k`, `PgUp` `PgDn`, `g` `G` | Move the selection |
| `Enter` | Open the selected playlist / run a search |
| `s` | Cycle sort: position → added → artist → title → album → length |
| `r` | Reverse the sort order |
| `w` | Save the current order to YouTube Music |
| `/` | Filter tracks (Library) or start a new search (Search) · `Esc` clears |
| `i` | Import songs into a new or existing playlist |
| `e` | Export the open playlist to `.txt` or `.csv` |
| `x` | Remove duplicate songs |
| `a` | Add the selected song to a playlist |
| `l` | Like the selected song |
| `d` | Remove the selected song (un-likes it in Liked Music) |
| `D` | Delete the open playlist |
| `n` | New playlist |
| `L` | Log in again |
| `Ctrl+R` | Reload |
| `q` / `Ctrl+C` | Quit |

In dialogs: `Enter` confirms, `Esc` cancels, `Tab` moves between fields, and `Ctrl+S` submits forms that have a multi-line box.

## Import format

One song per line, `Artist - Title`. Extra artists after a comma and suffixes like `- Remastered` are handled:

```
Nine Inch Nails - Head Like A Hole - Remastered
Korn, Fred Durst - All In the Family (feat. Fred Durst)
Deftones - Rosemary
```

Each line is searched as a song, then with punctuation stripped, then as a music video. The first result whose title matches is used. If none match, the top result is added and listed under "check these" in the import report.

## Notes and limits

- YouTube Music doesn't allow reordering Liked Music, so `w` is disabled there.
- Added dates are shown in UTC.
- This uses YouTube Music's internal web API (through [ytmapi-rs](https://github.com/nick42d/youtui)), the same one the website uses. It can break if YouTube changes that API.

## Development

```sh
cargo run --release                                  # run from source
cargo test                                           # offline unit tests
cargo test --release -- --ignored --nocapture        # live tests against your account
```

The live tests need a saved login. `live_mutations` and `live_add_from_search` create a temporary `zz-ytm-*` playlist and delete it afterwards.
