//! Track descriptions (turns with names, direction and tightness, straights, sectors, pit lane,
//! time lost in the pits) from Lovely Sim Racing's lovely-track-data
//! (https://github.com/Lovely-Sim-Racing/lovely-track-data, CC BY-NC-SA 4.0). Each track is
//! downloaded once and kept in a cache folder; the list of tracks at most once a day.

use crate::{error, sim::Sim, sqlite::pool::Pool, task::Task};
use anyhow::Context;
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
    /// and punctuation. The description carries `source_track`: the `name` it was found for.
    pub async fn get(
        &mut self,
        manifest: &mut Manifest,
        name: &str,
    ) -> anyhow::Result<Option<Arc<serde_json::Value>>> {
        if let Some((last, track)) = &self.last
            && last == name
        {
            return Ok(track.clone());
        }

        // A failure isn't kept: the next session tries again
        let track = manifest.track(self.sim, name).await?.map(|mut track| {
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
    let name = conn
        .run(|conn| {
            conn.query_row(
                "SELECT track FROM session WHERE track IS NOT NULL ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
        })
        .await?;
    drop(conn);

    // No session yet
    let Some(name) = name else {
        return Ok(None);
    };
    let key = (dir, name);
    if synced.as_ref() == Some(&key) {
        return Ok(None);
    }

    let track = {
        let mut track_data = track_data.lock().await;
        let mut manifest = manifest.lock().await;
        track_data.get(&mut manifest, &key.1).await?
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
                error("Error saving the list of tracks", e);
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
            .get(&mut manifest, "Autodromo Jose Carlos Pace")
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
            data.get(&mut manifest, "Not A Track")
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
        use lambiase_sim::{recorder::{DATABASE, Recorder}, sys::lambiase_session_info};

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
            Manifest::new(reqwest::Client::new(), prefs.path()).await.unwrap(),
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
        assert!(saved["turn"].as_array().is_some_and(|turns| !turns.is_empty()));

        // Already saved there: nothing to do
        assert!(sync(db, track_data, manifest, Some(synced)).await.unwrap().is_none());
    }
}
