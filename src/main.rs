mod audio;
mod decoder;
mod online_metadata;

use anyhow::{bail, Result};
use slint::{Image, ModelRc, SharedPixelBuffer, SharedString, VecModel};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use walkdir::WalkDir;

use audio::AudioEngine;
use decoder::{probe_metadata, FileDecoder, TrackMetadata};

slint::include_modules!();

const AUDIO_EXTENSIONS: &[&str] = &["flac", "wav", "aiff", "aif", "m4a", "mp3"];

#[derive(Clone)]
struct TrackEntry {
    path: PathBuf,
    title: String,
    duration_secs: f64,
    track_number: Option<u32>,
    disc_number: Option<u32>,
    sort_name: String,
}

struct Playlist {
    tracks: Vec<TrackEntry>,
    current: Option<usize>,
}

#[derive(Clone)]
struct AlbumEntry {
    folder: PathBuf,
    title: String,
    artist: String,
    /// Remote artwork (e.g. Cover Art Archive); overrides probing folder cover.
    cover_override: Option<Vec<u8>>,
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.iter().any(|&x| x.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

fn format_duration_hms(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "0:00".to_string();
    }
    let s = seconds.floor() as u64;
    let m = s / 60;
    let r = s % 60;
    format!("{m}:{r:02}")
}

fn build_playlist(selected: &Path) -> Result<(Playlist, usize)> {
    let parent = selected.parent().unwrap_or_else(|| Path::new("."));
    let mut entries: Vec<TrackEntry> = Vec::new();

    if let Ok(read_dir) = fs::read_dir(parent) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_file() || !is_audio_file(&path) {
                continue;
            }
            let (meta, duration) = match probe_metadata(&path) {
                Ok(x) => x,
                Err(e) => {
                    log::warn!("Skipping {:?}: {}", path, e);
                    continue;
                }
            };
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let title = meta
                .title
                .clone()
                .unwrap_or_else(|| file_name.clone());
            let sort_name = file_name.to_lowercase();
            entries.push(TrackEntry {
                path,
                title,
                duration_secs: duration,
                track_number: meta.track_number,
                disc_number: meta.disc_number,
                sort_name,
            });
        }
    }

    if !entries.iter().any(|e| e.path == selected) {
        if let Ok((meta, duration)) = probe_metadata(selected) {
            let file_name = selected
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let title = meta
                .title
                .clone()
                .unwrap_or_else(|| file_name.clone());
            let sort_name = file_name.to_lowercase();
            entries.push(TrackEntry {
                path: selected.to_path_buf(),
                title,
                duration_secs: duration,
                track_number: meta.track_number,
                disc_number: meta.disc_number,
                sort_name,
            });
        }
    }

    entries.sort_by(|a, b| {
        let da = a.disc_number.unwrap_or(1);
        let db = b.disc_number.unwrap_or(1);
        da.cmp(&db)
            .then_with(|| {
                let ta = a.track_number.unwrap_or(u32::MAX);
                let tb = b.track_number.unwrap_or(u32::MAX);
                ta.cmp(&tb)
            })
            .then_with(|| a.sort_name.cmp(&b.sort_name))
    });

    if entries.is_empty() {
        bail!("No playable audio files found in this folder");
    }

    let idx = entries
        .iter()
        .position(|e| e.path == selected)
        .unwrap_or(0);

    Ok((
        Playlist {
            tracks: entries,
            current: Some(idx),
        },
        idx,
    ))
}

fn push_tracks_model(app: &PlayerWindow, playlist: &Playlist) {
    let rows: Vec<TrackInfo> = playlist
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let display = t
                .track_number
                .map(|n| format!("{n:02}"))
                .unwrap_or_else(|| format!("{}", i + 1));
            TrackInfo {
                display_number: SharedString::from(display),
                title: SharedString::from(t.title.as_str()),
                duration_text: SharedString::from(format_duration_hms(t.duration_secs)),
            }
        })
        .collect();
    app.set_tracks(ModelRc::new(VecModel::from(rows)));
}

fn load_folder_cover_bytes(dir: &Path) -> Option<Vec<u8>> {
    let names = [
        "cover.jpg",
        "cover.jpeg",
        "cover.png",
        "folder.jpg",
        "folder.jpeg",
        "folder.png",
    ];
    for n in names {
        let p = dir.join(n);
        if p.is_file() {
            if let Ok(bytes) = fs::read(&p) {
                return Some(bytes);
            }
        }
    }
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let Some(fname_os) = p.file_name() else {
                continue;
            };
            let fname = fname_os.to_string_lossy().to_lowercase();
            if matches!(
                fname.as_str(),
                "cover.jpg" | "cover.jpeg" | "cover.png" | "folder.jpg" | "folder.jpeg" | "folder.png"
            ) {
                if let Ok(b) = fs::read(p) {
                    return Some(b);
                }
            }
        }
    }
    None
}

fn bytes_to_slint_image(bytes: &[u8]) -> Option<Image> {
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    let buf = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(img.as_raw(), w, h);
    Some(Image::from_rgba8(buf))
}

fn artwork_for_track(meta: &TrackMetadata, audio_path: &Path) -> Image {
    if let Some(ref b) = meta.artwork {
        if let Some(img) = bytes_to_slint_image(b) {
            return img;
        }
    }
    if let Some(dir) = audio_path.parent() {
        if let Some(folder_bytes) = load_folder_cover_bytes(dir) {
            if let Some(img) = bytes_to_slint_image(&folder_bytes) {
                return img;
            }
        }
    }
    Image::default()
}

fn first_audio_in_folder(folder: &Path) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(folder)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.is_file() && is_audio_file(&p) {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    files.sort_by(|a, b| {
        let na = a
            .file_name()
            .map(|x| x.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let nb = b
            .file_name()
            .map(|x| x.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        na.cmp(&nb)
    });
    files.into_iter().next()
}

fn has_audio_direct_child(dir: &Path) -> bool {
    fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .any(|e| {
            let p = e.path();
            p.is_file() && is_audio_file(&p)
        })
}

fn discover_album_roots(library_root: &Path) -> Vec<PathBuf> {
    let mut set = std::collections::BTreeSet::new();
    for entry in WalkDir::new(library_root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_dir() {
            continue;
        }
        let p = entry.path();
        if has_audio_direct_child(p) {
            set.insert(p.to_path_buf());
        }
    }
    set.into_iter().collect()
}

fn build_album_entries(folders: Vec<PathBuf>) -> Vec<AlbumEntry> {
    let mut albums = Vec::new();
    for folder in folders {
        let Some(track) = first_audio_in_folder(&folder) else {
            continue;
        };
        let Ok((meta, _)) = probe_metadata(&track) else {
            continue;
        };
        let title = meta.album.clone().unwrap_or_else(|| {
            folder
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Unknown album".into())
        });
        let artist = meta
            .album_artist
            .clone()
            .or(meta.artist.clone())
            .unwrap_or_else(|| "Unknown artist".into());
        albums.push(AlbumEntry {
            folder,
            title,
            artist,
            cover_override: None,
        });
    }
    albums.sort_by(|a, b| {
        a.artist
            .to_lowercase()
            .cmp(&b.artist.to_lowercase())
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });
    albums
}

fn empty_album_tile() -> AlbumTileData {
    AlbumTileData {
        title: SharedString::from(""),
        artist: SharedString::from(""),
        cover: Image::default(),
        album_index: -1,
    }
}

fn cover_image_for_album_entry(entry: &AlbumEntry) -> Image {
    if let Some(ref bytes) = entry.cover_override {
        if let Some(img) = bytes_to_slint_image(bytes) {
            return img;
        }
    }
    let Some(track) = first_audio_in_folder(&entry.folder) else {
        return Image::default();
    };
    let Ok((meta, _)) = probe_metadata(&track) else {
        return Image::default();
    };
    artwork_for_track(&meta, &track)
}

fn album_rows_from_library(albums: &[AlbumEntry]) -> Vec<AlbumRow> {
    let empty = empty_album_tile();
    albums
        .chunks(4)
        .enumerate()
        .map(|(row_i, chunk)| {
            let mut tiles = [
                empty.clone(),
                empty.clone(),
                empty.clone(),
                empty.clone(),
            ];
            for (col, album) in chunk.iter().enumerate() {
                let global = row_i * 4 + col;
                tiles[col] = AlbumTileData {
                    title: SharedString::from(album.title.as_str()),
                    artist: SharedString::from(album.artist.as_str()),
                    cover: cover_image_for_album_entry(album),
                    album_index: global as i32,
                };
            }
            AlbumRow {
                c0: tiles[0].clone(),
                c1: tiles[1].clone(),
                c2: tiles[2].clone(),
                c3: tiles[3].clone(),
            }
        })
        .collect()
}

fn refresh_album_rows(app: &PlayerWindow, library: &Mutex<Vec<AlbumEntry>>) {
    let albums = library.lock().unwrap();
    let rows = album_rows_from_library(&albums);
    app.set_album_rows(ModelRc::new(VecModel::from(rows)));
}

fn spawn_album_cover_backfill(
    app_weak: slint::Weak<PlayerWindow>,
    library: Arc<Mutex<Vec<AlbumEntry>>>,
    library_seq: Arc<AtomicU64>,
    expected_seq: u64,
) {
    std::thread::spawn(move || {
        let n = library.lock().map(|v| v.len()).unwrap_or(0);
        for i in 0..n {
            if library_seq.load(Ordering::Relaxed) != expected_seq {
                return;
            }
            let need_mb = {
                let lib = library.lock().unwrap();
                let Some(entry) = lib.get(i) else {
                    continue;
                };
                if entry.cover_override.is_some() {
                    false
                } else if let Some(track) = first_audio_in_folder(&entry.folder) {
                    match probe_metadata(&track) {
                        Ok((meta, _)) => {
                            meta.artwork.is_none()
                                && load_folder_cover_bytes(&entry.folder).is_none()
                        }
                        Err(_) => true,
                    }
                } else {
                    false
                }
            };
            if !need_mb {
                continue;
            }
            let (artist, album_title) = {
                let lib = library.lock().unwrap();
                let Some(e) = lib.get(i) else {
                    continue;
                };
                (e.artist.clone(), e.title.clone())
            };

            let mbid = match online_metadata::lookup_release_mbid(&artist, &album_title) {
                Ok(Some(m)) => m,
                Ok(None) => continue,
                Err(e) => {
                    log::warn!("MusicBrainz lookup: {e}");
                    continue;
                }
            };

            let Ok(Some(bytes)) = online_metadata::fetch_cover_art_bytes(&mbid) else {
                continue;
            };

            if library_seq.load(Ordering::Relaxed) != expected_seq {
                return;
            }
            {
                let mut lib = library.lock().unwrap();
                if let Some(entry) = lib.get_mut(i) {
                    entry.cover_override = Some(bytes);
                }
            }

            let app_w = app_weak.clone();
            let lib_arc = library.clone();
            let seq_arc = library_seq.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if seq_arc.load(Ordering::Relaxed) != expected_seq {
                    return;
                }
                if let Some(app) = app_w.upgrade() {
                    refresh_album_rows(&app, &lib_arc);
                }
            });
        }
    });
}

fn ui_apply_if_current<F>(
    app_weak: &slint::Weak<PlayerWindow>,
    seq: &Arc<AtomicU64>,
    expected: u64,
    f: F,
) where
    F: FnOnce(&PlayerWindow) + Send + 'static,
{
    let app_w = app_weak.clone();
    let seq_arc = seq.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if seq_arc.load(Ordering::Relaxed) != expected {
            return;
        }
        if let Some(app) = app_w.upgrade() {
            f(&app);
        }
    });
}

fn spawn_musicbrainz_track_enrichment(
    app_weak: slint::Weak<PlayerWindow>,
    artist: String,
    album: String,
    had_local_art: bool,
    track_seq: Arc<AtomicU64>,
    expected_seq: u64,
) {
    std::thread::spawn(move || {
        // Pre-check before doing any network I/O.
        if track_seq.load(Ordering::Relaxed) != expected_seq {
            return;
        }

        let mbid = match online_metadata::lookup_release_mbid(&artist, &album) {
            Ok(Some(m)) => m,
            Ok(None) => {
                ui_apply_if_current(&app_weak, &track_seq, expected_seq, |app| {
                    app.set_mb_status(SharedString::from(""));
                });
                return;
            }
            Err(e) => {
                log::warn!("MusicBrainz: {e}");
                ui_apply_if_current(&app_weak, &track_seq, expected_seq, |app| {
                    app.set_mb_status(SharedString::from(""));
                });
                return;
            }
        };

        if track_seq.load(Ordering::Relaxed) != expected_seq {
            return;
        }

        if !had_local_art {
            if let Ok(Some(bytes)) = online_metadata::fetch_cover_art_bytes(&mbid) {
                ui_apply_if_current(&app_weak, &track_seq, expected_seq, move |app| {
                    if let Some(img) = bytes_to_slint_image(&bytes) {
                        app.set_album_artwork(img);
                        app.set_mb_status(SharedString::from(
                            "Artwork: Cover Art Archive (release match)",
                        ));
                    }
                });
            }
        }

        if track_seq.load(Ordering::Relaxed) != expected_seq {
            return;
        }

        if let Ok(Some((title, credit, date))) = online_metadata::fetch_release_summary(&mbid) {
            ui_apply_if_current(&app_weak, &track_seq, expected_seq, move |app| {
                if app.get_track_title().is_empty() && !title.is_empty() {
                    app.set_track_title(SharedString::from(title));
                }
                if app.get_track_artist().is_empty() && !credit.is_empty() {
                    app.set_track_artist(SharedString::from(credit));
                }
                if app.get_track_year().is_empty() && date.len() >= 4 {
                    let y: String = date.chars().take(4).collect();
                    if y.chars().all(|c| c.is_ascii_digit()) {
                        app.set_track_year(SharedString::from(y));
                    }
                }
                if app.get_mb_status().is_empty() {
                    app.set_mb_status(SharedString::from("Credits/date: MusicBrainz"));
                }
            });
        } else {
            ui_apply_if_current(&app_weak, &track_seq, expected_seq, |app| {
                if app.get_mb_status().is_empty() {
                    app.set_mb_status(SharedString::from(""));
                }
            });
        }
    });
}

fn apply_metadata_to_ui(app: &PlayerWindow, meta: &TrackMetadata, filename: &str, info: decoder::AudioInfo) {
    app.set_track_title(SharedString::from(
        meta.title.as_deref().unwrap_or(filename),
    ));
    app.set_track_artist(SharedString::from(meta.artist.as_deref().unwrap_or("")));
    app.set_track_album(SharedString::from(meta.album.as_deref().unwrap_or("")));
    app.set_track_album_artist(SharedString::from(
        meta.album_artist.as_deref().unwrap_or(""),
    ));
    app.set_track_year(SharedString::from(meta.year.as_deref().unwrap_or("")));
    app.set_track_genre(SharedString::from(meta.genre.as_deref().unwrap_or("")));
    app.set_track_composer(SharedString::from(meta.composer.as_deref().unwrap_or("")));
    let pos_label = match (meta.track_number, meta.track_total) {
        (Some(n), Some(t)) => format!("Track {n} / {t}"),
        (Some(n), None) => format!("Track {n}"),
        _ => String::new(),
    };
    app.set_track_position_label(SharedString::from(pos_label));
    app.set_current_file(SharedString::from(filename));
    app.set_file_info(SharedString::from(format!(
        "{} Hz, {} bit, {} ch",
        info.sample_rate, info.bit_depth, info.channels
    )));
    app.set_duration(info.duration as f32);
}

fn load_track(
    app: &PlayerWindow,
    audio_engine: &Arc<Mutex<AudioEngine>>,
    playlist_arc: &Arc<Mutex<Playlist>>,
    track_seq: &Arc<AtomicU64>,
    index: usize,
    play: bool,
) -> Result<()> {
    let path = {
        let pl = playlist_arc.lock().unwrap();
        pl.tracks.get(index).map(|t| t.path.clone())
    };
    let Some(path) = path else {
        return Ok(());
    };

    let decoder = FileDecoder::open(&path)?;
    let info = *decoder.get_info();
    let meta = decoder.get_metadata().clone();
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Bump sequence so any in-flight MusicBrainz lookup for the previous track
    // is silently dropped before it can write into the new track's UI.
    let new_seq = track_seq.fetch_add(1, Ordering::Relaxed) + 1;

    let path_str = path.to_string_lossy().to_string();
    {
        let mut engine = audio_engine.lock().unwrap();
        engine.load_decoder(decoder, path_str)?;
        if play {
            engine.play()?;
        } else {
            engine.pause();
        }
    }

    apply_metadata_to_ui(app, &meta, &filename, info);

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let had_local_art = meta.artwork.is_some() || load_folder_cover_bytes(parent).is_some();

    app.set_album_artwork(artwork_for_track(&meta, &path));
    app.set_position(0.0);

    {
        let mut pl = playlist_arc.lock().unwrap();
        pl.current = Some(index);
    }
    app.set_current_track_index(index as i32);
    app.set_is_playing(play);

    if play {
        let engine = audio_engine.lock().unwrap();
        update_playback_info(app, &engine);
    }

    let artist_mb = meta
        .album_artist
        .clone()
        .or(meta.artist.clone())
        .unwrap_or_default();
    let album_mb = meta.album.clone().unwrap_or_default();
    if !artist_mb.trim().is_empty() && !album_mb.trim().is_empty() {
        app.set_mb_status(SharedString::from("MusicBrainz: looking up release…"));
        spawn_musicbrainz_track_enrichment(
            app.as_weak(),
            artist_mb,
            album_mb,
            had_local_art,
            track_seq.clone(),
            new_seq,
        );
    } else {
        app.set_mb_status(SharedString::from(""));
    }

    Ok(())
}

fn main() -> Result<()> {
    env_logger::init();

    let app = PlayerWindow::new()?;
    let app_weak = app.as_weak();

    let audio_engine = Arc::new(Mutex::new(AudioEngine::new()?));
    let audio_engine_timer = audio_engine.clone();

    let playlist = Arc::new(Mutex::new(Playlist {
        tracks: vec![],
        current: None,
    }));
    let playlist_load = playlist.clone();
    let playlist_prev = playlist.clone();
    let playlist_next = playlist.clone();
    let playlist_play = playlist.clone();
    let playlist_timer = playlist.clone();
    let playlist_open_album = playlist.clone();

    let library = Arc::new(Mutex::new(Vec::<AlbumEntry>::new()));
    let library_seq = Arc::new(AtomicU64::new(0));
    let library_scan = library.clone();
    let library_open = library.clone();

    let track_seq = Arc::new(AtomicU64::new(0));

    update_device_list(&app, &audio_engine)?;

    app.on_load_file({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        let playlist = playlist_load.clone();
        let track_seq = track_seq.clone();
        move || {
            let Some(path) = rfd::FileDialog::new()
                .add_filter("Audio Files", &["flac", "wav", "aiff", "aif", "m4a", "mp3"])
                .pick_file()
            else {
                return;
            };

            if let Some(app) = app_weak.upgrade() {
                app.set_current_file(SharedString::from("Loading folder…"));
            }

            let app_w = app_weak.clone();
            let engine = audio_engine.clone();
            let pl = playlist.clone();
            let seq = track_seq.clone();
            std::thread::spawn(move || match build_playlist(&path) {
                Ok((built, idx)) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = app_w.upgrade() {
                            {
                                let mut g = pl.lock().unwrap();
                                *g = built;
                            }
                            push_tracks_model(&app, &pl.lock().unwrap());
                            if let Err(e) = load_track(&app, &engine, &pl, &seq, idx, false) {
                                app.set_current_file(SharedString::from("Error loading file"));
                                app.set_file_info(SharedString::from(format!("{e}")));
                                log::error!("load_track: {e:?}");
                            }
                        }
                    });
                }
                Err(e) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = app_w.upgrade() {
                            app.set_current_file(SharedString::from("Error loading file"));
                            app.set_file_info(SharedString::from(format!("{e}")));
                        }
                    });
                }
            });
        }
    });

    app.on_scan_music_library({
        let app_weak = app_weak.clone();
        let library = library_scan.clone();
        let library_seq = library_seq.clone();
        move || {
            let Some(folder) = rfd::FileDialog::new().pick_folder() else {
                return;
            };
            // Cancel any previous scan/backfill before kicking off a new one.
            let expected_seq = library_seq.fetch_add(1, Ordering::Relaxed) + 1;

            let app_w = app_weak.clone();
            let lib_arc = library.clone();
            let seq_arc = library_seq.clone();
            std::thread::spawn(move || {
                let roots = discover_album_roots(&folder);
                let entries = build_album_entries(roots);

                if seq_arc.load(Ordering::Relaxed) != expected_seq {
                    return;
                }
                {
                    let mut lib = lib_arc.lock().unwrap();
                    *lib = entries;
                }
                let lib_for_ui = lib_arc.clone();
                let app_w_for_ui = app_w.clone();
                let seq_for_ui = seq_arc.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if seq_for_ui.load(Ordering::Relaxed) != expected_seq {
                        return;
                    }
                    if let Some(app) = app_w_for_ui.upgrade() {
                        refresh_album_rows(&app, &lib_for_ui);
                        app.set_active_view(1);
                        spawn_album_cover_backfill(
                            app_w_for_ui.clone(),
                            lib_for_ui,
                            seq_for_ui,
                            expected_seq,
                        );
                    }
                });
            });
        }
    });

    app.on_open_album({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        let playlist = playlist_open_album.clone();
        let library = library_open.clone();
        let track_seq = track_seq.clone();
        move |idx: i32| {
            if idx < 0 {
                return;
            }
            let folder = {
                let lib = library.lock().unwrap();
                lib.get(idx as usize).map(|e| e.folder.clone())
            };
            let Some(folder) = folder else {
                return;
            };
            let Some(track) = first_audio_in_folder(&folder) else {
                return;
            };

            let app_w = app_weak.clone();
            let engine = audio_engine.clone();
            let pl_arc = playlist.clone();
            let seq = track_seq.clone();
            std::thread::spawn(move || match build_playlist(&track) {
                Ok((built, ix)) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = app_w.upgrade() {
                            {
                                let mut g = pl_arc.lock().unwrap();
                                *g = built;
                            }
                            push_tracks_model(&app, &pl_arc.lock().unwrap());
                            app.set_active_view(0);
                            let _ = load_track(&app, &engine, &pl_arc, &seq, ix, true);
                        }
                    });
                }
                Err(e) => log::warn!("open album: {e}"),
            });
        }
    });

    app.on_play({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        move || {
            if let Some(app) = app_weak.upgrade() {
                let mut engine = audio_engine.lock().unwrap();
                if engine.play().is_ok() {
                    app.set_is_playing(true);
                    update_playback_info(&app, &engine);
                }
            }
        }
    });

    app.on_pause({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        move || {
            if let Some(app) = app_weak.upgrade() {
                let mut engine = audio_engine.lock().unwrap();
                engine.pause();
                app.set_is_playing(false);
            }
        }
    });

    app.on_stop({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        move || {
            if let Some(app) = app_weak.upgrade() {
                let mut engine = audio_engine.lock().unwrap();
                engine.stop();
                app.set_is_playing(false);
                app.set_position(0.0);
            }
        }
    });

    app.on_seek({
        let audio_engine = audio_engine.clone();
        move |position| {
            let mut engine = audio_engine.lock().unwrap();
            engine.seek(position as f64);
        }
    });

    app.on_volume_changed({
        let audio_engine = audio_engine.clone();
        move |volume| {
            let mut engine = audio_engine.lock().unwrap();
            engine.set_volume(volume);
        }
    });

    app.on_device_changed({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        move |device_name| {
            let mut engine = audio_engine.lock().unwrap();
            let devices = engine.enumerate_devices().unwrap_or_default();

            if let Some(device) = devices.iter().find(|d| d.name == device_name.as_str()) {
                if engine.select_device(device.id).is_ok() {
                    if let Some(app) = app_weak.upgrade() {
                        update_playback_info(&app, &engine);
                    }
                }
            }
        }
    });

    app.on_prev_track({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        let playlist = playlist_prev.clone();
        let track_seq = track_seq.clone();
        move || {
            if let Some(app) = app_weak.upgrade() {
                let pos = { audio_engine.lock().unwrap().get_position() };
                if pos > 3.0 {
                    let mut engine = audio_engine.lock().unwrap();
                    engine.seek(0.0);
                    app.set_position(0.0);
                } else {
                    let prev = {
                        let pl = playlist.lock().unwrap();
                        pl.current.and_then(|c| c.checked_sub(1))
                    };
                    if let Some(pi) = prev {
                        let _ = load_track(&app, &audio_engine, &playlist, &track_seq, pi, true);
                    } else {
                        let mut engine = audio_engine.lock().unwrap();
                        engine.seek(0.0);
                        app.set_position(0.0);
                    }
                }
            }
        }
    });

    app.on_next_track({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        let playlist = playlist_next.clone();
        let track_seq = track_seq.clone();
        move || {
            if let Some(app) = app_weak.upgrade() {
                let next = {
                    let pl = playlist.lock().unwrap();
                    pl.current.and_then(|c| {
                        let n = c + 1;
                        if n < pl.tracks.len() {
                            Some(n)
                        } else {
                            None
                        }
                    })
                };
                if let Some(ni) = next {
                    let _ = load_track(&app, &audio_engine, &playlist, &track_seq, ni, true);
                }
            }
        }
    });

    app.on_play_track({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        let playlist = playlist_play.clone();
        let track_seq = track_seq.clone();
        move |index: i32| {
            if let Some(app) = app_weak.upgrade() {
                if index >= 0 {
                    let _ = load_track(
                        &app,
                        &audio_engine,
                        &playlist,
                        &track_seq,
                        index as usize,
                        true,
                    );
                }
            }
        }
    });

    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), {
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine_timer;
        let playlist = playlist_timer;
        let track_seq = track_seq.clone();
        move || {
            if let Some(app) = app_weak.upgrade() {
                let (position, volume, should_advance) = {
                    let engine = audio_engine.lock().unwrap();
                    let pos = engine.get_position() as f32;
                    let vol = *engine.get_volume().lock().unwrap();
                    let advance = app.get_is_playing() && engine.should_stop_playback();
                    (pos, vol, advance)
                };

                app.set_position(position);
                app.set_volume(volume);

                if should_advance {
                    let next_idx = {
                        let pl = playlist.lock().unwrap();
                        match pl.current {
                            Some(c) if c + 1 < pl.tracks.len() => Some(c + 1),
                            _ => None,
                        }
                    };
                    if let Some(ni) = next_idx {
                        if let Err(e) =
                            load_track(&app, &audio_engine, &playlist, &track_seq, ni, true)
                        {
                            log::error!("auto-advance: {e:?}");
                            let mut engine = audio_engine.lock().unwrap();
                            engine.pause();
                            app.set_is_playing(false);
                        }
                    } else {
                        let mut engine = audio_engine.lock().unwrap();
                        engine.pause();
                        app.set_is_playing(false);
                    }
                } else if app.get_is_playing() {
                    let engine = audio_engine.lock().unwrap();
                    update_playback_info(&app, &engine);
                }
            }
        }
    });

    app.run()?;
    Ok(())
}

fn update_device_list(app: &PlayerWindow, audio_engine: &Arc<Mutex<AudioEngine>>) -> Result<()> {
    let engine = audio_engine.lock().unwrap();
    let devices = engine.enumerate_devices()?;

    let device_names: Vec<SharedString> = devices
        .iter()
        .map(|d| SharedString::from(&d.name))
        .collect();

    let model = ModelRc::new(VecModel::from(device_names));
    app.set_audio_devices(model);

    Ok(())
}

fn update_playback_info(app: &PlayerWindow, engine: &AudioEngine) {
    let (buffer_used, buffer_capacity, buffer_percentage) = engine.get_buffer_stats();
    let underrun_count = engine.get_underrun_count();

    let info = format!(
        "{}Hz {}bit | Buffer: {:.1}% ({}/{}) | Underruns: {} | {}",
        engine.get_sample_rate(),
        engine.get_bit_depth(),
        buffer_percentage,
        buffer_used,
        buffer_capacity,
        underrun_count,
        if engine.is_exclusive_mode() {
            "Exclusive"
        } else {
            "Shared"
        }
    );
    app.set_playback_info(SharedString::from(info));
}
