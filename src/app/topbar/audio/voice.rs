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

/// The voice the setup guide downloads for a language.
pub struct Recommended {
    pub lang: &'static str,
    /// The language's own name
    pub native: &'static str,
    /// Its name in the catalog
    pub voice: &'static str,
}

const fn voice(lang: &'static str, native: &'static str, voice: &'static str) -> Recommended {
    Recommended {
        lang,
        native,
        voice,
    }
}

/// Every language with a voice Bono can speak: Piper also has Hebrew, Japanese, Lithuanian and Thai
/// voices, whose phonemes aren't espeak's.
pub const RECOMMENDED: &[Recommended] = &[
    voice("ar", "العربية", "ar_JO-kareem-medium"),
    voice("bg", "Български", "bg_BG-dimitar-medium"),
    voice("bn", "বাংলা", "bn_BD-google-medium"),
    voice("ca", "Català", "ca_ES-upc_ona-medium"),
    voice("cs", "Čeština", "cs_CZ-jirka-medium"),
    voice("cy", "Cymraeg", "cy_GB-gwryw_gogleddol-medium"),
    voice("da", "Dansk", "da_DK-talesyntese-medium"),
    voice("de", "Deutsch", "de_DE-thorsten-medium"),
    voice("el", "Ελληνικά", "el_GR-joy-medium"),
    voice("en", "English", "en_US-mike-medium"),
    voice("es", "Español", "es_ES-davefx-medium"),
    voice("et", "Eesti", "et_EE-news-medium"),
    voice("eu", "Euskara", "eu_ES-antton-medium"),
    voice("fa", "فارسی", "fa_IR-amir-medium"),
    voice("fi", "Suomi", "fi_FI-harri-medium"),
    voice("fr", "Français", "fr_FR-siwis-medium"),
    voice("hi", "हिन्दी", "hi_IN-pratham-medium"),
    voice("hu", "Magyar", "hu_HU-anna-medium"),
    voice("hy", "Հայերեն", "hy_AM-gor-medium"),
    voice("id", "Bahasa Indonesia", "id_ID-news_tts-medium"),
    voice("is", "Íslenska", "is_IS-bui-medium"),
    voice("it", "Italiano", "it_IT-paola-medium"),
    voice("ka", "ქართული", "ka_GE-natia-medium"),
    voice("kk", "Қазақша", "kk_KZ-issai-high"),
    voice("ko", "한국어", "ko_KR-kss-medium"),
    voice("ku", "Kurdî", "ku_TR-berfin_renas-medium"),
    voice("lb", "Lëtzebuergesch", "lb_LU-marylux-medium"),
    voice("lv", "Latviešu", "lv_LV-aivars-medium"),
    voice("ml", "മലയാളം", "ml_IN-arjun-medium"),
    voice("mr", "मराठी", "mr_IN-google-medium"),
    voice("ne", "नेपाली", "ne_NP-chitwan-medium"),
    voice("nl", "Nederlands", "nl_NL-alex-medium"),
    voice("no", "Norsk", "no_NO-talesyntese-medium"),
    voice("pl", "Polski", "pl_PL-darkman-medium"),
    voice("pt", "Português", "pt_BR-faber-medium"),
    voice("ro", "Română", "ro_RO-mihai-medium"),
    voice("ru", "Русский", "ru_RU-denis-medium"),
    voice("sk", "Slovenčina", "sk_SK-lili-medium"),
    voice("sl", "Slovenščina", "sl_SI-artur-medium"),
    voice("sq", "Shqip", "sq_AL-edon-medium"),
    voice("sr", "Srpski", "sr_RS-serbski_institut-medium"),
    voice("sv", "Svenska", "sv_SE-lisa-medium"),
    voice("sw", "Kiswahili", "sw_CD-lanfrica-medium"),
    voice("te", "తెలుగు", "te_IN-maya-medium"),
    voice("tr", "Türkçe", "tr_TR-dfki-medium"),
    voice("uk", "Українська", "uk_UA-mykyta-high"),
    voice("ur", "اردو", "ur_PK-aegis_female-medium"),
    voice("vi", "Tiếng Việt", "vi_VN-vais1000-medium"),
    voice("zh", "中文", "zh_CN-huayan-medium"),
];

/// The recommended voice for the app's language (`locale`, e.g. `en-US`): its language, and its name
/// in the catalog.
pub(in crate::app) fn recommended(locale: &str) -> Option<(ArrayStr<2>, &'static str)> {
    let lang = locale.split('-').next()?;
    let recommended = RECOMMENDED.iter().find(|other| other.lang == lang)?;
    Some((ArrayStr::from_str(lang)?, recommended.voice))
}

pub struct Voices {
    catalog: MaybeDone<Option<Catalog>>,
    pref_path: Arc<Utf8Path>,
    client: reqwest::Client,
    /// The recommended voices the setup guide asked for
    setup: Vec<Setup>,
    files: super::ModelFiles,
}

/// A recommended voice the setup guide asked for: it's downloaded, then used once it's on disk
/// (unless another was picked meanwhile).
struct Setup {
    lang: ArrayStr<2>,
    /// Its name in the catalog
    name: &'static str,
    /// Its download was started: one that fails isn't started again until it's asked for again
    started: bool,
    /// When it was first found ready to replace the picked one: it waits for a check of the
    /// picked one's files started since, so a stale result doesn't replace it
    replace_since: Option<Instant>,
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
            setup: Vec::new(),
            files,
            catalog: Self::fetch_catalog(pref_path.clone(), client.clone(), 3),
            pref_path,
        };
    }

    /// Called every frame: sets up the recommended voices the setup guide asked for.
    pub fn poll(&mut self, rt: &Handle, config: &Config, synth: &mut Synthesizer) {
        if self.setup.is_empty() {
            return;
        }
        let Some(Some(catalog)) = self.catalog.update(rt) else {
            return;
        };
        let (files, client) = (&self.files, &self.client);
        (self.setup).retain_mut(|setup| !set_up(rt, config, synth, catalog, files, client, setup));
    }

    /// Downloads the recommended voice for each of `langs` (again, if it failed), and uses each
    /// once it's on disk, if its language has no voice files by then.
    pub fn setup(&mut self, langs: &[ArrayStr<2>]) {
        self.retry_catalog();
        self.setup = (langs.iter())
            .filter_map(|lang| {
                let (lang, name) = recommended(lang)?;
                Some(Setup {
                    lang,
                    name,
                    started: false,
                    replace_since: None,
                })
            })
            .collect();
    }

    /// The languages whose voice is being set up, and is coming: not those whose download failed.
    pub fn getting(&self) -> Vec<ArrayStr<2>> {
        let failed = |setup: &Setup| match &self.catalog {
            MaybeDone::Done(Some(catalog)) => (catalog.languages.iter())
                .filter(|language| language.code == setup.lang)
                .flat_map(|language| &language.voices)
                .any(|voice| voice.is_named(setup.name) && voice.failed()),
            _ => false,
        };
        (self.setup.iter())
            .filter(|setup| !failed(setup))
            .map(|setup| setup.lang)
            .collect()
    }

    /// The languages whose voice the setup guide asked for and isn't used yet, failed ones too.
    pub fn asked(&self) -> Vec<ArrayStr<2>> {
        self.setup.iter().map(|setup| setup.lang).collect()
    }

    /// The recommended voices for `langs`, as the setup guide shows them.
    pub fn setup_items(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &Synthesizer,
        langs: &[ArrayStr<2>],
    ) -> Vec<SetupItem> {
        (langs.iter())
            .filter_map(|&lang| self.setup_item(rt, config, synth, lang))
            .collect()
    }

    /// The recommended voice for `lang`, as the setup guide shows it: `None` if there's none, or
    /// another is picked (and loaded, or loading).
    fn setup_item(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &Synthesizer,
        lang: ArrayStr<2>,
    ) -> Option<SetupItem> {
        let setup = self.setup.iter().find(|setup| setup.lang == lang);
        let in_setup = setup.is_some();
        let (lang, name) = match setup {
            Some(setup) => (setup.lang, setup.name),
            None => recommended(&lang)?,
        };
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
        let Some(voice) = find(&mut catalog.languages, lang, name) else {
            return Some(SetupItem::Unavailable);
        };
        if !in_setup
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
            // On disk but not used, before the guide's Next: nothing to download, Next uses it
            (Some(true), _) if !in_setup && !used => SetupState::Ready,
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

/// Takes `setup` one step further. Returns whether it's done: its voice is used, or isn't needed.
fn set_up(
    rt: &Handle,
    config: &Config,
    synth: &mut Synthesizer,
    catalog: &mut Catalog,
    files: &super::ModelFiles,
    client: &reqwest::Client,
    setup: &mut Setup,
) -> bool {
    let (lang, name) = (setup.lang, setup.name);
    let picked = config.get().voices().get(&lang).cloned();
    // A picked voice that didn't load is replaced (or loaded again, if it's the recommended)
    let failed = synth.load(lang) == Some(VoiceLoad::Failed);
    // It stays asked for (the guide says it's unavailable), so what Automatic wants doesn't change
    let Some(voice) = find(&mut catalog.languages, lang, name) else {
        return false;
    };

    // Wait until the picked voice's files are checked before deciding to keep it
    let on_disk = match picked.as_ref().map(|path| files.voice_checked(path)) {
        Some(None) => return false,
        Some(Some(on_disk)) => on_disk,
        None => false,
    };
    if on_disk
        && !failed
        && picked
            .as_ref()
            .is_some_and(|path| !setup.started || path.as_path() != &*voice.path)
    {
        return true;
    }

    match voice.on_disk(rt, catalog.on_disk.borrow().as_ref()) {
        Some(true) => {
            let path = voice.path.to_path_buf();
            if let Some(picked) = picked.as_ref().filter(|picked| **picked != path) {
                let since = *setup.replace_since.get_or_insert_with(Instant::now);
                match files.voice_checked_since(picked, Some(since)) {
                    None => return false,
                    Some(true) if !failed => return true,
                    Some(_) => {}
                }
            }
            config.update(|raw| raw.set_voice(rt, lang, path, synth));
            true
        }
        Some(false) if !setup.started => {
            voice.start_download(client);
            setup.started = true;
            false
        }
        Some(false) => false,
        // Not looked for yet
        None => false,
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
        setup: &mut Vec<Setup>,
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
                                                setup.retain(|setup| setup.lang != code);
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
                                        setup.retain(|setup| !voice.is_named(setup.name));
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
    use crate::{
        app::topbar::audio::engine,
        arrstr,
        synthesizer::{Effects, Voice as Loaded},
    };
    use camino::Utf8PathBuf;
    use std::{collections::BTreeSet, num::NonZeroUsize};

    #[test]
    fn recommends_one_voice_per_language() {
        let langs = RECOMMENDED.iter().map(|voice| voice.lang).collect::<BTreeSet<_>>();
        assert_eq!(langs.len(), RECOMMENDED.len());
        for voice in RECOMMENDED {
            assert!(voice.voice.starts_with(&format!("{}_", voice.lang)), "{}", voice.voice);
            assert!(ArrayStr::<2>::from_str(voice.lang).is_some());
        }
        assert_eq!(recommended("en-US").unwrap().1, "en_US-mike-medium");
        assert_eq!(recommended("es").unwrap().1, "es_ES-davefx-medium");
        assert_eq!(recommended("ca-ES").unwrap().1, "ca_ES-upc_ona-medium");
        // All Parakeet knows, but those without a voice Bono can speak
        let known = (langs.iter())
            .filter(|lang| engine::parakeet_knows(ArrayStr::from_str(lang).unwrap()))
            .count();
        assert_eq!(known, 25 - 3);
        assert!(["hr", "lt", "mt"].iter().all(|lang| !langs.contains(lang)));
    }

    /// Downloads every one of them (3 GB, one at a time), and says something. `BONO_VOICES`, if set,
    /// has the languages to check, such as `fr,de`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn recommended_voices_are_in_the_catalog_and_speak() {
        let client = reqwest::Client::new();
        let only = std::env::var("BONO_VOICES").ok();
        let mut failed = Vec::new();
        for recommended in RECOMMENDED {
            if (only.as_ref()).is_some_and(|only| !only.split(',').any(|lang| lang == recommended.lang)) {
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            let mut catalog = Catalog::fetch(Utf8Path::from_path(dir.path()).unwrap(), &client).await.unwrap();
            let lang = ArrayStr::from_str(recommended.lang).unwrap();
            let Some(voice) = find(&mut catalog.languages, lang, recommended.voice) else {
                failed.push(format!("{}: not in the catalog", recommended.voice));
                continue;
            };
            voice.start_download(&client);
            while !voice.failed() && voice.progress().is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            let path = voice.path.clone();
            let spoken = tokio::task::spawn_blocking(move || -> anyhow::Result<f32> {
                let mut voice = Loaded::load(&path, NonZeroUsize::new(4).unwrap(), false)?;
                let effects = Effects {
                    radio: 0.0,
                    beep_volume: 0.0,
                    speed: 1.0,
                };
                let speech = voice.synthesize("1, 2, 3.", effects)?;
                Ok(speech.samples.len() as f32 / speech.sample_rate as f32)
            });
            match spoken.await.unwrap() {
                Ok(seconds) if seconds > 0.3 => println!("{}: {seconds:.1} s", recommended.voice),
                Ok(seconds) => failed.push(format!("{}: only {seconds} s", recommended.voice)),
                Err(e) => failed.push(format!("{}: {e:#}", recommended.voice)),
            }
        }
        assert!(failed.is_empty(), "{failed:#?}");
    }

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

    #[tokio::test(flavor = "multi_thread")]
    async fn sets_up_several_voices() {
        let rt = Handle::current();
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| Utf8PathBuf::try_from(dir.path().join(name)).unwrap();
        let config_path = dir.path().join("config.json");
        std::fs::write(&config_path, b"{}").unwrap();
        let config = Config::new(&config_path).await.unwrap();
        let mut synth = Synthesizer::new(&rt, &RawConfig::default());

        let mut voices = Voices {
            catalog: MaybeDone::Done(None),
            pref_path: Arc::from(Utf8Path::from_path(dir.path()).unwrap()),
            client: reqwest::Client::new(),
            setup: Vec::new(),
            files: crate::app::topbar::audio::ModelFiles::new(),
        };
        // Without a recommended voice, there's nothing to set up
        voices.setup(&[arrstr!("es"), arrstr!("fr"), arrstr!("xx")]);
        assert_eq!(voices.getting(), [arrstr!("es"), arrstr!("fr")]);

        // Spanish's is on disk; French's hasn't been looked for yet
        let mut spanish = voice(&path("es.onnx"));
        spanish.model_file = "es/es_ES/davefx/medium/es_ES-davefx-medium.onnx".into();
        let mut french = voice(&path("fr.onnx"));
        french.index = 1;
        french.model_file = "fr/fr_FR/siwis/medium/fr_FR-siwis-medium.onnx".into();
        let language = |code, voice| Language {
            code,
            name_english: String::new(),
            name_native: String::new(),
            voices: vec![voice],
        };
        let (_send, on_disk) = tokio::sync::watch::channel(Some((vec![true], Instant::now())));
        voices.catalog = MaybeDone::Done(Some(Catalog {
            languages: vec![language(arrstr!("es"), spanish), language(arrstr!("fr"), french)],
            on_disk,
        }));
        tokio::task::block_in_place(|| voices.poll(&rt, &config, &mut synth));
        assert_eq!(voices.getting(), [arrstr!("fr")]);
        assert_eq!(config.get().voices().get(&arrstr!("es")), Some(&path("es.onnx")));
        assert_eq!(config.get().voices().get(&arrstr!("fr")), None);
    }

    #[test]
    fn a_voice_is_named_as_in_the_catalog() {
        let voice = voice("en.onnx".into());
        assert!(voice.is_named("en_US-mike-medium"));
        assert!(!voice.is_named("mike-medium"));
    }
}
