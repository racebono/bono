//! Track descriptions (turns with names, direction and tightness, straights, sectors, pit lane,
//! time lost in the pits) from Lovely Sim Racing's lovely-track-data
//! (https://github.com/Lovely-Sim-Racing/lovely-track-data, CC BY-NC-SA 4.0). Each track is
//! downloaded once and kept in a cache folder; the list of tracks at most once a day.
//!
//! A track the simulator's data hasn't got can be borrowed from another simulator's data for the
//! same layout (see `BORROWED`).

use crate::{error, sim::Sim, sqlite::pool::Pool, task::Task};
use anyhow::Context;
use lambiase_i18n::t;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Deserializer};
use std::{
    collections::HashMap,
    hint::unreachable_unchecked,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    runtime::Handle,
    sync::Mutex,
};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

const DATA_URL: &str =
    "https://raw.githubusercontent.com/Lovely-Sim-Racing/lovely-track-data/main/data/";

/// A download that takes longer is given up on: the session goes on without the description.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15);
/// How old the cached list of tracks may be before it's fetched again (new tracks get added).
const MANIFEST_MAX_AGE: Duration = Duration::from_hours(24);

/// Where the engineer tool reads the session's track description: next to the database.
pub const TRACK_FILE: &str = "track.json";

/// A track a simulator's data hasn't got, and the same layout in another simulator's data.
struct Borrowed {
    /// The simulator, and its track as it names it
    sim: &'static str,
    track: &'static str,
    /// The other simulator, and the layout's name or id in its data
    from: &'static str,
    from_name: &'static str,
    layout: &'static str,
}

/// Tracks borrowed from another simulator's data. Each pair is the same layout, checked by hand
/// (turns, pit lane and length): a venue's other layouts are left out. Left out too: Daytona's and
/// Long Beach's iRacing data (turn markers only, without where the turns start and end), and
/// Paul Ricard (no other simulator's data).
const BORROWED: &[Borrowed] = &[
    // Today's layout, without the chicane before the last corner: iRacing's "historic" one
    Borrowed {
        sim: "lmu",
        track: "Circuit de Barcelona",
        from: "iracing",
        from_name: "iRacing",
        layout: "barcelona historic",
    },
    Borrowed {
        sim: "lmu",
        track: "WeatherTech Raceway Laguna Seca",
        from: "iracing",
        from_name: "iRacing",
        layout: "lagunaseca",
    },
    Borrowed {
        sim: "lmu",
        track: "Michelin Raceway Road Atlanta",
        from: "iracing",
        from_name: "iRacing",
        layout: "roadatlanta full",
    },
    // WEC's and ELMS's are both the Arena Grand Prix circuit
    Borrowed {
        sim: "lmu",
        track: "Silverstone Grand Prix Circuit - WEC",
        from: "iracing",
        from_name: "iRacing",
        layout: "silverstone 2019 gp",
    },
    Borrowed {
        sim: "lmu",
        track: "Silverstone Grand Prix Circuit - ELMS",
        from: "iracing",
        from_name: "iRacing",
        layout: "silverstone 2019 gp",
    },
];

/// How much a borrowed layout's length may differ from the simulator's lap: more, and it's taken
/// for another layout (the data's lengths are measured differently, and are a little off).
const SAME_LAYOUT: f64 = 0.03;

/// One simulator's track descriptions: the last one asked for is kept, so a session asking again
/// costs nothing.
pub struct TrackData {
    sim: &'static str,
    /// The track last asked for, as the simulator names it, and its description (`None`: the data
    /// hasn't got it)
    last: Option<(String, Option<Arc<serde_json::Value>>)>,
}

impl TrackData {
    #[inline]
    pub fn new(sim: &'static str) -> Self {
        Self { sim, last: None }
    }

    /// The description of the simulator's track, downloaded first if it isn't cached yet; `None`
    /// when the data hasn't got the track. `sim` is the simulator's id in the data (SimHub's game
    /// id, e.g. "lmu"); `name` the track as the simulator gives it, matched ignoring case, accents
    /// and punctuation, and `length` its lap in metres (for a borrowed track). The description
    /// carries `source_track`: the `name` it was found for.
    pub async fn get(
        &mut self,
        manifest: &mut Manifest,
        name: &str,
        length: Option<f64>,
    ) -> anyhow::Result<Option<Arc<serde_json::Value>>> {
        if let Some((last, track)) = &self.last
            && last == name
        {
            return Ok(track.clone());
        }

        // A failure isn't kept: the next session tries again
        let track = match manifest.track(self.sim, name).await? {
            Some(track) => Some(track),
            None => borrow(manifest, self.sim, name, length).await?,
        };
        let track = track.map(|mut track| {
            if let Some(track) = track.as_object_mut() {
                track.insert("source_track".into(), name.into());
            }
            Arc::new(track)
        });

        self.last = Some((name.to_owned(), track.clone()));
        return Ok(track);
    }

    /// Saves the description of the session's track as `track.json` in `dir` (next to the
    /// database), where the engineer tool reads it; without one, removes the file so a
    /// description left from another track isn't used.
    pub async fn save(track: Option<&serde_json::Value>, dir: &Path) -> std::io::Result<()> {
        let path = dir.join(TRACK_FILE);
        match track {
            Some(track) => write_whole(&path, &serde_json::to_vec_pretty(track)?).await,
            None => match tokio::fs::remove_file(&path).await {
                Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
        }
    }
}

/// The same layout from another simulator's data (see `BORROWED`), if it has it and its length
/// agrees with the simulator's lap. Positions in the data are fractions of the lap from the
/// start/finish line, so they carry over: rescaled to this lap's length, a turn is as far along it
/// as along the other's. The description says where it's from (`borrowed`), and takes the
/// simulator's `length`. Where the other's lap starts somewhere else, every turn is that far off:
/// the engineer tool lines the turns up with the car's laps (its `calibrate`).
async fn borrow(
    manifest: &mut Manifest,
    sim: &str,
    name: &str,
    length: Option<f64>,
) -> anyhow::Result<Option<serde_json::Value>> {
    let normalized = normalize(name);
    let Some(borrowed) = BORROWED
        .iter()
        .find(|borrowed| borrowed.sim == sim && normalize(borrowed.track) == normalized)
    else {
        return Ok(None);
    };

    let Some(mut track) = manifest.track(borrowed.from, borrowed.layout).await? else {
        log::warn!(
            "{}'s track data no longer has {} for {name}",
            borrowed.from_name,
            borrowed.layout
        );
        return Ok(None);
    };

    let theirs = track.get("length").and_then(serde_json::Value::as_f64);
    let (Some(length), Some(theirs)) = (
        length.filter(|length| *length > 0.0),
        theirs.filter(|theirs| *theirs > 0.0),
    ) else {
        log::info!("Not borrowing {name}'s description: its length isn't known");
        return Ok(None);
    };
    if (theirs - length).abs() / length > SAME_LAYOUT {
        log::warn!(
            "Not borrowing {name}'s description: {}'s {} is {theirs:.0} m, the lap {length:.0} m \
             (another layout?)",
            borrowed.from_name,
            borrowed.layout
        );
        return Ok(None);
    }

    let Some(object) = track.as_object_mut() else {
        return Ok(None);
    };
    let layout = object
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(borrowed.layout)
        .to_owned();
    object.insert(
        "borrowed".into(),
        serde_json::json!({
            "sim": borrowed.from_name,
            "layout": layout,
            "length": theirs,
        }),
    );
    object.insert("length".into(), length.into());
    return Ok(Some(track));
}

/// Keeps `track.json` next to the running simulator's database describing its session's track,
/// for the engineer tool: looks every few seconds at the session's track, and when it changes
/// (or the database moved), fetches its description and saves it.
pub struct TrackSync {
    /// `None` if the cache folder couldn't be opened: no track descriptions then
    manifest: Option<Arc<Mutex<Manifest>>>,
    task: Option<Task<anyhow::Result<Option<Synced>>>>,
    next_check: Instant,
    /// The database folder and track `track.json` was last saved for
    synced: Option<Synced>,
}

type Synced = (PathBuf, String);

impl TrackSync {
    /// How often the session's track is looked at.
    const CHECK_INTERVAL: Duration = Duration::from_secs(5);
    /// How long after a failure (e.g. no connection) before trying again.
    const RETRY_INTERVAL: Duration = Duration::from_secs(60);

    pub async fn new(client: reqwest::Client, pref_path: &Path) -> Self {
        let manifest = match Manifest::new(client, pref_path).await {
            Ok(manifest) => Some(Arc::new(Mutex::new(manifest))),
            Err(e) => {
                log::error!("Error opening the track data cache, going without it: {e}");
                None
            }
        };

        Self {
            manifest,
            task: None,
            next_check: Instant::now(),
            synced: None,
        }
    }

    /// Called every frame with the running simulator, if any.
    pub fn update(&mut self, rt: &Handle, sim: Option<&mut Sim>) {
        if let Some(task) = &mut self.task {
            let Some(result) = task.update(rt) else {
                return;
            };
            self.task = None;
            match result {
                Ok(Some(synced)) => {
                    log::info!("Track description saved for {}", synced.1);
                    self.synced = Some(synced);
                }
                Ok(None) => {}
                Err(e) => {
                    log::warn!("Error getting the track's description: {e:#}");
                    self.next_check = Instant::now() + Self::RETRY_INTERVAL;
                }
            }
        }

        if Instant::now() < self.next_check {
            return;
        }
        self.next_check = Instant::now() + Self::CHECK_INTERVAL;

        let (Some(manifest), Some(sim)) = (&self.manifest, sim) else {
            return;
        };
        let track_data = sim.track_data().clone();
        let Some(db) = sim.database().cloned() else {
            return;
        };
        self.task = Some(Task::spawn(sync(
            db,
            track_data,
            manifest.clone(),
            self.synced.clone(),
        )));
    }
}

/// Saves the description of the session's track next to the database, unless it was already
/// saved there (`synced`). What was saved, if anything.
async fn sync(
    db: Pool,
    track_data: Arc<Mutex<TrackData>>,
    manifest: Arc<Mutex<Manifest>>,
    synced: Option<Synced>,
) -> anyhow::Result<Option<Synced>> {
    let dir = db
        .database()
        .path()
        .parent()
        .context("The database has no folder")?
        .to_owned();

    let conn = db.get().await?;
    let session = conn
        .run(|conn| {
            conn.query_row(
                "SELECT track, track_length FROM session WHERE track IS NOT NULL ORDER BY id DESC \
                 LIMIT 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?)),
            )
            .optional()
        })
        .await?;
    drop(conn);

    // No session yet
    let Some((name, length)) = session else {
        return Ok(None);
    };
    let key = (dir, name);
    if synced.as_ref() == Some(&key) {
        return Ok(None);
    }

    let track = {
        let mut track_data = track_data.lock().await;
        let mut manifest = manifest.lock().await;
        track_data.get(&mut manifest, &key.1, length).await?
    };
    TrackData::save(track.as_deref(), &key.0).await?;
    if track.is_none() {
        log::info!("No track description for {}", key.1);
    }
    return Ok(Some(key));
}

/// The list of tracks, shared by every simulator, and the cache folder of their descriptions.
pub struct Manifest {
    client: reqwest::Client,
    /// The cache folder
    dir: PathBuf,
    /// The cached list, or while a new one is being written to it, the way to get it back
    file: Result<tokio::fs::File, tokio::sync::oneshot::Receiver<tokio::fs::File>>,
    manifest: Option<ManifestBody>,
}

impl Manifest {
    pub(super) async fn new(client: reqwest::Client, pref_path: &Path) -> std::io::Result<Self> {
        let dir = pref_path.join("track");
        tokio::fs::create_dir_all(&dir).await?;
        let file = tokio::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("manifest.json"))
            .await?;

        Ok(Self {
            client,
            dir,
            manifest: None,
            file: Ok(file),
        })
    }

    /// The description of a simulator's track (see `TrackData::get`), from the cache folder or
    /// downloaded into it.
    async fn track(&mut self, sim: &str, name: &str) -> anyhow::Result<Option<serde_json::Value>> {
        let Some(path) = self.get().await?.find_path(sim, name).map(str::to_owned) else {
            return Ok(None);
        };
        let relative = safe_relative(&path).context("The track's path in the data isn't valid")?;
        let file = self.dir.join(relative);

        let text = match tokio::fs::read(&file).await {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let text = self
                    .download(&path)
                    .await
                    .with_context(|| format!("Error downloading the description of {name}"))?;
                serde_json::from_str::<serde_json::Value>(&text)
                    .with_context(|| format!("The downloaded description of {name} isn't valid"))?;
                write_whole(&file, text.as_bytes()).await?;
                text.into_bytes()
            }
            Err(e) => return Err(e.into()),
        };

        return Ok(Some(serde_json::from_slice(&text)?));
    }

    /// The list of tracks: the cached one while it's fresh, otherwise downloaded (or the old one
    /// if that fails).
    async fn get(&mut self) -> anyhow::Result<&ManifestBody> {
        let file = self.file().await?;
        let meta = file.metadata().await?;
        let is_outdated = meta.len() == 0
            || !meta
                .modified()?
                .elapsed()
                .is_ok_and(|elapsed| elapsed < MANIFEST_MAX_AGE);

        if !is_outdated {
            if self.manifest.is_none() {
                // A file left half written (the app stopped mid-write) is downloaded again
                match serde_json::from_str(&self.read().await?) {
                    Ok(manifest) => self.manifest = Some(manifest),
                    Err(e) => log::warn!("Cached list of tracks isn't valid, downloading it: {e}"),
                }
            }
            if self.manifest.is_some() {
                return Ok(unsafe { self.manifest.as_ref().unwrap_unchecked() });
            }
        }

        match self.download("manifest.json").await {
            Ok(text) => {
                let manifest = serde_json::from_str(&text)
                    .context("The downloaded list of tracks isn't valid")?;
                self.save(text.into_bytes());
                self.manifest = Some(manifest);
            }
            // The one already loaded stays until the next try
            Err(e) if self.manifest.is_some() => {
                log::warn!("Error updating the list of tracks: {e:#}");
            }
            Err(e) => {
                let text = self.read().await?;
                if text.is_empty() {
                    return Err(e.context("Error downloading the list of tracks"));
                }
                log::warn!("Error updating the list of tracks, using the old one: {e:#}");
                self.manifest = Some(serde_json::from_str(&text)?);
            }
        }

        return Ok(unsafe { self.manifest.as_ref().unwrap_unchecked() });
    }

    /// The cached list's file, once a write of it in the background has finished.
    async fn file(&mut self) -> anyhow::Result<&mut tokio::fs::File> {
        if let Err(ref mut recv) = self.file {
            self.file = Ok(recv
                .await
                .context("The cached list of tracks was lost while saving it")?);
        }

        match self.file {
            Ok(ref mut file) => Ok(file),
            _ => unsafe { unreachable_unchecked() },
        }
    }

    async fn read(&mut self) -> anyhow::Result<String> {
        let file = self.file().await?;
        file.rewind().await?;
        let mut text = String::new();
        file.read_to_string(&mut text).await?;
        return Ok(text);
    }

    /// Writes the list to its file in the background: the next `get` waits for it.
    fn save(&mut self, bytes: Vec<u8>) {
        let (send, recv) = tokio::sync::oneshot::channel();
        let mut file = match std::mem::replace(&mut self.file, Err(recv)) {
            Ok(file) => file,
            // `get` always waits for the file first
            _ => unsafe { unreachable_unchecked() },
        };

        tokio::spawn(async move {
            let write = async {
                file.rewind().await?;
                file.set_len(0).await?;
                file.write_all(&bytes).await?;
                file.flush().await
            };

            if let Err(e) = write.await {
                error(t!("error.saving_tracks"), e);
            }

            let _ = send.send(file);
        });
    }

    async fn download(&self, path: &str) -> anyhow::Result<String> {
        Ok(self
            .client
            .get(format!("{DATA_URL}{path}"))
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?)
    }
}

#[derive(Deserialize)]
struct ManifestBody {
    #[serde(default)]
    tracks: HashMap<String, Vec<ManifestTrack>>,
}

impl ManifestBody {
    /// The track's file in the data (relative to `data/`).
    fn find_path(&self, sim: &str, name: &str) -> Option<&str> {
        let name = normalize(name);
        self.tracks
            .get(sim)?
            .iter()
            .find(|track| {
                !track.path.is_empty() && (track.track_name == name || track.track_id == name)
            })
            .map(|track| track.path.as_str())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestTrack {
    #[serde(default, deserialize_with = "normalized")]
    pub track_name: String,
    #[serde(default, deserialize_with = "normalized")]
    pub track_id: String,
    #[serde(default)]
    pub path: String,
}

fn normalized<'de, D>(d: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(normalize(&String::deserialize(d)?))
}

/// A name for matching: lower case, accents dropped, anything but letters and digits as single
/// spaces.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;

    for c in s.nfd() {
        // É -> E + U+0301; drop the accent.
        if is_combining_mark(c) {
            continue;
        }
        // Latin-1 letters NFD leaves alone.
        let c = match c {
            'Æ' | 'æ' => 'a',
            'Ð' | 'ð' => 'd',
            'Ø' | 'ø' => 'o',
            'Þ' | 'þ' => 't',
            'ß' => 's',
            c => c,
        };
        if c.is_ascii_alphanumeric() {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(c.to_ascii_lowercase());
        } else {
            // ASCII punctuation, ×, ÷, and anything that didn't reduce to ASCII.
            space = true;
        }
    }

    return out;
}

/// The data's path as a relative path, refusing anything that would leave the cache folder.
fn safe_relative(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    let mut relative = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!relative.as_os_str().is_empty()).then_some(relative)
}

/// Writes a file whole, through a temporary one, so a reader never sees half of it.
async fn write_whole(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    tokio::fs::write(&part, bytes).await?;
    tokio::fs::rename(&part, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> ManifestBody {
        serde_json::from_value(serde_json::json!({
            "tracks": {
                "lmu": [
                    {
                        "trackName": "Autódromo José Carlos Pace",
                        "trackId": "interlagos",
                        "path": "lmu/autodromo-jose-carlos-pace.json"
                    },
                    {"trackName": "Circuit de la Sarthe", "path": "lmu/circuit-de-la-sarthe.json"},
                    {"trackName": "No file"}
                ]
            }
        }))
        .unwrap()
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(
            normalize("Autódromo José Carlos Pace"),
            "autodromo jose carlos pace"
        );
        assert_eq!(
            normalize("  Spa-Francorchamps (2024)!"),
            "spa francorchamps 2024"
        );
        assert_eq!(
            normalize("Nürburgring Nordschleife"),
            "nurburgring nordschleife"
        );
        assert_eq!(normalize("Øresund"), "oresund");
    }

    #[test]
    fn finds_tracks_by_name_or_id() {
        let manifest = manifest();
        assert_eq!(
            manifest.find_path("lmu", "AUTODROMO JOSE CARLOS PACE"),
            Some("lmu/autodromo-jose-carlos-pace.json")
        );
        assert_eq!(
            manifest.find_path("lmu", "Interlagos"),
            Some("lmu/autodromo-jose-carlos-pace.json")
        );
        assert_eq!(
            manifest.find_path("lmu", "circuit de la sarthe"),
            Some("lmu/circuit-de-la-sarthe.json")
        );
        assert_eq!(manifest.find_path("lmu", "No file"), None);
        assert_eq!(manifest.find_path("lmu", "Monza"), None);
        assert_eq!(manifest.find_path("iracing", "Interlagos"), None);
    }

    #[test]
    fn refuses_paths_out_of_the_cache() {
        assert_eq!(
            safe_relative("lmu/./spa.json"),
            Some(PathBuf::from("lmu").join("spa.json"))
        );
        assert_eq!(safe_relative("../secrets.json"), None);
        assert_eq!(safe_relative("lmu/../../secrets.json"), None);
        assert_eq!(safe_relative("/etc/passwd"), None);
        assert_eq!(safe_relative(""), None);
    }

    /// A cache folder (in `prefs`) holding `manifest` and `files`, as if just downloaded.
    async fn cached(
        prefs: &Path,
        manifest: serde_json::Value,
        files: &[(&str, serde_json::Value)],
    ) -> Manifest {
        let dir = prefs.join("track");
        for (path, file) in files {
            write_whole(&dir.join(path), file.to_string().as_bytes())
                .await
                .unwrap();
        }
        write_whole(&dir.join("manifest.json"), manifest.to_string().as_bytes())
            .await
            .unwrap();
        Manifest::new(reqwest::Client::new(), prefs).await.unwrap()
    }

    #[tokio::test]
    async fn borrows_the_same_layout_from_another_simulator() {
        let prefs = tempfile::tempdir().unwrap();
        let turn = |start: f64, end: f64, name: &str| serde_json::json!({"start": start, "end": end, "name": name});
        let mut manifest = cached(
            prefs.path(),
            serde_json::json!({
                "tracks": {
                    "lmu": [{"trackName": "Circuit de la Sarthe", "path": "lmu/sarthe.json"}],
                    "iracing": [
                        {
                            "trackName": "Barcelona Historic",
                            "trackId": "barcelona historic",
                            "path": "iracing/barcelona-historic.json"
                        },
                        {
                            "trackName": "Silverstone Circuit - Arena Grand Prix",
                            "trackId": "silverstone 2019 gp",
                            "path": "iracing/silverstone-2019-gp.json"
                        }
                    ]
                }
            }),
            &[
                (
                    "lmu/sarthe.json",
                    serde_json::json!({"name": "Circuit de la Sarthe", "length": 13623}),
                ),
                (
                    "iracing/barcelona-historic.json",
                    serde_json::json!({
                        "name": "Barcelona Historic",
                        "length": 4680,
                        "pitentry": 0.922,
                        "turn": [turn(0.05, 0.08, "Elf"), turn(0.9, 0.95, "New Holland")]
                    }),
                ),
                (
                    "iracing/silverstone-2019-gp.json",
                    serde_json::json!({"name": "Silverstone Circuit - Arena Grand Prix", "length": 5800}),
                ),
            ],
        )
        .await;

        // The same layout: its lap fractions, on this lap
        let track = TrackData::new("lmu")
            .get(&mut manifest, "Circuit de Barcelona", Some(4655.1))
            .await
            .unwrap()
            .expect("borrowed from iRacing");
        assert_eq!(track["source_track"], "Circuit de Barcelona");
        assert_eq!(track["length"], 4655.1);
        assert_eq!(track["turn"][0]["start"], 0.05);
        assert_eq!(track["turn"][1]["name"], "New Holland");
        assert_eq!(track["pitentry"], 0.922);
        assert_eq!(
            track["borrowed"],
            serde_json::json!({"sim": "iRacing", "layout": "Barcelona Historic", "length": 4680.0})
        );

        // Silverstone's National circuit isn't the Grand Prix one, whatever the name says
        assert!(
            TrackData::new("lmu")
                .get(
                    &mut manifest,
                    "Silverstone Grand Prix Circuit - WEC",
                    Some(2638.0)
                )
                .await
                .unwrap()
                .is_none()
        );
        // Without the lap's length, there's no telling
        assert!(
            TrackData::new("lmu")
                .get(&mut manifest, "Circuit de Barcelona", None)
                .await
                .unwrap()
                .is_none()
        );
        // Not one of the borrowed tracks
        assert!(
            TrackData::new("lmu")
                .get(&mut manifest, "Circuit Paul Ricard", Some(5800.0))
                .await
                .unwrap()
                .is_none()
        );
        // Nor for another simulator
        assert!(
            TrackData::new("iracing")
                .get(&mut manifest, "Circuit de Barcelona", Some(4655.1))
                .await
                .unwrap()
                .is_none()
        );

        // The simulator's own data comes first
        let track = TrackData::new("lmu")
            .get(&mut manifest, "Circuit de la Sarthe", Some(13624.0))
            .await
            .unwrap()
            .expect("in LMU's data");
        assert_eq!(track["length"], 13623);
        assert!(track.get("borrowed").is_none());
    }

    /// Every borrowed layout is in the real data, as long as LMU's lap (from its results files).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "downloads from GitHub"]
    async fn borrows_every_layout_from_the_real_data() {
        let lmu_lengths = [
            ("Circuit de Barcelona", 4655.1),
            ("WeatherTech Raceway Laguna Seca", 3590.4),
            ("Michelin Raceway Road Atlanta", 4083.5),
            ("Silverstone Grand Prix Circuit - WEC", 5869.2),
            ("Silverstone Grand Prix Circuit - ELMS", 5869.2),
        ];
        assert_eq!(BORROWED.len(), lmu_lengths.len());

        let prefs = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::new(reqwest::Client::new(), prefs.path())
            .await
            .unwrap();
        for (name, length) in lmu_lengths {
            assert!(
                manifest.track("lmu", name).await.unwrap().is_none(),
                "LMU's data has {name} now: no need to borrow it"
            );
            let track = TrackData::new("lmu")
                .get(&mut manifest, name, Some(length))
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("{name} wasn't borrowed"));
            println!(
                "{name} ({length} m): {} turns from {}",
                track["turn"].as_array().map_or(0, Vec::len),
                track["borrowed"]
            );
            assert!(
                track["turn"]
                    .as_array()
                    .is_some_and(|turns| turns.iter().all(|t| t["start"].is_number()))
            );
        }
    }

    /// Downloads the real list and a real track, into a temporary folder.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "downloads from GitHub"]
    async fn downloads_a_track() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::new(reqwest::Client::new(), dir.path())
            .await
            .unwrap();
        let mut data = TrackData::new("lmu");

        let track = data
            .get(&mut manifest, "Autodromo Jose Carlos Pace", None)
            .await
            .unwrap()
            .expect("Interlagos is in the data");
        assert_eq!(track["source_track"], "Autodromo Jose Carlos Pace");
        assert!(
            track["turn"]
                .as_array()
                .is_some_and(|turns| !turns.is_empty())
        );
        assert!(
            dir.path()
                .join("track/lmu/autodromo-jose-carlos-pace.json")
                .is_file()
        );

        assert!(
            data.get(&mut manifest, "Not A Track", None)
                .await
                .unwrap()
                .is_none()
        );

        // From the cache: a new list, loaded from its file
        let mut manifest = Manifest::new(reqwest::Client::new(), dir.path())
            .await
            .unwrap();
        assert!(
            manifest
                .track("lmu", "AUTÓDROMO JOSÉ CARLOS PACE")
                .await
                .unwrap()
                .is_some()
        );

        TrackData::save(Some(&track), dir.path()).await.unwrap();
        assert!(dir.path().join(TRACK_FILE).is_file());
        TrackData::save(None, dir.path()).await.unwrap();
        assert!(!dir.path().join(TRACK_FILE).exists());
    }

    /// A recorded session at Interlagos gets its description next to the database, once.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "downloads from GitHub"]
    async fn saves_the_session_track_next_to_the_database() {
        use crate::sqlite::Database;
        use lambiase_sim::{
            recorder::{DATABASE, Recorder},
            sys::lambiase_session_info,
        };

        let prefs = tempfile::tempdir().unwrap();
        let game = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(game.path(), None).unwrap();
        let mut session = lambiase_session_info::default();
        session.track = Some("Autódromo José Carlos Pace".to_owned()).into();
        recorder.push_scoring(session, Vec::new());
        drop(recorder);

        let one = std::num::NonZeroUsize::new(1).unwrap();
        let db = Pool::new(
            Database::read_only(Arc::new(game.path().join(DATABASE))),
            one,
            one,
        );
        let manifest = Arc::new(Mutex::new(
            Manifest::new(reqwest::Client::new(), prefs.path())
                .await
                .unwrap(),
        ));
        let track_data = Arc::new(Mutex::new(TrackData::new("lmu")));

        let synced = sync(db.clone(), track_data.clone(), manifest.clone(), None)
            .await
            .unwrap()
            .expect("saved");
        assert_eq!(synced.1, "Autódromo José Carlos Pace");

        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(game.path().join(TRACK_FILE)).unwrap()).unwrap();
        assert_eq!(saved["source_track"], "Autódromo José Carlos Pace");
        assert!(
            saved["turn"]
                .as_array()
                .is_some_and(|turns| !turns.is_empty())
        );

        // Already saved there: nothing to do
        assert!(
            sync(db, track_data, manifest, Some(synced))
                .await
                .unwrap()
                .is_none()
        );
    }
}
