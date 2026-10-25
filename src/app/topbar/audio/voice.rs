//! Lists, downloads and selects [Piper](https://github.com/rhasspy/piper) voices for text to
//! speech, one per language.

use crate::{
    Download,
    app::topbar::{
        audio::{SetupItem, SetupState},
        setup_label,
    },
    config::{ArrayStr, Config},
    error,
    synthesizer::Synthesizer,
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
    time::Duration,
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

    pub fn new(pref_path: &Arc<Utf8Path>, client: reqwest::Client) -> Self {
        let pref_path = pref_path.clone();

        return Self {
            client: client.clone(),
            setup: None,
            setup_started: false,
            catalog: Self::fetch_catalog(pref_path.clone(), client.clone(), 3),
            pref_path,
        };
    }

    /// Called every frame: sets up the recommended voice if the setup guide asked for it.
    pub fn poll(&mut self, rt: &Handle, config: &Config, synth: &mut Synthesizer) {
        let Some((lang, name)) = self.setup else {
            return;
        };
        if config.get().voices().contains_key(&lang) {
            self.setup = None;
            return;
        }

        let Some(Some(catalog)) = self.catalog.update(rt) else {
            return;
        };
        let Some(voice) = find(&mut catalog.languages, lang, name) else {
            log::warn!("The recommended voice '{name}' isn't in the catalog");
            self.setup = None;
            return;
        };

        let on_disk = (catalog.on_disk.borrow().as_ref())
            .and_then(|on_disk| on_disk.get(voice.index).copied());
        match on_disk {
            Some(true) => {
                let path = voice.path.to_path_buf();
                config.update(|raw| raw.set_voice(rt, lang, Some(path), synth));
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
    /// and uses it once it's on disk, if that language has none by then.
    pub fn setup(&mut self, locale: &str) {
        self.retry_catalog();
        self.setup = recommended(locale);
        self.setup_started = false;
    }

    /// The recommended voice for the app's language (`locale`), as the setup guide shows it:
    /// `None` if there's none, or another is picked.
    pub fn setup_item(&mut self, rt: &Handle, config: &Config, locale: &str) -> Option<SetupItem> {
        let (lang, name) = self.setup.or_else(|| recommended(locale))?;
        let picked = config.get().voices().get(&lang).cloned();
        let catalog = match self.catalog.update(rt) {
            None => return picked.is_none().then_some(SetupItem::Loading),
            Some(None) => return picked.is_none().then_some(SetupItem::Unavailable),
            Some(Some(catalog)) => catalog,
        };
        let voice = find(&mut catalog.languages, lang, name)?;
        if self.setup.is_none() && picked.is_some_and(|picked| picked != *voice.path) {
            return None;
        }

        let on_disk = (catalog.on_disk.borrow().as_ref())
            .and_then(|on_disk| on_disk.get(voice.index).copied());
        let state = match (on_disk, voice.progress()) {
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
        let pending = self.catalog.update(rt).is_none();
        let failed = matches!(self.catalog, MaybeDone::Done(None));
        if pending || failed {
            ui.menu_button(
                setup_label(config.get().voices().is_empty(), t!("audio.text_to_speech")),
                |ui| {
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
        let missing = config.get().voices().is_empty();
        let locale = config.get().locale().clone();
        let suggested = ArrayStr::<2>::from_str(locale.split('-').next().unwrap_or_default());

        ui.add_enabled_ui(catalog.is_some(), |ui| {
            ui.menu_button(setup_label(missing, t!("audio.text_to_speech")), |ui| {
                let Some(catalog) = catalog else {
                    return;
                };

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

                            let label =
                                setup_label(missing && Some(language.code) == suggested, label);
                            ui.menu_button(label, |ui| {
                                language.update(rt, ui, client, config, on_disk.as_deref(), synth);
                            });
                        }
                    });
            });
        });
    }
}

/// The voice of language `lang` named `name` in the catalog (e.g. `en_US-mike-medium`).
fn find<'a>(languages: &'a mut [Language], lang: ArrayStr<2>, name: &str) -> Option<&'a mut Voice> {
    let file = format!("/{name}.onnx");
    languages
        .iter_mut()
        .find(|language| language.code == lang)?
        .voices
        .iter_mut()
        .find(|voice| voice.model_file.ends_with(&file))
}

struct Catalog {
    languages: Vec<Language>,
    /// Whether each voice's files are on disk, indexed by [`Voice::index`]
    on_disk: tokio::sync::watch::Receiver<Option<Vec<bool>>>,
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
            async move {
                let mut on_disk = Vec::with_capacity(paths.len());
                for path in paths.iter() {
                    let exists = async |path| tokio::fs::try_exists(path).await.unwrap_or(false);
                    on_disk.push(
                        exists(path.as_std_path().to_path_buf()).await
                            && exists(path.as_std_path().with_added_extension("json")).await,
                    );
                }
                Some(on_disk)
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
        on_disk: Option<&[bool]>,
        synth: &mut Synthesizer,
    ) {
        let code = self.code;
        let voices = &mut self.voices;

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

                            let Some(&on_disk) =
                                on_disk.and_then(|on_disk| on_disk.get(voice.index))
                            else {
                                return;
                            };

                            row.col(|ui| {
                                if on_disk {
                                    let used =
                                        config.voices().get(&code).map(|path| path.as_path())
                                            == Some(&*voice.path);
                                    ui.add_enabled_ui(!used, |ui| {
                                        if ui.button(t!("audio.use")).clicked() {
                                            config.set_voice(
                                                rt,
                                                code,
                                                Some(voice.path.to_path_buf()),
                                                synth,
                                            );
                                            updated = true;
                                        }
                                    });
                                } else if let Some(progress) = voice.progress() {
                                    egui::ProgressBar::new(progress as f32)
                                        .show_percentage()
                                        .ui(ui);
                                } else if ui.button(egui_phosphor::bold::CLOUD_ARROW_DOWN).clicked()
                                {
                                    voice.start_download(client);
                                }
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
}

impl Voice {
    /// Download progress of both files, weighted by size: `None` if they aren't downloading, or
    /// either failed.
    fn progress(&self) -> Option<f64> {
        if self.failed() {
            return None;
        }
        let [model, config] = self.downloads.as_ref()?;
        let (model_size, config_size) = (self.model_size as f64, self.config_size as f64);
        let total = (model_size + config_size).max(1.0);
        return Some((model.progress() * model_size + config.progress() * config_size) / total);
    }

    /// Whether the last download of either file failed.
    fn failed(&self) -> bool {
        (self.downloads.iter().flatten()).any(Download::failed)
    }

    fn start_download(&mut self, client: &reqwest::Client) {
        // Both start over if either failed
        if self.failed() {
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
