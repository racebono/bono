//! Lists, downloads and selects [Piper](https://github.com/rhasspy/piper) voices for text to
//! speech, one per language.

use crate::{
    Download,
    app::topbar::{
        audio::{Deletion, SetupItem, SetupState, scroll_bar_beside},
        setup_label, size_text,
    },
    config::{ArrayStr, Config, RawConfig},
    error, language,
    synthesizer::phonemizer,
    synthesizer::{Synthesizer, VoiceLoad},
    task::MaybeDone,
    updating,
};
use bono_i18n::t;
use camino::{Utf8Component, Utf8Path};
use egui::{RichText, Widget};
use egui_extras::Column;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::runtime::Handle;

const CATALOG_URL: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main/voices.json";
const FILE_URL: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main/";

/// The voice the setup guide downloads for the app's language (`locale`, e.g. `en-US`): its
/// language, and its name in the catalog.
fn recommended(locale: &str) -> Option<(ArrayStr<2>, &'static str)> {
    let lang = locale.split('-').next()?;
    let voice = match lang {
        "en" => "en_US-mike-medium",
        "es" => "es_ES-davefx-medium",
        "ca" => "ca_ES-upc_ona-medium",
        _ => return None,
    };
    Some((ArrayStr::from_str(lang)?, voice))
}

pub struct Voices {
    catalog: MaybeDone<Option<Catalog>>,
    pref_path: Arc<Utf8Path>,
    client: reqwest::Client,
    /// The setup guide asked for the recommended voice of a language (and which it is): it's
    /// downloaded, then used once it's on disk (unless another was picked meanwhile)
    setup: Option<(ArrayStr<2>, &'static str)>,
    /// The setup's download was started: one that fails isn't started again until it's asked
    /// for again
    setup_started: bool,
    /// When setup first found the recommended voice ready to replace the picked one: it waits for
    /// a check of the picked one's files started since, so a stale result doesn't replace it
    replace_since: Option<Instant>,
    files: super::ModelFiles,
}

impl Voices {
    /// The startup request gets three retries; manual requests get none.
    fn fetch_catalog(
        pref_path: Arc<Utf8Path>,
        client: reqwest::Client,
        retries: usize,
    ) -> MaybeDone<Option<Catalog>> {
        MaybeDone::spawn(async move {
            for attempt in 0..=retries {
                match Catalog::fetch(&pref_path, &client).await {
                    Ok(catalog) => return Some(catalog),
                    Err(e) => {
                        if attempt == 0 {
                            error(t!("error.tts_catalog"), e);
                        } else {
                            log::warn!("Failed to fetch the tts catalog: {e}");
                        }
                    }
                }
                if attempt < retries {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
            None
        })
    }

    fn retry_catalog(&mut self) {
        if matches!(self.catalog, MaybeDone::Done(None)) {
            self.catalog = Self::fetch_catalog(self.pref_path.clone(), self.client.clone(), 0);
        }
    }

    pub fn new(
        pref_path: &Arc<Utf8Path>,
        client: reqwest::Client,
        files: super::ModelFiles,
    ) -> Self {
        let pref_path = pref_path.clone();

        return Self {
            client: client.clone(),
            setup: None,
            setup_started: false,
            replace_since: None,
            files,
            catalog: Self::fetch_catalog(pref_path.clone(), client.clone(), 3),
            pref_path,
        };
    }

    /// Called every frame: sets up the recommended voice if the setup guide asked for it.
    pub fn poll(&mut self, rt: &Handle, config: &Config, synth: &mut Synthesizer) {
        let Some((lang, name)) = self.setup else {
            return;
        };
        let picked = config.get().voices().get(&lang).cloned();
        // A picked voice that didn't load is replaced (or loaded again, if it's the recommended)
        let failed = synth.load(lang) == Some(VoiceLoad::Failed);
        let Some(Some(catalog)) = self.catalog.update(rt) else {
            return;
        };
        let Some(voice) = find(&mut catalog.languages, lang, name) else {
            log::warn!("The recommended voice '{name}' isn't in the catalog");
            self.setup = None;
            return;
        };

        // Wait until the picked voice's files are checked before deciding to keep it
        let on_disk = match picked.as_ref().map(|path| self.files.voice_checked(path)) {
            Some(None) => return,
            Some(Some(on_disk)) => on_disk,
            None => false,
        };
        if on_disk
            && !failed
            && picked
                .as_ref()
                .is_some_and(|path| !self.setup_started || path.as_path() != &*voice.path)
        {
            self.setup = None;
            return;
        }

        let on_disk = voice.on_disk(rt, catalog.on_disk.borrow().as_ref());
        match on_disk {
            Some(true) => {
                let path = voice.path.to_path_buf();
                if let Some(picked) = picked.as_ref().filter(|picked| **picked != path) {
                    let since = *self.replace_since.get_or_insert_with(Instant::now);
                    match self.files.voice_checked_since(picked, Some(since)) {
                        None => return,
                        Some(true) if !failed => {
                            self.setup = None;
                            return;
                        }
                        Some(_) => {}
                    }
                }
                config.update(|raw| raw.set_voice(rt, lang, path, synth));
                self.setup = None;
            }
            Some(false) if !self.setup_started => {
                voice.start_download(&self.client);
                self.setup_started = true;
            }
            Some(false) => {}
            // Not looked for yet
            None => {}
        }
    }

    /// Downloads the recommended voice for the app's language (`locale`) (again, if it failed),
    /// and uses it once it's on disk, if that language has no voice files by then.
    pub fn setup(&mut self, locale: &str) {
        self.retry_catalog();
        self.setup = recommended(locale);
        self.setup_started = false;
        self.replace_since = None;
    }

    /// The recommended voice for the app's language (`locale`), as the setup guide shows it:
    /// `None` if there's none, or another is picked (and loaded, or loading).
    pub fn setup_item(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &Synthesizer,
        locale: &str,
    ) -> Option<SetupItem> {
        let (lang, name) = self.setup.or_else(|| recommended(locale))?;
        let picked = config.get().voices().get(&lang).cloned();
        let load = synth.load(lang);
        let failed = load == Some(VoiceLoad::Failed);
        let unusable = failed
            || picked
                .as_ref()
                .is_none_or(|picked| !self.files.voice(picked));
        let catalog = match self.catalog.update(rt) {
            None => return unusable.then_some(SetupItem::Loading),
            Some(None) => return unusable.then_some(SetupItem::Unavailable),
            Some(Some(catalog)) => catalog,
        };
        let voice = find(&mut catalog.languages, lang, name)?;
        if self.setup.is_none()
            && !failed
            && (picked.as_ref())
                .is_some_and(|picked| *picked != *voice.path && self.files.voice(picked))
        {
            return None;
        }

        let on_disk = voice.on_disk(rt, catalog.on_disk.borrow().as_ref());
        let used = (picked.as_ref()).is_some_and(|picked| *picked == *voice.path);
        let state = match (on_disk, voice.progress()) {
            (Some(true), _) if used && failed => SetupState::LoadFailed,
            // On disk but not used, and setup isn't going to: retrying uses it
            (Some(true), _) if self.setup.is_none() && !used => SetupState::Missing,
            // Being loaded, or setup is about to use it
            (Some(true), _) if !used || load == Some(VoiceLoad::Loading) => SetupState::Loading,
            (Some(true), _) => SetupState::Ready,
            (_, Some(progress)) => SetupState::Downloading(progress),
            _ if voice.failed() => SetupState::Failed,
            _ => SetupState::Missing,
        };
        Some(SetupItem::Found {
            name: format!("{} ({})", voice.name, voice.country),
            size: Some(voice.model_size + voice.config_size),
            state,
        })
    }

    pub fn update(
        &mut self,
        rt: &Handle,
        config: &mut Config,
        ui: &mut egui::Ui,
        synth: &mut Synthesizer,
    ) {
        // The languages whose voice won't load
        let load_failed: Vec<ArrayStr<2>> = (config.get().voices().keys().copied())
            .filter(|&lang| synth.load(lang) == Some(VoiceLoad::Failed))
            .collect();
        let pending = self.catalog.update(rt).is_none();
        let failed = matches!(self.catalog, MaybeDone::Done(None));
        if pending || failed {
            ui.menu_button(
                setup_label(
                    config.get().voices().is_empty()
                        || (config.get().voices().values()).any(|path| !self.files.voice(path))
                        || !load_failed.is_empty(),
                    t!("audio.text_to_speech"),
                ),
                |ui| {
                    gpu_checkbox(ui, config, synth);
                    if ui
                        .add_enabled(failed, egui::Button::new(t!("onboarding.retry")))
                        .clicked()
                    {
                        self.retry_catalog();
                    }
                    if pending {
                        ui.spinner();
                    }
                },
            );
            return;
        }
        let client = &self.client;
        let catalog = match self.catalog.update(rt) {
            Some(Some(catalog)) => Some(catalog),
            Some(None) | None => None,
        };

        // Without any voice, the app's language is the one to get one for
        let missing = config.get().voices().is_empty()
            || config
                .get()
                .voices()
                .values()
                .any(|path| !self.files.voice(path));
        let locale = config.get().locale().clone();
        let suggested = ArrayStr::<2>::from_str(locale.split('-').next().unwrap_or_default());

        let setup = &mut self.setup;
        ui.add_enabled_ui(catalog.is_some(), |ui| {
            let label = setup_label(
                missing || !load_failed.is_empty(),
                t!("audio.text_to_speech"),
            );
            ui.menu_button(label, |ui| {
                let Some(catalog) = catalog else {
                    return;
                };

                gpu_checkbox(ui, config, synth);

                // Copied out so the poller isn't blocked while the menu is drawn
                let on_disk = catalog.on_disk.borrow().clone();

                egui::ScrollArea::vertical()
                    .max_height(400.0)
                    .show(ui, |ui| {
                        for language in &mut catalog.languages {
                            let has_voice = config.get().voices().contains_key(&language.code);
                            let mut label = RichText::new(format!(
                                "{} ({})",
                                language.name_english, language.name_native
                            ));
                            if has_voice {
                                label = label.strong();
                            }

                            let label = setup_label(
                                (missing && Some(language.code) == suggested)
                                    || load_failed.contains(&language.code),
                                label,
                            );
                            ui.menu_button(label, |ui| {
                                language.update(
                                    rt,
                                    ui,
                                    client,
                                    config,
                                    on_disk.as_ref(),
                                    synth,
                                    setup,
                                );
                            });
                        }
                    });
            });
        });
    }

    /// Whether a voice is still being deleted.
    pub fn deleting(&self) -> bool {
        matches!(&self.catalog, MaybeDone::Done(Some(catalog))
            if (catalog.languages.iter().flat_map(|language| &language.voices))
                .any(|voice| voice.deletion.running()))
    }
}

/// The voice of language `lang` named `name` in the catalog (e.g. `en_US-mike-medium`).
fn find<'a>(languages: &'a mut [Language], lang: ArrayStr<2>, name: &str) -> Option<&'a mut Voice> {
    languages
        .iter_mut()
        .find(|language| language.code == lang)?
        .voices
        .iter_mut()
        .find(|voice| voice.is_named(name))
}

/// Whether voices run on the GPU, whether or not the catalog could be fetched. DirectML is Windows
/// only: elsewhere, voices run on the processor and there's nothing to choose.
fn gpu_checkbox(ui: &mut egui::Ui, config: &mut Config, synth: &Synthesizer) {
    if !cfg!(windows) {
        return;
    }

    config.maybe_update(|raw| {
        let changed = raw.voice_gpu.edit(*raw.voice_gpu(), |gpu| {
            ui.checkbox(gpu, t!("audio.voice_gpu"))
                .on_hover_text(t!("audio.voice_gpu_hover"))
                .changed()
        });
        if changed {
            synth.set_gpu(*raw.voice_gpu());
        }
        changed
    });
    ui.separator();
}

struct Catalog {
    languages: Vec<Language>,
    /// Whether each voice's files are on disk, indexed by [`Voice::index`], and when that check
    /// started
    on_disk: tokio::sync::watch::Receiver<Option<(Vec<bool>, Instant)>>,
}

impl Catalog {
    async fn fetch(pref_dir: &Utf8Path, client: &reqwest::Client) -> reqwest::Result<Self> {
        #[derive(Deserialize)]
        struct RawVoice {
            name: String,
            language: RawLanguage,
            quality: String,
            num_speakers: u32,
            files: HashMap<String, RawFile>,
        }

        #[derive(Deserialize)]
        struct RawLanguage {
            family: String,
            name_native: String,
            name_english: String,
            country_english: String,
        }

        #[derive(Deserialize)]
        struct RawFile {
            size_bytes: u64,
            md5_digest: Option<String>,
        }

        let catalog = client
            .get(CATALOG_URL)
            .send()
            .await?
            .error_for_status()?
            .json::<HashMap<String, RawVoice>>()
            .await?;

        let mut languages = BTreeMap::<ArrayStr<2>, Language>::new();
        for (key, voice) in catalog {
            // Voices are keyed by Whisper's two-letter language codes
            let Some(code) = ArrayStr::<2>::from_str(&voice.language.family) else {
                log::warn!(
                    "Skipping voice '{key}': unsupported language '{}'",
                    voice.language.family
                );
                continue;
            };

            let file = |extension: &str| {
                voice
                    .files
                    .iter()
                    .find(|(path, _)| path.ends_with(extension) && is_relative_path(path))
                    .map(|(path, file)| (path.clone(), file.size_bytes, file.md5_digest.clone()))
            };

            let (Some((model_file, model_size, model_md5)), Some((_, config_size, config_md5))) =
                (file(".onnx"), file(".onnx.json"))
            else {
                log::warn!("Skipping voice '{key}': missing model or config file");
                continue;
            };

            let language = languages.entry(code).or_insert_with(|| Language {
                code,
                name_english: voice.language.name_english.clone(),
                name_native: voice.language.name_native.clone(),
                voices: Vec::new(),
            });

            language.voices.push(Voice {
                index: 0,
                name: voice.name,
                country: voice.language.country_english,
                quality: Quality::from(voice.quality.as_str()),
                speakers: voice.num_speakers,
                path: Arc::from(pref_dir.join("piper").join(&model_file)),
                model_file,
                model_size,
                config_size,
                model_md5,
                config_md5,
                downloads: None,
                deletion: Deletion::default(),
            });
        }

        let mut languages = languages.into_values().collect::<Vec<_>>();
        languages.sort_by(|lhs, rhs| lhs.name_english.cmp(&rhs.name_english));

        let mut paths = Vec::new();
        for language in &mut languages {
            language.voices.sort_by(|lhs, rhs| {
                (&lhs.country, &lhs.name, lhs.quality).cmp(&(&rhs.country, &rhs.name, rhs.quality))
            });

            for voice in &mut language.voices {
                voice.index = paths.len();
                paths.push(voice.path.clone());
            }
        }

        // One poller for the whole catalog, rather than a task per voice
        let paths = Arc::<[Arc<Utf8Path>]>::from(paths);
        let on_disk = updating(Duration::from_millis(1234), None, move |_| {
            let paths = paths.clone();
            let started = Instant::now();
            async move {
                let mut on_disk = Vec::with_capacity(paths.len());
                for path in paths.iter() {
                    let exists = async |path| tokio::fs::try_exists(path).await.unwrap_or(false);
                    on_disk.push(
                        exists(path.as_std_path().to_path_buf()).await
                            && exists(path.as_std_path().with_added_extension("json")).await,
                    );
                }
                Some((on_disk, started))
            }
        });

        return Ok(Self { languages, on_disk });
    }
}

struct Language {
    code: ArrayStr<2>,
    name_english: String,
    name_native: String,
    voices: Vec<Voice>,
}

impl Language {
    fn update(
        &mut self,
        rt: &Handle,
        ui: &mut egui::Ui,
        client: &reqwest::Client,
        config: &mut Config,
        on_disk: Option<&(Vec<bool>, Instant)>,
        synth: &mut Synthesizer,
        setup: &mut Option<(ArrayStr<2>, &'static str)>,
    ) {
        let code = self.code;
        let voices = &mut self.voices;

        scroll_bar_beside(ui);
        egui_extras::TableBuilder::new(ui)
            .id_salt(("piper-voices", code))
            .striped(true)
            .columns(Column::auto(), 5)
            .column(Column::remainder())
            .header(20.0, |mut header| {
                for title in [
                    t!("audio.voice"),
                    t!("audio.region"),
                    t!("audio.quality"),
                    t!("audio.speakers"),
                    t!("audio.size"),
                ] {
                    header.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|mut body| {
                config.maybe_update(|config| {
                    let mut updated = false;
                    for voice in voices {
                        body.row(18.0, |mut row| {
                            row.col(|ui| {
                                ui.label(&voice.name);
                            });

                            row.col(|ui| {
                                ui.label(&voice.country);
                            });

                            row.col(|ui| {
                                ui.label(voice.quality.label());
                            });

                            row.col(|ui| {
                                ui.label(voice.speakers.to_string());
                            });

                            row.col(|ui| {
                                ui.label(format!(
                                    "{:.0} MB",
                                    (voice.model_size + voice.config_size) as f64 / 1e6
                                ));
                            });

                            let on_disk = voice.on_disk(rt, on_disk);
                            if voice.deletion.pending() {
                                row.col(|ui| {
                                    ui.spinner();
                                });
                                return;
                            }
                            let Some(on_disk) = on_disk else {
                                return;
                            };

                            row.col(|ui| {
                                if let Some(progress) = voice.progress() {
                                    egui::ProgressBar::new(progress as f32)
                                        .show_percentage()
                                        .ui(ui);
                                    return;
                                }
                                let used = config.voices().get(&code).map(|path| path.as_path())
                                    == Some(&*voice.path);
                                ui.horizontal(|ui| {
                                    if on_disk {
                                        // The voice in use can be picked again only if it failed
                                        // to load, to retry
                                        let usable = !used || synth.failed(code, &voice.path);
                                        ui.add_enabled_ui(usable, |ui| {
                                            if ui.button(t!("audio.use")).clicked() {
                                                let path = voice.path.to_path_buf();
                                                config.set_voice(rt, code, path, synth);
                                                // A voice picked by hand is what's used, whatever
                                                // setup was getting for its language
                                                setup.take_if(|(lang, _)| *lang == code);
                                                updated = true;
                                            }
                                        });
                                    } else if ui
                                        .button(egui_phosphor::bold::CLOUD_ARROW_DOWN)
                                        .clicked()
                                    {
                                        voice.start_download(client);
                                    }
                                    // Deleting it removes every language it's the voice of
                                    let speaks = (config.voices().iter())
                                        .filter(|(_, path)| path.as_path() == &*voice.path)
                                        .map(|(&lang, _)| lang)
                                        .collect::<Vec<_>>();
                                    // Without its files too, a voice in use can be removed
                                    if (on_disk || !speaks.is_empty())
                                        && voice.delete_button(ui, &speaks, on_disk)
                                    {
                                        voice.delete(config, synth);
                                        // Setup doesn't get it again
                                        setup.take_if(|(_, name)| voice.is_named(name));
                                        updated = true;
                                    }
                                });
                            });
                        });
                    }
                    return updated;
                });
            });
    }
}

struct Voice {
    /// Position in [`Catalog::on_disk`]
    index: usize,
    name: String,
    country: String,
    quality: Quality,
    speakers: u32,
    /// Where the model is stored. Its config goes next to it, at `{path}.json`, where
    /// the synthesizer looks for it.
    path: Arc<Utf8Path>,
    /// The model's path in the repository (the config's is the same plus `.json`)
    model_file: String,
    model_size: u64,
    config_size: u64,
    model_md5: Option<String>,
    config_md5: Option<String>,
    /// The model's and the config's
    downloads: Option<[Download; 2]>,
    deletion: Deletion,
}

impl Voice {
    /// Whether it's the voice named `name` in the catalog (e.g. `en_US-mike-medium`).
    fn is_named(&self, name: &str) -> bool {
        self.model_file.ends_with(&format!("/{name}.onnx"))
    }

    /// Whether its files are on disk, as [`Catalog::on_disk`] found: `None` until it's been
    /// checked, and while they're deleted.
    fn on_disk(&mut self, rt: &Handle, checked: Option<&(Vec<bool>, Instant)>) -> Option<bool> {
        let found = checked.and_then(|(found, started)| Some((*found.get(self.index)?, *started)));
        self.deletion.on_disk(rt, found)
    }

    /// The button to [`Self::delete`] it, the voice of the languages it `speaks`. Returns whether
    /// it was clicked.
    fn delete_button(&self, ui: &mut egui::Ui, speaks: &[ArrayStr<2>], on_disk: bool) -> bool {
        let size = on_disk.then(|| {
            t!(
                "audio.delete",
                size = size_text(self.model_size + self.config_size)
            )
        });
        let stops = (!speaks.is_empty()).then(|| {
            let languages = speaks.iter().map(|&lang| language::ui_name(lang));
            t!(
                "audio.delete_voice_in_use",
                language = languages.collect::<Vec<_>>().join(", ")
            )
        });
        ui.button(egui_phosphor::bold::TRASH)
            .on_hover_text(size.into_iter().chain(stops).collect::<Vec<_>>().join("\n"))
            .clicked()
    }

    /// Deletes its files once it's unloaded, removing the languages it's the voice of.
    fn delete(&mut self, config: &mut RawConfig, synth: &mut Synthesizer) {
        self.downloads = None;
        let workers = config.remove_voice(&self.path, synth);
        let model = self.path.as_std_path().to_path_buf();
        let json = model.with_added_extension("json");
        let release = move || {
            drop(workers);
            // It may have inherited the files, if it was started while they were open
            phonemizer::stop();
        };
        self.deletion.start(release, vec![model, json]);
    }

    /// Download progress of both files, weighted by size: `None` if they aren't downloading, or
    /// either failed.
    fn progress(&self) -> Option<f64> {
        let [model, config] = self.downloads.as_ref()?;
        if self.failed() || (model.finished() && config.finished()) {
            return None;
        }
        let (model_size, config_size) = (self.model_size as f64, self.config_size as f64);
        let total = (model_size + config_size).max(1.0);
        return Some((model.progress() * model_size + config.progress() * config_size) / total);
    }

    /// Whether the last download of either file failed.
    fn failed(&self) -> bool {
        (self.downloads.iter().flatten()).any(Download::failed)
    }

    fn start_download(&mut self, client: &reqwest::Client) {
        // Both start over if either failed, or once done if their files went missing since
        if self.failed() || (self.downloads.iter().flatten()).all(Download::finished) {
            self.downloads = None;
        }
        self.downloads.get_or_insert_with(|| {
            let path = self.path.as_std_path();
            [
                Download::new(
                    client.get(format!("{FILE_URL}{}", self.model_file)),
                    path.to_path_buf(),
                    Some(self.model_size),
                    self.model_md5.clone().map(crate::DownloadDigest::Md5),
                ),
                Download::new(
                    client.get(format!("{FILE_URL}{}.json", self.model_file)),
                    path.with_added_extension("json"),
                    Some(self.config_size),
                    self.config_md5.clone().map(crate::DownloadDigest::Md5),
                ),
            ]
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Quality {
    ExtraLow,
    Low,
    Medium,
    High,
    Unknown,
}

impl Quality {
    fn label(self) -> &'static str {
        match self {
            Quality::ExtraLow => t!("quality.extra_low"),
            Quality::Low => t!("quality.low"),
            Quality::Medium => t!("quality.medium"),
            Quality::High => t!("quality.high"),
            Quality::Unknown => t!("quality.unknown"),
        }
    }
}

impl From<&str> for Quality {
    fn from(quality: &str) -> Self {
        match quality {
            "x_low" => Quality::ExtraLow,
            "low" => Quality::Low,
            "medium" => Quality::Medium,
            "high" => Quality::High,
            _ => Quality::Unknown,
        }
    }
}

/// Catalog paths become paths under the preferences directory, so only accept plain relative
/// ones (no `..`, roots or prefixes).
fn is_relative_path(path: &str) -> bool {
    Utf8Path::new(path)
        .components()
        .all(|component| matches!(component, Utf8Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrstr;
    use camino::Utf8PathBuf;

    fn voice(path: &Utf8Path) -> Voice {
        Voice {
            index: 0,
            name: "mike".into(),
            country: "United States".into(),
            quality: Quality::Medium,
            speakers: 1,
            path: path.into(),
            model_file: "en/en_US/mike/medium/en_US-mike-medium.onnx".into(),
            model_size: 0,
            config_size: 0,
            model_md5: None,
            config_md5: None,
            downloads: None,
            deletion: Deletion::default(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_voice_in_use_removes_its_language_and_both_files() {
        let rt = Handle::current();
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| Utf8PathBuf::try_from(dir.path().join(name)).unwrap();
        let (deleted, kept) = (path("en.onnx"), path("es.onnx"));
        let files = |model: &Utf8Path| [model.to_owned(), model.with_added_extension("json")];
        for file in [&deleted, &kept].into_iter().flat_map(|model| files(model)) {
            std::fs::write(file, []).unwrap();
        }

        let (mut config, mut synth) = (
            RawConfig::default(),
            Synthesizer::new(&rt, &RawConfig::default()),
        );
        config.set_voice(&rt, arrstr!("en"), deleted.clone(), &mut synth);
        config.set_voice(&rt, arrstr!("es"), kept.clone(), &mut synth);

        let mut voice = voice(&deleted);
        assert_eq!(
            voice.on_disk(&rt, Some(&(vec![true], Instant::now()))),
            Some(true)
        );
        let before = Instant::now();
        voice.delete(&mut config, &mut synth);
        assert_eq!(
            *config.voices(),
            BTreeMap::from([(arrstr!("es"), kept.clone())])
        );
        assert_eq!(synth.load(arrstr!("en")), None);

        let deadline = Instant::now() + Duration::from_secs(10);
        while voice.deletion.running() {
            assert!(Instant::now() < deadline, "timed out");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let on_disk = |voice: &mut Voice, found, started| {
            tokio::task::block_in_place(|| voice.on_disk(&rt, Some(&(vec![found], started))))
        };
        // Not used (by the row, nor setup) until it's checked again
        assert_eq!(on_disk(&mut voice, true, before), None);
        assert_eq!(on_disk(&mut voice, false, Instant::now()), Some(false));
        assert!(files(&deleted).iter().all(|file| !file.exists()));
        assert!(files(&kept).iter().all(|file| file.exists()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_voice_keeps_the_one_in_use() {
        let rt = Handle::current();
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| Utf8PathBuf::try_from(dir.path().join(name)).unwrap();
        let (deleted, used) = (path("deleted.onnx"), path("used.onnx"));
        let (mut config, mut synth) = (
            RawConfig::default(),
            Synthesizer::new(&rt, &RawConfig::default()),
        );
        config.set_voice(&rt, arrstr!("en"), used.clone(), &mut synth);

        voice(&deleted).delete(&mut config, &mut synth);
        assert_eq!(*config.voices(), BTreeMap::from([(arrstr!("en"), used)]));
        assert!(synth.load(arrstr!("en")).is_some());
    }

    #[test]
    fn a_voice_is_named_as_in_the_catalog() {
        let voice = voice("en.onnx".into());
        assert!(voice.is_named("en_US-mike-medium"));
        assert!(!voice.is_named("mike-medium"));
    }
}
