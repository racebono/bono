//! Lists, downloads and selects [Piper](https://github.com/rhasspy/piper) voices for text to
//! speech, one per language.

use crate::{
    Download,
    config::{ArrayStr, Config},
    error,
    synthesizer::Synthesizer,
    task::MaybeDone,
    updating,
};
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

pub struct Voices {
    catalog: MaybeDone<Option<Catalog>>,
    client: reqwest::Client,
}

impl Voices {
    pub fn new(pref_path: &Arc<Utf8Path>, client: reqwest::Client) -> Self {
        let pref_path = pref_path.clone();
        let fetch_client = client.clone();

        return Self {
            client,
            catalog: MaybeDone::spawn(async move {
                match Catalog::fetch(&pref_path, &fetch_client).await {
                    Ok(catalog) => Some(catalog),
                    Err(e) => {
                        error("Error fetching text to speech catalog", e);
                        None
                    }
                }
            }),
        };
    }

    pub fn update(
        &mut self,
        rt: &Handle,
        config: &mut Config,
        ui: &mut egui::Ui,
        synth: &mut Synthesizer,
    ) {
        let client = &self.client;
        let catalog = match self.catalog.update(rt) {
            Some(Some(catalog)) => Some(catalog),
            Some(None) | None => None,
        };

        ui.add_enabled_ui(catalog.is_some(), |ui| {
            ui.menu_button("Text To Speech", |ui| {
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

                            ui.menu_button(label, |ui| {
                                language.update(rt, ui, client, config, on_disk.as_deref(), synth);
                            });
                        }
                    });
            });
        });
    }
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
                    .map(|(path, file)| (path.clone(), file.size_bytes))
            };

            let (Some((model_file, model_size)), Some((_, config_size))) =
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
                for title in ["Voice", "Region", "Quality", "Speakers", "Size"] {
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
                                        if ui.button("Use").clicked() {
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
                                } else if ui.button("Download").clicked() {
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
    /// The model's and the config's
    downloads: Option<[Download; 2]>,
}

impl Voice {
    /// Download progress of both files, weighted by size.
    fn progress(&self) -> Option<f64> {
        let [model, config] = self.downloads.as_ref()?;
        let (model_size, config_size) = (self.model_size as f64, self.config_size as f64);
        let total = (model_size + config_size).max(1.0);
        return Some((model.progress() * model_size + config.progress() * config_size) / total);
    }

    fn start_download(&mut self, client: &reqwest::Client) {
        self.downloads.get_or_insert_with(|| {
            let path = self.path.as_std_path();
            [
                Download::new(
                    client.get(format!("{FILE_URL}{}", self.model_file)),
                    path.to_path_buf(),
                    Some(self.model_size),
                ),
                Download::new(
                    client.get(format!("{FILE_URL}{}.json", self.model_file)),
                    path.with_added_extension("json"),
                    Some(self.config_size),
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
            Quality::ExtraLow => "Extra low",
            Quality::Low => "Low",
            Quality::Medium => "Medium",
            Quality::High => "High",
            Quality::Unknown => "Unknown",
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
