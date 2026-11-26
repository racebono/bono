use crate::{
    Download,
    app::topbar::{
        audio::{
            Deletion, RecordingStream, SetupItem, SetupState, app_language,
            engine::{self, Engine, Note, Used, Why},
            gpu,
            parakeet::{self, Parakeet},
            scroll_bar_beside,
        },
        SETUP_COLOR, setup_label, size_text,
    },
    arrstr,
    button::ConfigButton,
    config::{ArrayStr, Config, RawConfig},
    engineer::{Heard, NotListening},
    error, language,
    synthesizer::phonemizer,
    task::{JoinHandle, MaybeDone, spawn, spawn_blocking},
    updating,
};
use bono_i18n::t;
use camino::Utf8Path;
use egui::Widget;
use egui_extras::Column;
use sdl3::{
    audio::{AudioFormat, AudioRecordingCallback, AudioSpec, AudioStream},
    event::{Event, WindowEvent},
    get_error,
    joystick::JoystickId,
    sys::audio::{SDL_AudioStream, SDL_GetAudioStreamAvailable, SDL_GetAudioStreamData},
};
use serde::Deserialize;
use std::{
    ffi::c_int,
    mem::MaybeUninit,
    num::{NonZero, NonZeroUsize},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::runtime::Handle;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
    get_lang_str,
};

pub const RECORDING_SPEC: AudioSpec = AudioSpec {
    freq: Some(16000),
    channels: Some(1),
    format: Some(AudioFormat::f32_sys()),
};

/// Whether Bono was built with a backend that runs whisper on a GPU
pub(super) const GPU_BACKEND: bool = cfg!(any(
    feature = "hipblas",
    feature = "intel-sycl",
    feature = "vulkan",
    target_vendor = "apple"
));

/// The Whisper model the setup guide downloads, and Automatic uses: it understands every language,
/// and is quick on a GPU
const RECOMMENDED: &str = "ggml-large-v3-turbo-q5_0.bin";

/// Hears the pilot with Whisper, or Parakeet (see [`engine`]).
pub struct Whisper {
    /// Model and reusable session, shared with workers and only freed on one.
    ctx: Option<MaybeDone<Option<Arc<LoadedModel>>>>,
    /// The model `ctx` is loaded from: Parakeet's encoder for Parakeet
    loaded: Option<Arc<Utf8Path>>,
    /// "Choose a model" was clicked with nothing in use: the models are shown, and Automatic
    /// goes on until one is picked
    choosing: bool,
    /// Whether `ctx` was loaded to run on the GPU
    loaded_gpu: bool,
    /// The setup guide asked for this engine's model, for Automatic: it's downloaded, unless a
    /// model is picked, and used by Automatic once it's on disk
    setup: Option<Engine>,
    /// Setup's download was started: one that fails isn't started again until it's asked for again
    setup_started: bool,
    stream: Option<WhisperStream>,
    /// The joystick whose talk button started the recording: unplugging it means the release
    /// never comes
    talk_joystick: Option<JoystickId>,
    catalog: MaybeDone<Option<Vec<Model>>>,
    pref_path: Arc<Utf8Path>,
    threads: NonZeroUsize,
    client: reqwest::Client,
    files: super::ModelFiles,
    parakeet: parakeet::Files,
}

impl Whisper {
    /// The startup request gets three retries; manual requests get none.
    fn fetch_catalog(
        pref_path: Arc<Utf8Path>,
        client: reqwest::Client,
        retries: usize,
    ) -> MaybeDone<Option<Vec<Model>>> {
        MaybeDone::spawn(async move {
            for attempt in 0..=retries {
                match Model::fetch_catalog(&pref_path, &client).await {
                    Ok(catalog) => return Some(catalog),
                    Err(e) => {
                        if attempt == 0 {
                            error(t!("error.stt_catalog"), e);
                        } else {
                            log::warn!("Failed to fetch the stt catalog: {e}");
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
        config: &RawConfig,
        client: reqwest::Client,
        threads: Option<NonZeroUsize>,
        files: super::ModelFiles,
    ) -> Self {
        let pref_path = pref_path.clone();
        // Half the logical cores. Whisper's threads wait for each other at every step, so once there
        // are too few cores for them and the simulator, transcribing takes many times longer
        let threads = threads
            .or_else(|| NonZeroUsize::new(std::thread::available_parallelism().ok()?.get() / 2))
            .unwrap_or(NonZeroUsize::MIN);

        // What's used is loaded by `poll`, once the files it needs have been checked
        Self {
            client: client.clone(),
            ctx: None,
            loaded: None,
            choosing: false,
            loaded_gpu: *config.whisper_gpu(),
            setup: None,
            setup_started: false,
            stream: None,
            talk_joystick: None,
            catalog: Self::fetch_catalog(pref_path.clone(), client.clone(), 3),
            parakeet: parakeet::Files::new(&pref_path),
            pref_path,
            threads,
            files,
        }
    }

    /// The Speech To Text menu. `getting`: the languages whose voice is being set up.
    pub fn update(
        &mut self,
        rt: &Handle,
        config: &mut Config,
        ui: &mut egui::Ui,
        getting: &[ArrayStr<2>],
    ) {
        // Chosen by hand, then made automatic (by the setup guide)
        if config.get().whisper_model().is_some() {
            self.choosing = false;
        }
        let label = setup_label(self.missing(&config.get(), getting), t!("audio.speech_to_text"));
        ui.menu_button(label, |ui| {
            let automatic = config.get().whisper_model().is_none() && !self.choosing;
            ui.horizontal(|ui| {
                if ui.radio(automatic, t!("audio.stt_automatic")).clicked() && !automatic {
                    self.choosing = false;
                    config.update(|raw| raw.whisper_model.set(None));
                }
                ui.weak(t!("audio.stt_recommended"));
            });
            // It starts from the model in use, which stays loaded
            if ui.radio(!automatic, t!("audio.stt_choose")).clicked() && automatic {
                match self.loaded.clone() {
                    Some(path) => config.update(|raw| raw.whisper_model.set(Some(path))),
                    None => self.choosing = true,
                }
            }
            ui.separator();
            if automatic {
                self.note(rt, config, ui, getting);
            }
            if self.engine() == Some(Engine::Whisper) {
                if automatic {
                    ui.separator();
                }
                whisper_settings(ui, config);
            }
            if !automatic {
                if self.engine() == Some(Engine::Whisper) {
                    ui.separator();
                }
                self.models(rt, config, ui, getting);
            }
        });
    }

    /// What Automatic uses, and why, with what it needs.
    fn note(&mut self, rt: &Handle, config: &Config, ui: &mut egui::Ui, getting: &[ArrayStr<2>]) {
        let (wanted, heard, voices) = {
            let raw = config.get();
            (self.wanted(&raw, getting), *raw.heard_language(), voices(&raw, getting))
        };
        match engine::note(wanted, self.engine(), self.failed(), heard, &voices) {
            Note::Using(engine, why) => {
                ui.strong(t!("audio.stt_using", engine = engine_name(engine)));
                why_text(ui, &why, heard, &voices, false);
            }
            Note::HearOnly { lang, unknown } => {
                ui.strong(t!("audio.stt_using", engine = engine_name(Engine::Whisper)));
                let language = language::ui_name(lang);
                ui.label(format!(
                    "{} {}",
                    doesnt_hear(parakeet::NAME, &unknown),
                    t!("audio.stt_hear_only_question", language = &language),
                ));
                if ui.button(t!("audio.stt_hear_only", language = &language)).clicked() {
                    config.update(|raw| raw.heard_language.set(Some(lang)));
                }
            }
            Note::Missing { wanted, used, why } => {
                match used {
                    Some(used) => warning(ui, t!("audio.stt_still", engine = engine_name(used))),
                    None => warning(ui, t!("audio.stt_none").into()),
                }
                why_text(ui, &why, heard, &voices, wanted == Engine::Whisper);
                self.download(rt, ui, wanted);
            }
            Note::Failed(engine) => {
                warning(ui, t!("audio.stt_failed", engine = engine_name(engine)));
                if ui.button(t!("onboarding.retry")).clicked()
                    && let Some(path) = self.loaded.clone()
                {
                    self.set_model(path, *config.get().whisper_gpu());
                }
            }
        }
    }

    /// The button to download `engine`'s model, or how far its download is.
    fn download(&mut self, rt: &Handle, ui: &mut egui::Ui, engine: Engine) {
        let client = self.client.clone();
        let (on_disk, progress, size) = match engine {
            Engine::Parakeet => (
                self.parakeet.on_disk(&self.files),
                self.parakeet.progress(),
                Some(parakeet::SIZE),
            ),
            Engine::Whisper => match self.recommended(rt) {
                Some(model) => (
                    model.on_disk(rt),
                    model.download().map(Download::progress),
                    model.size,
                ),
                None => {
                    // Whisper's model is only downloaded with the catalog's size and hash
                    if matches!(self.catalog, MaybeDone::Done(None)) {
                        if ui.button(t!("onboarding.retry")).clicked() {
                            self.retry_catalog();
                        }
                    } else {
                        ui.spinner();
                    }
                    return;
                }
            },
        };
        if let Some(progress) = progress {
            egui::ProgressBar::new(progress as f32).show_percentage().ui(ui);
            return;
        }
        // Not checked yet: it may be there
        if on_disk != Some(false) {
            ui.spinner();
            return;
        }
        let size = size.map_or("?".into(), size_text);
        if ui.button(t!("audio.stt_download", size = size)).clicked() {
            match engine {
                Engine::Parakeet => self.parakeet.start_download(&client, &self.files),
                Engine::Whisper => {
                    if let Some(model) = self.recommended(rt) {
                        model.start_download(&client);
                    }
                }
            }
        }
    }

    /// The models to choose from: Parakeet, then Whisper's catalog.
    fn models(
        &mut self,
        rt: &Handle,
        config: &mut Config,
        ui: &mut egui::Ui,
        getting: &[ArrayStr<2>],
    ) {
        let picked = config.get().whisper_model().clone();
        let failed = self.failed();
        let languages = {
            let raw = config.get();
            let voices = voices(&raw, getting);
            engine::in_use(*raw.heard_language(), &voices, app_language(raw.locale()))
        };
        let parakeet_path = self.parakeet.model().clone();
        // What Automatic loaded is in use too, while one is chosen
        let loaded = self.loaded.clone();
        // The model picked doesn't hear a language in use
        let english_only = match &picked {
            Some(picked) => (self.catalog(rt).unwrap_or_default().iter())
                .find(|model| model.path == *picked && model.english_only)
                .map(|model| model.name.clone()),
            None => None,
        };
        let unheard = |name: &str, hears: fn(ArrayStr<2>) -> bool| {
            let unheard = languages.iter().copied().filter(|&lang| !hears(lang));
            Some((name.to_owned(), unheard.collect::<Vec<_>>()))
        };
        let unheard = match english_only {
            _ if picked.as_ref() == Some(&parakeet_path) => {
                unheard(parakeet::NAME, engine::parakeet_knows)
            }
            Some(name) => unheard(&name, |lang| lang == arrstr!("en")),
            None => None,
        };
        if let Some((name, unheard)) = unheard.filter(|(_, unheard)| !unheard.is_empty()) {
            warning(ui, doesnt_hear(&name, &unheard));
        }

        let mut reload = None;
        let mut retry = false;
        let mut deleted_recommended = false;
        let client = self.client.clone();
        scroll_bar_beside(ui);
        egui_extras::TableBuilder::new(ui)
            .id_salt("whisper-models")
            .striped(true)
            .columns(Column::auto(), 4)
            .column(Column::remainder())
            .header(20.0, |mut header| {
                for title in [
                    t!("audio.model"),
                    t!("audio.quantization"),
                    t!("audio.languages"),
                    t!("audio.size"),
                ] {
                    header.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|mut body| {
                body.row(18.0, |mut row| {
                    row.col(|ui| {
                        ui.label(parakeet::NAME);
                    });
                    row.col(|ui| {
                        ui.label("int8");
                    });
                    row.col(|ui| {
                        ui.label(t!("audio.stt_european"))
                            .on_hover_text(t!("audio.stt_credit"));
                    });
                    row.col(|ui| {
                        ui.label(size_text(parakeet::SIZE));
                    });
                    let on_disk = self.parakeet.shown_on_disk(rt, &self.files);
                    row.col(|ui| {
                        if self.parakeet.deleting() {
                            ui.spinner();
                            return;
                        }
                        let picked = picked.as_ref() == Some(&parakeet_path);
                        let used = picked || loaded.as_ref() == Some(&parakeet_path);
                        match (on_disk, self.parakeet.progress()) {
                            (Some(true), None) => {
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_enabled(!picked || failed, egui::Button::new(t!("audio.use")))
                                        .clicked()
                                    {
                                        reload = Some(parakeet_path.clone());
                                    }
                                    if delete_button(ui, Some(parakeet::SIZE), used) {
                                        self.parakeet.delete();
                                    }
                                });
                            }
                            (_, Some(progress)) => {
                                egui::ProgressBar::new(progress as f32).show_percentage().ui(ui);
                            }
                            (Some(false), None) => {
                                let partly = (self.parakeet.paths().iter())
                                    .any(|path| self.files.checked(path) == Some(true));
                                ui.horizontal(|ui| {
                                    if ui.button(egui_phosphor::bold::CLOUD_ARROW_DOWN).clicked() {
                                        self.parakeet.start_download(&client, &self.files);
                                    }
                                    // What an unfinished download left
                                    if partly && delete_button(ui, Some(parakeet::SIZE), false) {
                                        self.parakeet.delete();
                                    }
                                });
                            }
                            (None, None) => {}
                        }
                    });
                });

                let catalog = match self.catalog.update(rt) {
                    Some(Some(catalog)) => catalog,
                    unavailable => {
                        let unavailable = unavailable.is_some();
                        body.row(18.0, |mut row| {
                            row.col(|ui| {
                                if !unavailable {
                                    ui.spinner();
                                } else if ui.button(t!("onboarding.retry")).clicked() {
                                    retry = true;
                                }
                            });
                        });
                        return;
                    }
                };
                for model in catalog {
                    body.row(18.0, |mut row| {
                        row.col(|ui| {
                            ui.label(&model.name);
                        });
                        row.col(|ui| {
                            if let Some(ref q) = model.quantization {
                                ui.label(q);
                            }
                        });
                        row.col(|ui| {
                            ui.label(if model.english_only {
                                t!("audio.english_only")
                            } else {
                                t!("audio.all_languages")
                            });
                        });
                        row.col(|ui| {
                            if let Some(size) = model.size {
                                ui.label(size_text(size));
                            }
                        });

                        let on_disk = model.on_disk(rt);
                        if model.deletion.pending() {
                            row.col(|ui| {
                                ui.spinner();
                            });
                        } else if let Some(on_disk) = on_disk {
                            row.col(|ui| {
                                if on_disk && model.download().is_none() {
                                    let picked = picked.as_ref() == Some(&model.path);
                                    let used = picked || loaded.as_ref() == Some(&model.path);
                                    ui.horizontal(|ui| {
                                        ui.add_enabled_ui(!picked || failed, |ui| {
                                            if ui.button(t!("audio.use")).clicked() {
                                                reload = Some(model.path.clone());
                                            }
                                        });
                                        if model.delete_button(ui, used) {
                                            deleted_recommended |= model.file == RECOMMENDED;
                                        }
                                    });
                                } else if let Some(download) = model.download() {
                                    egui::ProgressBar::new(download.progress() as f32)
                                        .show_percentage()
                                        .ui(ui);
                                } else if ui
                                    .button(egui_phosphor::bold::CLOUD_ARROW_DOWN)
                                    .clicked()
                                {
                                    model.start_download(&client);
                                }
                            });
                        }
                    });
                }
            });
        if retry {
            self.retry_catalog();
        }
        // Setup doesn't get it again
        if deleted_recommended {
            self.setup = None;
        }
        if let Some(path) = reload {
            // A model picked by hand is what's used, whatever setup was getting
            self.setup = None;
            config.update(|raw| raw.whisper_model.set(Some(path.clone())));
            // The one Automatic loaded is only loaded again if it failed
            if self.loaded.as_ref() != Some(&path) || self.failed() {
                self.set_model(path, *config.get().whisper_gpu());
            }
        }
    }

    pub fn event(
        &mut self,
        rt: &Handle,
        event: &Event,
        config: &Config,
        device: Option<&sdl3::audio::AudioDevice>,
    ) -> Result<JoinHandle<anyhow::Result<Heard>>, bool> {
        let talk_button = config.get().talk_button().clone();
        return match talk(talk_button.as_ref(), event, self.talking()) {
            Talk::Begin => {
                self.talk_joystick = match *event {
                    Event::JoyButtonDown { which, .. } => Some(which),
                    _ => None,
                };
                self.begin(rt, device)
            }
            Talk::Finish => self.finish(config),
            Talk::Ignore => Err(false),
        };
    }

    /// Discard an interrupted recording instead of transcribing silence or partial speech.
    pub fn cancel_recording(&mut self) {
        self.stream = None;
    }

    /// Whether `event` ends the recording, so it must reach [`Self::event`] even when the UI used
    /// it (a text field with focus takes the talk key's release).
    pub fn ends_recording(&self, config: &Config, event: &Event) -> bool {
        talk(config.get().talk_button().as_ref(), event, self.talking()) == Talk::Finish
    }

    /// What [`talk`] needs to know of the recording: whether there's one, and who started it.
    fn talking(&self) -> Option<Option<JoystickId>> {
        self.recording().then_some(self.talk_joystick)
    }

    /// Whether the model used is loaded, and can listen.
    pub fn ready(&mut self, rt: &Handle) -> bool {
        matches!(
            self.ctx.as_mut().and_then(|ctx| ctx.update(rt)),
            Some(Some(_))
        )
    }

    /// The microphone is open, from the talk button's press until its release.
    #[inline]
    pub fn recording(&self) -> bool {
        self.stream.is_some()
    }

    /// Opens the microphone. When it can't be, what's heard right away says why.
    fn begin(
        &mut self,
        rt: &Handle,
        device: Option<&sdl3::audio::AudioDevice>,
    ) -> Result<JoinHandle<anyhow::Result<Heard>>, bool> {
        if self.stream.is_some() {
            return Err(false);
        }

        let heard = |heard| Ok(spawn(std::future::ready(heard)));
        let not_listening = |why| heard(Ok(Heard::NotListening(why)));
        let Some(device) = device else {
            return not_listening(NotListening::NoMicrophone);
        };
        let ctx = match self.ctx.as_mut().map(|ctx| ctx.update(rt)) {
            Some(Some(Some(ctx))) => ctx.clone(),
            Some(None) => return not_listening(NotListening::Loading),
            Some(Some(None)) | None => return not_listening(NotListening::NoModel),
        };

        let f = || {
            let (send, recv) = oneshot::channel();
            let stream = RecordingStream::new(device.open_recording_stream_with_callback(
                &RECORDING_SPEC,
                Recorder {
                    send: Some(send),
                    samples: Ok(Vec::new()),
                    remaining_buf: [0; 3],
                    remaining_count: 0,
                },
            )?);
            stream.resume()?;
            log::debug!("Started microphone recording");

            return Ok::<_, anyhow::Error>(WhisperStream {
                recv,
                session: PendingSession(DropInBackground::new(ctx)),
                stream,
            });
        };

        return match f() {
            Ok(stream) => {
                self.stream = Some(stream);
                Err(true)
            }
            Err(e) => heard(Err(e.context(t!("error.recording")))),
        };
    }

    fn finish(&mut self, config: &Config) -> Result<JoinHandle<anyhow::Result<Heard>>, bool> {
        let Some(stream) = self.stream.take() else {
            return Err(false);
        };
        log::debug!("Finishing microphone recording");
        return Ok(stream.finish(config.clone()));
    }
}

impl Whisper {
    pub fn catalog(&mut self, rt: &Handle) -> Option<&[Model]> {
        match self.catalog.update(rt) {
            Some(Some(catalog)) => Some(catalog),
            Some(None) | None => None,
        }
    }

    fn catalog_mut(&mut self, rt: &Handle) -> Option<&mut [Model]> {
        match self.catalog.update(rt) {
            Some(Some(catalog)) => Some(catalog),
            Some(None) | None => None,
        }
    }

    /// Called every frame: loads the model to use once it changes, and sets up the recommended
    /// one if the setup guide asked for it. `getting`: the languages whose voice is being set up.
    pub fn poll(&mut self, rt: &Handle, config: &Config, getting: &[ArrayStr<2>]) {
        if let Some(ctx) = &mut self.ctx {
            ctx.update(rt);
        }
        let gpu = *config.get().whisper_gpu();
        // While a model is deleted, a check from before may still find it: what's loaded stays
        self.parakeet.shown_on_disk(rt, &self.files);
        let turbo_deleted = (self.recommended(rt)).is_some_and(|model| {
            model.on_disk(rt);
            model.deletion.pending()
        });
        let deleting = self.parakeet.deleting() || turbo_deleted;
        let target = self.target(&config.get(), getting).filter(|_| !deleting);
        if let Some(target) = target {
            let whisper = target.as_ref().is_some_and(|path| path != self.parakeet.model());
            if target != self.loaded || (whisper && gpu != self.loaded_gpu) {
                match target {
                    Some(path) => self.set_model(path, gpu),
                    None => {
                        drop_in_background(self.ctx.take());
                        self.loaded = None;
                    }
                }
            }
        }

        let Some(wanted) = self.setup else {
            return;
        };
        // A model picked by hand is what's used: the guide unpicks the ones it shouldn't keep
        if config.get().whisper_model().is_some() {
            self.setup = None;
            return;
        }
        let start = !self.setup_started;
        let client = self.client.clone();
        let on_disk = match wanted {
            Engine::Parakeet => self.parakeet.on_disk(&self.files),
            Engine::Whisper => match self.recommended(rt) {
                Some(model) => model.on_disk(rt),
                None => return,
            },
        };
        match on_disk {
            // Automatic uses it
            Some(true) => self.setup = None,
            Some(false) if start => {
                match wanted {
                    Engine::Parakeet => self.parakeet.start_download(&client, &self.files),
                    Engine::Whisper => {
                        if let Some(model) = self.recommended(rt) {
                            model.start_download(&client);
                        }
                    }
                }
                self.setup_started = true;
            }
            Some(false) => {}
            // Not looked for yet
            None => {}
        }
    }

    /// Downloads `engine`'s model for Automatic (again, if it failed), unless a model is picked.
    /// `None` sets up nothing.
    pub fn setup(&mut self, engine: Option<Engine>) {
        self.retry_catalog();
        self.setup = engine;
        self.setup_started = false;
    }

    /// The model of `engine`, as the setup guide shows it: `None` for none, when the model picked
    /// stays.
    pub fn setup_item(&mut self, rt: &Handle, engine: Option<Engine>) -> Option<SetupItem> {
        let engine = engine?;
        let (failed, loading, loaded) = (self.failed(), self.loading(), self.loaded.clone());
        let state = |path: &Arc<Utf8Path>, on_disk, progress, download_failed| {
            let used = loaded.as_ref() == Some(path);
            match (on_disk, progress) {
                (Some(true), _) if used && failed => SetupState::LoadFailed,
                (Some(true), _) if used && loading => SetupState::Loading,
                (Some(true), _) => SetupState::Ready,
                (_, Some(progress)) => SetupState::Downloading(progress),
                _ if download_failed => SetupState::Failed,
                _ => SetupState::Missing,
            }
        };
        let (name, size, state) = match engine {
            Engine::Parakeet => {
                let on_disk = self.parakeet.on_disk(&self.files);
                let (progress, download_failed) = (self.parakeet.progress(), self.parakeet.failed());
                let state = state(self.parakeet.model(), on_disk, progress, download_failed);
                (parakeet::NAME, Some(parakeet::SIZE), state)
            }
            Engine::Whisper => {
                let path = self.turbo();
                let on_disk = self.files.checked(&path);
                match self.catalog.update(rt) {
                    Some(Some(catalog)) => {
                        let model = (catalog.iter_mut()).find(|model| model.file == RECOMMENDED)?;
                        let progress = model.download().map(Download::progress);
                        let state = state(&path, model.on_disk(rt), progress, model.failed());
                        (TURBO_NAME, model.size, state)
                    }
                    // It's only downloaded with the catalog's size and hash
                    _ if on_disk != Some(true) => {
                        return Some(match self.catalog.update(rt) {
                            None => SetupItem::Loading,
                            Some(_) => SetupItem::Unavailable,
                        });
                    }
                    _ => (TURBO_NAME, None, state(&path, on_disk, None, false)),
                }
            }
        };
        Some(SetupItem::Found {
            name: name.to_owned(),
            size,
            state,
        })
    }

    /// Where the recommended Whisper model is, known without the catalog.
    pub fn turbo(&self) -> Arc<Utf8Path> {
        self.pref_path.join("whisper").join(RECOMMENDED).into()
    }

    /// Where Parakeet is, as the model picked has it.
    pub fn parakeet_path(&self) -> &Arc<Utf8Path> {
        self.parakeet.model()
    }

    /// What Automatic wants now, and why. `getting`: the languages whose voice is being set up,
    /// which count as having one.
    pub fn wanted(&self, config: &RawConfig, getting: &[ArrayStr<2>]) -> (Engine, Why) {
        let voices = voices(config, getting);
        let reply = config.spoken_language().filter(|lang| voices.contains(lang));
        engine::wanted(*config.heard_language(), &voices, reply, app_language(config.locale()))
    }

    /// Whether the model is missing, or didn't load: the menu and setup guide have their sign.
    pub fn missing(&self, config: &RawConfig, getting: &[ArrayStr<2>]) -> bool {
        let parakeet_missing = || self.parakeet.on_disk(&self.files) == Some(false);
        self.failed()
            || match config.whisper_model() {
                Some(path) if path == self.parakeet.model() => parakeet_missing(),
                Some(path) => !self.files.is_file(path),
                None => match self.wanted(config, getting).0 {
                    Engine::Parakeet => parakeet_missing(),
                    Engine::Whisper => !self.files.is_file(&self.turbo()),
                },
            }
    }

    /// The model to load: `Some(None)` for none, and `None` while that isn't known yet (what's
    /// loaded stays).
    fn target(&self, config: &RawConfig, getting: &[ArrayStr<2>]) -> Option<Option<Arc<Utf8Path>>> {
        let parakeet = self.parakeet.model();
        let used = match config.whisper_model() {
            // Only with all its files: one of them alone doesn't load
            Some(path) if path == parakeet => {
                return Some(self.parakeet.on_disk(&self.files)?.then(|| path.clone()));
            }
            Some(path) => return Some(Some(path.clone())),
            None => engine::used(
                self.wanted(config, getting).0,
                self.parakeet.on_disk(&self.files),
                self.files.checked(&self.turbo()),
            ),
        };
        match used {
            Used::Engine(Engine::Parakeet) => Some(Some(parakeet.clone())),
            Used::Engine(Engine::Whisper) => Some(Some(self.turbo())),
            Used::Nothing => Some(None),
            Used::Wait => None,
        }
    }

    fn recommended(&mut self, rt: &Handle) -> Option<&mut Model> {
        self.catalog_mut(rt)?
            .iter_mut()
            .find(|model| model.file == RECOMMENDED)
    }

    pub fn failed(&self) -> bool {
        matches!(self.ctx, Some(MaybeDone::Done(None)))
    }

    /// Whether a model is still being deleted.
    pub fn deleting(&self) -> bool {
        self.parakeet.deletion_running()
            || matches!(&self.catalog, MaybeDone::Done(Some(catalog))
                if catalog.iter().any(|model| model.deletion.running()))
    }

    /// The engine in use: `None` if nothing is.
    pub fn engine(&self) -> Option<Engine> {
        let loaded = self.loaded.as_ref()?;
        Some(match loaded == self.parakeet.model() {
            true => Engine::Parakeet,
            false => Engine::Whisper,
        })
    }

    fn loading(&self) -> bool {
        matches!(self.ctx, Some(MaybeDone::Task(_)))
    }

    /// Loads `path`, Parakeet's encoder or a Whisper model, the latter to run on the GPU if `gpu`
    /// (and there's one).
    pub fn set_model(&mut self, path: Arc<Utf8Path>, gpu: bool) {
        self.loaded = Some(path.clone());
        self.loaded_gpu = gpu;
        // The model replaced (or still loading) is freed on a worker
        drop_in_background(self.ctx.take());
        let threads = self.threads;
        if path == *self.parakeet.model() {
            let dir = self.parakeet.folder().to_owned();
            self.ctx = Some(MaybeDone::spawn_blocking(move || {
                match Parakeet::load(&dir, threads) {
                    Ok(parakeet) => {
                        log::info!("Loaded {dir} for speech to text");
                        Some(Arc::new(LoadedModel::new(LoadedEngine::Parakeet(parakeet))))
                    }
                    Err(e) => {
                        error(t!("error.stt_model"), format!("{e:#}"));
                        None
                    }
                }
            }));
            return;
        }
        self.ctx = Some(MaybeDone::spawn_blocking(move || {
            let mut params = WhisperContextParameters::default();
            params.use_gpu(gpu);
            let gpu = if gpu { gpu::choose() } else { None };
            params.gpu_device(gpu.as_ref().map_or(0, |gpu| gpu.device));
            // Off on the CPU, where it's a little slower
            let flash = gpu.as_ref().is_some_and(|gpu| gpu.flash);
            params.flash_attn(flash);
            match WhisperContext::new_with_params(path.as_os_str(), params) {
                Ok(ctx) => {
                    let gpu = gpu.map_or("none".to_owned(), |gpu| gpu.name);
                    log::info!(
                        "Loaded {path} for speech to text (GPU: {gpu}, flash attention: {flash})"
                    );
                    let model = Arc::new(LoadedModel::new(LoadedEngine::Whisper(WhisperModel {
                        ctx,
                        session: SessionCache::default(),
                        threads,
                    })));
                    if flash {
                        let model = Arc::downgrade(&model);
                        // Not part of loading, as questions are refused until that's done. Nor for
                        // a model that was replaced while it loaded
                        Handle::current().spawn_blocking(move || {
                            if let Some(model) = model.upgrade()
                                && let LoadedEngine::Whisper(whisper) = &model.engine
                            {
                                whisper.warm_up();
                            }
                        });
                    }
                    Some(model)
                }
                Err(e) => {
                    error(t!("error.stt_model"), e);
                    None
                }
            }
        }));
    }
}

/// The languages with a voice, and those `getting` one (being set up), which count as having one.
fn voices(config: &RawConfig, getting: &[ArrayStr<2>]) -> Vec<ArrayStr<2>> {
    let mut voices = config.voices().keys().copied().collect::<Vec<_>>();
    for &lang in getting {
        if !voices.contains(&lang) {
            voices.push(lang);
        }
    }
    voices
}

/// Automatic's Whisper, as the menu names it.
const TURBO_NAME: &str = "Whisper large-v3-turbo";

/// The engine Automatic uses, as the menu names it.
fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Parakeet => parakeet::NAME,
        Engine::Whisper => TURBO_NAME,
    }
}

/// `langs` as the app names them.
fn language_names(langs: &[ArrayStr<2>]) -> String {
    let names = langs.iter().map(|&lang| language::ui_name(lang));
    names.collect::<Vec<_>>().join(", ")
}

/// That the model `name` doesn't hear `langs`.
fn doesnt_hear(name: &str, langs: &[ArrayStr<2>]) -> String {
    t!("audio.stt_doesnt_hear", engine = name, languages = language_names(langs))
}

/// Why Automatic wants the engine it does. `whisper_does`: Whisper is wanted and not downloaded.
fn why_text(
    ui: &mut egui::Ui,
    why: &Why,
    heard: Option<ArrayStr<2>>,
    voices: &[ArrayStr<2>],
    whisper_does: bool,
) {
    let point = || {
        t!(
            "audio.stt_point",
            menu = t!("engineer"),
            setting = t!("engineer.heard_language")
        )
    };
    let text = match why {
        Why::Known(langs) => t!("audio.stt_parakeet", languages = language_names(langs)),
        Why::Unknown(langs) => {
            let mut text = doesnt_hear(parakeet::NAME, langs);
            // Several voices in languages Parakeet knows: one of them can be the one heard
            let known = voices.iter().filter(|&&lang| engine::parakeet_knows(lang));
            if heard.is_none() && known.count() > 1 {
                text = format!("{text} {}", point());
            }
            if whisper_does {
                text = format!("{text} {}", t!("audio.stt_whisper_does"));
            }
            text
        }
        Why::Ambiguous(langs) => format!(
            "{} {}",
            t!("audio.stt_ambiguous", languages = language_names(langs)),
            point()
        ),
        Why::AnyLanguage => t!("audio.stt_any").into(),
    };
    ui.label(text);
}

/// The settings that only apply to Whisper.
fn whisper_settings(ui: &mut egui::Ui, config: &mut Config) {
    config.maybe_update(|raw| {
        raw.min_confidence
            .edit(*raw.min_confidence(), |confidence| {
                ui.add(
                    egui::Slider::new(confidence, 0.0..=1.0)
                        .text(t!("audio.language_confidence"))
                        .custom_formatter(|value, _| format!("{:.0}%", value * 100.0))
                        .custom_parser(|text| {
                            let text = text.trim().trim_end_matches('%').trim();
                            text.parse::<f64>().ok().map(|percent| percent / 100.0)
                        }),
                )
                .on_hover_text(t!("audio.language_confidence_hover"))
                .changed()
            })
    });
    // Without a GPU backend built in, it runs on the CPU either way
    if GPU_BACKEND {
        // Changing it reloads the model, in `poll`
        config.maybe_update(|raw| {
            raw.whisper_gpu.edit(*raw.whisper_gpu(), |gpu| {
                ui.checkbox(gpu, t!("audio.whisper_gpu"))
                    .on_hover_text(t!("audio.whisper_gpu_hover"))
                    .changed()
            })
        });
    }
}

/// `text`, with the sign of something to set up.
fn warning(ui: &mut egui::Ui, text: String) {
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(egui_phosphor::bold::WARNING).color(SETUP_COLOR));
        ui.label(text);
    });
}

/// The button to delete a model of `size` bytes, unless it's `used`. Returns whether it was
/// clicked.
fn delete_button(ui: &mut egui::Ui, size: Option<u64>, used: bool) -> bool {
    let size = size.map_or("?".into(), size_text);
    ui.add_enabled(!used, egui::Button::new(egui_phosphor::bold::TRASH))
        .on_hover_text(t!("audio.delete", size = size))
        .on_disabled_hover_text(t!("audio.delete_in_use"))
        .clicked()
}

/// What an event does to the talk button's recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Talk {
    Begin,
    Finish,
    Ignore,
}

/// `recording` is `None` without a recording, or the joystick that started it.
fn talk(
    button: Option<&ConfigButton>,
    event: &Event,
    recording: Option<Option<JoystickId>>,
) -> Talk {
    let Some(button) = button else {
        return Talk::Ignore;
    };
    // The key held down repeats, but it's still the one press
    if button.pressed(event) {
        return Talk::Begin;
    }
    let Some(joystick) = recording else {
        return Talk::Ignore;
    };
    if button.released(event) {
        return Talk::Finish;
    }
    // A key's release goes to whichever window has focus, so once Bono loses it the release may
    // never come. Joysticks are heard in the background, so their release still will
    if matches!(button, ConfigButton::Key(_))
        && matches!(
            event,
            Event::Window {
                win_event: WindowEvent::FocusLost,
                ..
            }
        )
    {
        return Talk::Finish;
    }
    // An unplugged joystick never sends its button's release
    if let Event::JoyDeviceRemoved { which, .. } = *event
        && joystick == Some(which)
    {
        return Talk::Finish;
    }
    Talk::Ignore
}

struct WhisperStream {
    recv: oneshot::Receiver<anyhow::Result<Vec<f32>>>,
    session: PendingSession,
    stream: RecordingStream<Recorder>,
}

impl WhisperStream {
    fn finish(self, config: Config) -> JoinHandle<anyhow::Result<Heard>> {
        return self.session.finish(self.recv, self.stream, config);
    }
}

impl Drop for Whisper {
    fn drop(&mut self) {
        drop_in_background(self.ctx.take());
    }
}

/// Frees `T` on a worker: whisper's model and state free large (GPU) buffers.
fn drop_in_background<T: Send + 'static>(value: T) {
    // Without a runtime (tests) there's no worker to free it on
    if let Ok(rt) = Handle::try_current() {
        // Not this crate's `spawn_blocking`: its handle aborts the task once dropped
        rt.spawn_blocking(move || drop(value));
    }
}

/// Owns `T`, and frees it with [`drop_in_background`] unless taken back.
struct DropInBackground<T: Send + 'static>(Option<T>);

impl<T: Send + 'static> DropInBackground<T> {
    fn new(value: T) -> Self {
        Self(Some(value))
    }

    fn as_ref(&self) -> &T {
        self.0.as_ref().expect("taken only once")
    }

    fn into_inner(mut self) -> T {
        self.0.take().expect("taken only once")
    }
}

impl<T: Send + 'static> Drop for DropInBackground<T> {
    fn drop(&mut self) {
        drop_in_background(self.0.take());
    }
}

/// One loaded model generation, kept by the recordings that started with it.
struct LoadedModel {
    engine: LoadedEngine,
    /// The last recording finished: earlier ones still waiting aren't heard
    latest: AtomicU64,
}

impl LoadedModel {
    fn new(engine: LoadedEngine) -> Self {
        Self {
            engine,
            latest: AtomicU64::new(0),
        }
    }
}

enum LoadedEngine {
    Whisper(WhisperModel),
    Parakeet(Parakeet),
}

/// The languages an engine can hear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Knows {
    Everything,
    English,
    /// Those of [`engine::parakeet_knows`]
    Parakeet,
}

impl LoadedEngine {
    fn knows(&self) -> Knows {
        match self {
            Self::Whisper(whisper) if whisper.ctx.is_multilingual() => Knows::Everything,
            Self::Whisper(_) => Knows::English,
            Self::Parakeet(_) => Knows::Parakeet,
        }
    }
}

/// Workers serialize access instead of allocating a second state.
struct WhisperModel {
    ctx: WhisperContext,
    session: SessionCache<WhisperSession>,
    threads: NonZeroUsize,
}

impl WhisperModel {
    /// Transcribes a second of silence. The driver builds its shaders the first time after installing
    /// Bono or the driver, which takes seconds that the first question would wait for.
    fn warm_up(&self) {
        let start = Instant::now();
        let silence = self.session.run(
            || self.new_session(),
            |session| session.speech_to_text(arrstr!("en"), &[0.0; 16000]),
        );
        match silence {
            Ok(_) => log::debug!("Warmed up speech to text in {:?}", start.elapsed()),
            Err(e) => log::warn!("Unable to warm up speech to text: {e}"),
        }
    }

    fn new_session(&self) -> anyhow::Result<WhisperSession> {
        let start = Instant::now();
        let state = self.ctx.create_state()?;
        log::debug!("Created whisper's state in {:?}", start.elapsed());
        Ok(WhisperSession {
            state,
            threads: self.threads,
            multilingual: self.ctx.is_multilingual(),
        })
    }
}

/// Access only on blocking workers: initialization, use and destruction can all be expensive.
struct SessionCache<T>(Mutex<Option<T>>);

impl<T> Default for SessionCache<T> {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

impl<T> SessionCache<T> {
    fn run<R>(
        &self,
        create: impl FnOnce() -> anyhow::Result<T>,
        use_session: impl FnOnce(&mut T) -> anyhow::Result<R>,
    ) -> anyhow::Result<R> {
        let mut cached = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Whisper session worker panicked"))?;
        if cached.is_none() {
            *cached = Some(create()?);
        }
        let result = use_session(cached.as_mut().unwrap());
        // Retry after processing failures. Invalid UTF-8 is only a text conversion error,
        // after inference succeeded, so the state remains reusable.
        if result.as_ref().is_err_and(|error| {
            !matches!(
                error.downcast_ref::<whisper_rs::WhisperError>(),
                Some(whisper_rs::WhisperError::InvalidUtf8 { .. })
            )
        }) {
            *cached = None;
        }
        result
    }
}

/// A recording retains its model generation, including when settings or the engine change
/// mid-recording.
struct PendingSession(DropInBackground<Arc<LoadedModel>>);

pub struct WhisperSession {
    state: WhisperState,
    threads: NonZeroUsize,
    multilingual: bool,
}

impl PendingSession {
    fn finish(
        self,
        recv: oneshot::Receiver<anyhow::Result<Vec<f32>>>,
        mut stream: RecordingStream<Recorder>,
        config: Config,
    ) -> JoinHandle<anyhow::Result<Heard>> {
        let released = Instant::now();
        // Supersede queued workers even if capture shutdown below fails. Aborting a started
        // blocking task alone cannot stop it from using the session.
        let ticket = self
            .0
            .as_ref()
            .latest
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        // Blocks the UI thread, briefly: sdl3's stream can't be sent to another thread (it holds
        // the `!Send` `AudioSubsystem`), and the callback must be done with before it's freed.
        // Pausing, flushing and locking each wait for at most one callback call; draining copies
        // what arrived since the last one (a device buffer, milliseconds of audio). The
        // transcription, the slow part, runs in the background.
        if let Err(e) = stream.pause() {
            return spawn(std::future::ready(Err(e.into())));
        }

        if let Err(e) = stream.flush() {
            return spawn(std::future::ready(Err(e.into())));
        }

        // The callback only runs when new data arrives, so read what `flush` released
        // (and anything else still queued) ourselves before the `Recorder` is dropped
        let raw = stream.raw();
        match stream.lock() {
            Some(mut recorder) => recorder.drain(raw),
            None => return spawn(std::future::ready(Err(get_error().into()))),
        }

        return spawn_blocking(move || {
            let samples = recv.recv()??;
            self.0.into_inner().hear(&samples, &config, ticket, released)
        });
    }
}

impl LoadedModel {
    /// What was said in `samples`, the recording `ticket` (released at `released`). Blocks.
    fn hear(
        &self,
        samples: &[f32],
        config: &Config,
        ticket: u64,
        released: Instant,
    ) -> anyhow::Result<Heard> {
        let start = Instant::now();
        log::debug!("Sample count: {}", samples.len());
        if samples.len() < MIN_SAMPLES {
            return Ok(Heard::TooShort);
        }
        let (voices, min_confidence, heard_language, spoken_language) = {
            let config = config.get();
            let voices = config.voices().keys().copied().collect::<Vec<_>>();
            (
                voices,
                *config.min_confidence(),
                *config.heard_language(),
                config.engineer_reply_language(),
            )
        };

        if let Some(problem) = transcription_problem(
            self.engine.knows(),
            heard_language,
            spoken_language,
            &voices,
        ) {
            return Ok(problem);
        }
        // Whisper makes words up for silence, so it never hears it
        if !voiced(samples) {
            log::debug!("Only silence on the radio");
            return Ok(Heard::Nothing);
        }
        let superseded = || self.latest.load(Ordering::Relaxed) != ticket;
        let model = match &self.engine {
            LoadedEngine::Whisper(model) => model,
            LoadedEngine::Parakeet(parakeet) => {
                if superseded() {
                    return Ok(Heard::Nothing);
                }
                let text = speech(&parakeet.transcribe(samples)?);
                log::debug!("Speech to text took {:?} with Parakeet", start.elapsed());
                return Ok(match text.is_empty() {
                    true => Heard::Nothing,
                    // Parakeet doesn't say which language it heard: it's only known if there's one
                    false => Heard::Question {
                        text,
                        lang: only_language(heard_language, &voices),
                        released,
                    },
                });
            }
        };
        // With a fixed reply language, the input needs no voice of its own.
        let candidates = match spoken_language {
            Some(_) => (0..=whisper_rs::get_lang_max_id())
                .filter_map(|id| whisper_language(id as usize))
                .collect::<Vec<_>>(),
            None => voices.clone(),
        };
        model.session.run(
            || {
                anyhow::ensure!(!superseded(), "Superseded recording");
                model.new_session()
            },
            |session| {
                if superseded() {
                    return Ok(Heard::Nothing);
                }
                let waited = start.elapsed();
                let detect = Instant::now();
                let spoken = Spoken::resolve(session.multilingual, heard_language, &voices, || {
                    // Only detection needs this separate pass. Full transcription prepares
                    // its own mel, so a fixed language goes straight to `speech_to_text`.
                    session.state.pcm_to_mel(samples, session.threads.get())?;
                    session.detect_language(&candidates, min_confidence)
                })?;
                if superseded() {
                    return Ok(Heard::Nothing);
                }
                let transcribe = Instant::now();
                // Something said in a language without a voice is still transcribed: if it's no
                // words after all, it was noise
                let heard = match spoken {
                    Spoken::Voiced(lang) => match session.speech_to_text(lang, samples)? {
                        text if text.is_empty() => Heard::Nothing,
                        text => Heard::Question {
                            text,
                            lang: Some(lang),
                            released,
                        },
                    },
                    Spoken::Unvoiced(lang) => match session.speech_to_text(lang, samples)? {
                        text if text.is_empty() => Heard::Nothing,
                        text => Heard::Unvoiced { text, lang },
                    },
                    Spoken::Unclear => Heard::Nothing,
                };
                log::debug!(
                    "Speech to text took {:?}: waited {waited:?} for the state, {:?} \
                     resolving the language (including mel preparation for detection), {:?} transcribing",
                    start.elapsed(),
                    transcribe - detect,
                    transcribe.elapsed(),
                );
                return Ok(heard);
            },
        )
    }
}

impl WhisperSession {
    /// The language spoken, among `voices` (the languages with a voice) if it's one of them.
    fn detect_language(
        &mut self,
        voices: &[ArrayStr<2>],
        min_confidence: f32,
    ) -> anyhow::Result<Spoken> {
        if !self.multilingual {
            return Ok(Spoken::Voiced(arrstr!("en")));
        }

        let (_, probs) = self.state.lang_detect(0, self.threads.get())?;
        let mut likeliest = (probs.iter().copied().enumerate())
            .filter_map(|(id, prob)| Some((whisper_language(id)?, prob)))
            .collect::<Vec<_>>();
        likeliest.sort_by(|(_, a), (_, b)| b.total_cmp(a));
        log::info!(
            "Languages heard on the radio: {:?}",
            &likeliest[..likeliest.len().min(3)]
        );

        return Ok(Spoken::choose(&likeliest, voices, min_confidence));
    }

    /// What was said in `lang`: empty if it's no words.
    fn speech_to_text(&mut self, lang: ArrayStr<2>, samples: &[f32]) -> anyhow::Result<String> {
        // Use greedy decoding on both CPU and GPU, keeping the default full audio
        // context and temperature fallback. A shorter context makes turbo repeat what was said.
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

        params.set_n_threads(c_int::try_from(self.threads.get()).unwrap_or(c_int::MAX));
        params.set_language(Some(&lang));
        params.set_detect_language(false);
        // Recordings are independent even though their buffers are reused.
        params.set_no_context(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        // No blanks, nor tokens for sounds that aren't speech
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);

        self.state.full(params, samples)?;
        let mut text = String::new();
        for segment in self.state.as_iter() {
            // Whisper makes words up for silence and noise ("Thank you.")
            let no_speech = segment.no_speech_probability();
            if no_speech > NO_SPEECH {
                log::debug!("Not speech ({no_speech}): {:?}", segment.to_str_lossy()?);
                continue;
            }
            text.push_str(segment.to_str()?);
        }

        return Ok(speech(&text));
    }
}

/// Less than this is too short to have said anything (0.3 s): the talk button was likely
/// pressed by accident.
const MIN_SAMPLES: usize = 16000 * 3 / 10;

/// What loudness is measured over (20 ms).
const FRAME: usize = 16000 / 50;

/// How loud (RMS) a frame must be to be more than silence: -50 dBFS. Speech into a microphone is
/// louder, while one unplugged, muted or blocked records zeros, and a quiet room little more.
const SILENCE: f32 = 0.003_162;

/// How long (0.1 s) a recording must be louder than [`SILENCE`] to be transcribed: a click of
/// the talk button isn't speech.
const MIN_VOICED: usize = 16000 / 10;

/// Whether `samples` are loud enough, for long enough, to have anything said in them. Whisper
/// can't tell: it makes words up for silence and says it's sure they're speech.
fn voiced(samples: &[f32]) -> bool {
    let len = FRAME as f32;
    let loud = (samples.chunks_exact(FRAME))
        .filter(|frame| {
            // Around its mean: some inputs record silence offset from zero
            let mean = frame.iter().sum::<f32>() / len;
            let power = frame.iter().map(|s| (s - mean).powi(2)).sum::<f32>() / len;
            power > SILENCE * SILENCE
        })
        .count();
    loud * FRAME >= MIN_VOICED
}

/// Reject incompatible settings before inference or language detection starts.
fn transcription_problem(
    knows: Knows,
    heard_language: Option<ArrayStr<2>>,
    spoken_language: Option<ArrayStr<2>>,
    voices: &[ArrayStr<2>],
) -> Option<Heard> {
    if voices.is_empty() {
        return Some(Heard::NoVoice(None));
    }
    match knows {
        Knows::Everything => {}
        Knows::English => {
            if let Some(lang) = heard_language.filter(|lang| *lang != arrstr!("en")) {
                return Some(Heard::UnsupportedLanguage(lang));
            }
            if spoken_language.is_none() && !voices.contains(&arrstr!("en")) {
                return Some(Heard::NoVoice(Some(arrstr!("en"))));
            }
        }
        // With several languages, it hears those it knows: with none, the first it doesn't
        Knows::Parakeet => {
            let unknown = match only_language(heard_language, voices) {
                Some(lang) => Some(lang),
                None if !voices.iter().any(|&lang| engine::parakeet_knows(lang)) => {
                    voices.first().copied()
                }
                None => None,
            };
            if let Some(lang) = unknown.filter(|&lang| !engine::parakeet_knows(lang)) {
                return Some(Heard::UnsupportedLanguage(lang));
            }
        }
    }
    None
}

/// The language of everything said, if there's only one: the Heard language, or a lone voice's.
fn only_language(heard: Option<ArrayStr<2>>, voices: &[ArrayStr<2>]) -> Option<ArrayStr<2>> {
    heard.or(match voices {
        [only] => Some(*only),
        _ => None,
    })
}

/// How sure whisper has to be of a language without a voice to tell the pilot there's none for
/// it: noise is often a little likelier to be some language or other.
const UNVOICED_CONFIDENCE: f32 = 0.5;

/// How much less likely than one without a voice the likeliest language with a voice can be
/// before it's taken as said in the other: close languages (Catalan heard as Spanish) are
/// still answered.
const FAR_BEHIND: f32 = 0.25;

/// How likely a segment can be to be no speech at all, and still be taken as said.
const NO_SPEECH: f32 = 0.8;

/// Whisper's language `id`, as the voices have it.
fn whisper_language(id: usize) -> Option<ArrayStr<2>> {
    let code = get_lang_str(i32::try_from(id).ok()?)?;
    ArrayStr::from_str(code).map(crate::language::voice_code)
}

/// The language spoken on the radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spoken {
    /// One with a voice, or a fixed input language that can use a different reply voice
    Voiced(ArrayStr<2>),
    /// One clearly without a voice
    Unvoiced(ArrayStr<2>),
    /// Unclear, if anything was said at all
    Unclear,
}

impl Spoken {
    /// Resolve known languages before doing any detection or preparing audio for it.
    fn resolve(
        multilingual: bool,
        heard_language: Option<ArrayStr<2>>,
        voices: &[ArrayStr<2>],
        detect: impl FnOnce() -> anyhow::Result<Self>,
    ) -> anyhow::Result<Self> {
        let fixed = match multilingual {
            false => Some(arrstr!("en")),
            true => only_language(heard_language, voices),
        };
        match fixed {
            Some(lang) => Ok(Self::Voiced(lang)),
            None => detect(),
        }
    }

    /// The language spoken, from how likely each is (`likeliest`, likeliest first), among
    /// `voices` (the languages with a voice) if it's one of them.
    fn choose(
        likeliest: &[(ArrayStr<2>, f32)],
        voices: &[ArrayStr<2>],
        min_confidence: f32,
    ) -> Self {
        let voiced = (likeliest.iter().copied())
            .find(|(lang, prob)| voices.contains(lang) && *prob >= min_confidence);
        match (likeliest.first().copied(), voiced) {
            // Clearly another language, the likeliest with a voice far behind it
            (Some((lang, prob)), voiced)
                if !voices.contains(&lang)
                    && prob >= min_confidence.max(UNVOICED_CONFIDENCE)
                    && voiced.is_none_or(|(_, voiced)| voiced < prob * FAR_BEHIND) =>
            {
                Spoken::Unvoiced(lang)
            }
            (_, Some((lang, _))) => Spoken::Voiced(lang),
            _ => Spoken::Unclear,
        }
    }
}

/// `text` without what whisper writes for sounds that aren't speech (`[BLANK_AUDIO]`, `(music)`,
/// `♪`): empty if there are no words left.
fn speech(text: &str) -> String {
    let mut speech = String::with_capacity(text.len());
    let mut closing = None;
    for c in text.chars() {
        match (closing, c) {
            (Some(close), c) => {
                if c == close {
                    closing = None;
                }
            }
            (None, '[') => closing = Some(']'),
            (None, '(') => closing = Some(')'),
            (None, '♩' | '♪' | '♫' | '♬') => {}
            (None, c) => speech.push(c),
        }
    }

    match speech.chars().any(char::is_alphanumeric) {
        true => speech.split_whitespace().collect::<Vec<_>>().join(" "),
        false => String::new(),
    }
}

struct Segment {
    start: Duration,
    end: Duration,
    text: String,
}

struct Recorder {
    send: Option<oneshot::Sender<anyhow::Result<Vec<f32>>>>,
    samples: anyhow::Result<Vec<f32>>,
    remaining_buf: [u8; 3],
    remaining_count: usize,
}

impl Recorder {
    /// Reads everything the stream currently has available.
    fn drain(&mut self, stream: *mut SDL_AudioStream) {
        if self.send.as_ref().is_none_or(oneshot::Sender::is_closed) {
            self.send = None;
            self.samples = Ok(Vec::new());
            return;
        }

        let Ok(ref mut samples) = self.samples else {
            return;
        };

        if let Err(e) = Self::try_drain(
            stream,
            samples,
            &mut self.remaining_buf,
            &mut self.remaining_count,
        ) {
            self.samples = Err(e);
            self.send = None;
        }
    }

    fn try_drain(
        stream: *mut SDL_AudioStream,
        samples: &mut Vec<f32>,
        remaining_buf: &mut [u8; 3],
        remaining_count: &mut usize,
    ) -> anyhow::Result<()> {
        loop {
            let available = unsafe { SDL_GetAudioStreamAvailable(stream) };
            let Some(available) =
                NonZero::new(usize::try_from(available).map_err(|_| get_error())?)
            else {
                return Ok(());
            };

            if Self::try_read(stream, available, samples, remaining_buf, remaining_count)? == 0 {
                return Ok(());
            }
        }
    }

    /// Reads up to `available` bytes, returning how many were read.
    fn try_read(
        stream: *mut SDL_AudioStream,
        available: NonZero<usize>,
        samples: &mut Vec<f32>,
        remaining_buf: &mut [u8; 3],
        remaining_count: &mut usize,
    ) -> anyhow::Result<usize> {
        unsafe {
            samples.try_reserve(
                available
                    .get()
                    .div_ceil(4)
                    .unchecked_add((*remaining_count > 0) as usize),
            )?;

            let spare_bytes = std::slice::from_raw_parts_mut(
                samples
                    .spare_capacity_mut()
                    .as_mut_ptr()
                    .cast::<MaybeUninit<u8>>(),
                available.get().unchecked_add(*remaining_count),
            );

            let (pre, post) = spare_bytes.split_at_mut_unchecked(*remaining_count);
            pre.write_copy_of_slice(remaining_buf.get_unchecked(..*remaining_count));

            match usize::try_from(SDL_GetAudioStreamData(
                stream,
                post.as_mut_ptr().cast(),
                post.len() as c_int,
            )) {
                Ok(read) => {
                    let count = remaining_count.unchecked_add(read);
                    let (full, partial) = spare_bytes
                        .get_unchecked(..count)
                        .assume_init_ref()
                        .as_chunks::<4>();

                    let len = samples.len();
                    samples.set_len(len.unchecked_add(full.len()));

                    std::ptr::copy_nonoverlapping(
                        partial.as_ptr(),
                        remaining_buf.as_mut_ptr(),
                        partial.len(),
                    );
                    *remaining_count = partial.len();
                    return Ok(read);
                }
                Err(_) => return Err(get_error().into()),
            }
        }
    }
}

impl AudioRecordingCallback<f32> for Recorder {
    // `_available` is the number of bytes just added in the *device's* format (divided by
    // `size_of::<f32>()` by sdl3-rs), not what can be read in `RECORDING_SPEC`, so ask SDL instead
    fn callback(&mut self, stream: &mut AudioStream, _available: i32) {
        self.drain(stream.raw());
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let Some(send) = self.send.take() else { return };

        if self.remaining_count != 0 {
            log::warn!("Audio recording returned unexpected trailing bytes");
        }

        let _ = send.send(std::mem::replace(&mut self.samples, Ok(Vec::new())));
    }
}

const CATALOG_URL: &str = "https://huggingface.co/api/models/ggerganov/whisper.cpp/tree/main";
const FILE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/";

pub struct Model {
    pub size: Option<u64>,
    pub name: String,
    pub english_only: bool,
    pub quantization: Option<String>,
    pub sha256: Option<String>,
    pub file: String,
    pub path: Arc<Utf8Path>,
    /// Whether it's on disk, and when that check started
    on_disk: tokio::sync::watch::Receiver<Option<(bool, Instant)>>,
    download: Option<Download>,
    deletion: Deletion,
}

impl Model {
    /// Whether it's on disk: `None` until it's been checked, and while it's deleted.
    pub fn on_disk(&mut self, rt: &Handle) -> Option<bool> {
        let found = *self.on_disk.borrow();
        self.deletion.on_disk(rt, found)
    }

    /// The button to delete it, unless it's `used`. Returns whether it was clicked.
    fn delete_button(&mut self, ui: &mut egui::Ui, used: bool) -> bool {
        let clicked = delete_button(ui, self.size, used);
        if clicked {
            self.download = None;
            let path = self.path.as_std_path().to_path_buf();
            // The phonemizer inherited the file if it was started while whisper was reading it, and
            // keeps it open
            self.deletion.start(phonemizer::stop, vec![path]);
        }
        clicked
    }

    /// The download under way: `None` if there's none, it failed, or it's done.
    pub fn download(&self) -> Option<&Download> {
        (self.download.as_ref()).filter(|download| !download.failed() && !download.finished())
    }

    /// Whether the last download failed.
    pub fn failed(&self) -> bool {
        self.download.as_ref().is_some_and(Download::failed)
    }

    pub fn start_download(&mut self, client: &reqwest::Client) -> &mut Download {
        // One that failed starts over, as does one whose file went missing since
        if self.failed() || self.download.as_ref().is_some_and(Download::finished) {
            self.download = None;
        }
        self.download.get_or_insert_with(|| {
            let path = Arc::into_raw(self.path.clone());
            let path = unsafe { Arc::<Path>::from_raw(path as *const Path) };
            Download::new(
                client.get(format!("{FILE_URL}{}", self.file)),
                path,
                self.size,
                self.sha256.clone().map(crate::DownloadDigest::Sha256),
            )
        })
    }

    async fn fetch_catalog(
        pref_dir: &Utf8Path,
        client: &reqwest::Client,
    ) -> reqwest::Result<Vec<Model>> {
        #[derive(Debug, Clone, Deserialize)]
        struct Lfs {
            #[serde(default)]
            size: Option<u64>,
            #[serde(default)]
            oid: Option<String>,
        }

        #[derive(Debug, Clone, Deserialize)]
        struct Catalog {
            #[serde(default)]
            r#type: Option<String>,
            #[serde(default)]
            size: Option<u64>,
            #[serde(default)]
            oid: Option<String>,
            #[serde(default)]
            lfs: Option<Lfs>,
            #[serde(default)]
            path: Option<String>,
        }

        let catalog = client
            .get(CATALOG_URL)
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<Catalog>>()
            .await?;

        let mut models = Vec::<Model>::new();
        for entry in catalog {
            if entry.r#type.as_deref() != Some("file") {
                continue;
            }

            let Some(file) = entry.path else {
                continue;
            };

            let Some(name) = file
                .strip_prefix("ggml-")
                .and_then(|file| file.strip_suffix(".bin"))
            else {
                continue;
            };

            // Only files in its own folder
            if name.contains(['/', '\\']) {
                continue;
            }

            let (name, quantization) = match name.rsplit_once("-q") {
                Some((name, quantization)) => (name, Some(quantization)),
                None => (name, None),
            };

            let (name, english_only) = match name.strip_suffix(".en") {
                Some(name) => (name, true),
                None => (name, false),
            };

            let size = match (entry.size, entry.lfs.as_ref()) {
                (.., Some(lfs)) => lfs.size,
                (size @ Some(..), None) => size,
                (None, None) => None,
            };

            let oid = match (entry.oid, entry.lfs) {
                (.., Some(lfs)) => lfs.oid,
                (oid @ Some(..), None) => oid,
                (None, None) => None,
            };

            let path = Arc::<Utf8Path>::from(pref_dir.join("whisper").join(&file));
            models.push(Model {
                size,
                name: name.to_owned(),
                english_only,
                quantization: quantization.map(ToOwned::to_owned),
                sha256: oid,
                file,
                path: path.clone(),
                on_disk: updating(Duration::from_millis(1234), None, move |_| {
                    let path = path.clone();
                    let started = Instant::now();
                    async move { Some((tokio::fs::try_exists(&*path).await.ok()?, started)) }
                }),
                download: None,
                deletion: Deletion::default(),
            });
        }

        models.sort_by_key(|lhs| lhs.size);
        return Ok(models);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesizer::{Effects, Voice};
    use camino::Utf8PathBuf;
    use sdl3::keyboard::{Mod, Scancode};
    use std::collections::BTreeMap;
    use whisper_rs::get_lang_id;

    fn key(scancode: Scancode, down: bool) -> Event {
        let (timestamp, window_id, keycode, keymod, repeat, which, raw) =
            (0, 1, None, Mod::NOMOD, false, 0, 0);
        let scancode = Some(scancode);
        match down {
            true => Event::KeyDown {
                timestamp,
                window_id,
                keycode,
                scancode,
                keymod,
                repeat,
                which,
                raw,
            },
            false => Event::KeyUp {
                timestamp,
                window_id,
                keycode,
                scancode,
                keymod,
                repeat,
                which,
                raw,
            },
        }
    }

    fn window(win_event: WindowEvent) -> Event {
        Event::Window {
            timestamp: 0,
            window_id: 1,
            win_event,
        }
    }

    #[test]
    fn the_talk_keys_release_finishes_the_recording() {
        let button = ConfigButton::Key(Scancode::Space);
        let release = key(Scancode::Space, false);
        assert_eq!(
            talk(Some(&button), &key(Scancode::Space, true), None),
            Talk::Begin
        );
        assert_eq!(talk(Some(&button), &release, Some(None)), Talk::Finish);
        // Nothing to finish, so a text field may keep it
        assert_eq!(talk(Some(&button), &release, None), Talk::Ignore);
        assert_eq!(
            talk(Some(&button), &key(Scancode::A, false), Some(None)),
            Talk::Ignore
        );
        assert_eq!(talk(None, &release, Some(None)), Talk::Ignore);
    }

    #[test]
    fn losing_focus_finishes_a_key_recording() {
        let key = ConfigButton::Key(Scancode::Space);
        let focus_lost = window(WindowEvent::FocusLost);
        assert_eq!(talk(Some(&key), &focus_lost, Some(None)), Talk::Finish);
        assert_eq!(talk(Some(&key), &focus_lost, None), Talk::Ignore);
        assert_eq!(
            talk(Some(&key), &window(WindowEvent::FocusGained), Some(None)),
            Talk::Ignore
        );

        // A joystick's release is still heard in the background
        let joystick = ConfigButton::Joystick {
            guid: unsafe { std::mem::zeroed() },
            button: 0,
        };
        assert_eq!(talk(Some(&joystick), &focus_lost, Some(None)), Talk::Ignore);
    }

    #[test]
    fn unplugging_the_talk_joystick_finishes_its_recording() {
        let button = ConfigButton::Joystick {
            guid: unsafe { std::mem::zeroed() },
            button: 0,
        };
        let removed = |id: u32| Event::JoyDeviceRemoved {
            timestamp: 0,
            which: JoystickId::new(id),
        };
        let wheel = Some(JoystickId::new(7));
        assert_eq!(talk(Some(&button), &removed(7), Some(wheel)), Talk::Finish);
        // Another joystick, a key's recording, or no recording at all
        assert_eq!(talk(Some(&button), &removed(8), Some(wheel)), Talk::Ignore);
        assert_eq!(talk(Some(&button), &removed(7), Some(None)), Talk::Ignore);
        assert_eq!(talk(Some(&button), &removed(7), None), Talk::Ignore);
    }

    /// Sends the thread it's dropped on.
    struct DropThread(std::sync::mpsc::Sender<std::thread::ThreadId>);

    impl Drop for DropThread {
        fn drop(&mut self) {
            let _ = self.0.send(std::thread::current().id());
        }
    }

    #[test]
    fn dropping_frees_on_a_worker() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (send, recv) = std::sync::mpsc::channel();
        drop(DropInBackground::new(DropThread(send)));
        let dropped_on = recv.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_ne!(dropped_on, std::thread::current().id());
    }

    #[test]
    fn taking_back_doesnt_free() {
        let (send, recv) = std::sync::mpsc::channel();
        let value = DropInBackground::new(DropThread(send)).into_inner();
        assert!(recv.try_recv().is_err());
        drop(value);
        assert_eq!(recv.try_recv().unwrap(), std::thread::current().id());
    }

    #[test]
    fn a_pending_session_dropped_is_freed_on_a_worker() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (send, recv) = std::sync::mpsc::channel();
        let (send_value, value) = oneshot::channel();
        drop(DropInBackground::new(value));
        // Created about when it's dropped: whichever worker is last frees it
        std::thread::spawn(move || {
            let _ = send_value.send(DropThread(send));
        })
        .join()
        .unwrap();
        let dropped_on = recv.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_ne!(dropped_on, std::thread::current().id());
    }

    #[test]
    fn session_cache_reuses_and_retries_after_errors() {
        let cache = SessionCache::default();
        let mut created = 0;
        for expected in 1..=3 {
            let count = cache
                .run(
                    || {
                        created += 1;
                        Ok(0)
                    },
                    |state| {
                        *state += 1;
                        Ok(*state)
                    },
                )
                .unwrap();
            assert_eq!(count, expected);
        }
        assert_eq!(created, 1);
        let failed: anyhow::Result<()> = cache.run(
            || panic!("already cached"),
            |_| anyhow::bail!("processing failed"),
        );
        assert!(failed.is_err());
        assert!(
            cache
                .run::<()>(|| anyhow::bail!("creation failed"), |_| panic!("no state"))
                .is_err()
        );
        assert_eq!(cache.run(|| Ok(42), |state| Ok(*state)).unwrap(), 42);
    }

    #[test]
    fn text_conversion_errors_retain_the_session() {
        let cache = SessionCache::default();
        let failed: anyhow::Result<()> = cache.run(
            || Ok(42),
            |_| {
                let invalid = vec![0xff];
                Err(
                    whisper_rs::WhisperError::from(std::str::from_utf8(&invalid).unwrap_err())
                        .into(),
                )
            },
        );
        assert!(failed.is_err());
        assert_eq!(
            cache
                .run(|| panic!("healthy state retained"), |state| Ok(*state))
                .unwrap(),
            42
        );
    }

    #[test]
    fn concurrent_runs_share_one_session() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache = SessionCache::default();
        let created = AtomicUsize::new(0);
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    barrier.wait();
                    cache
                        .run(
                            || {
                                created.fetch_add(1, Ordering::SeqCst);
                                Ok(0)
                            },
                            |state| {
                                *state += 1;
                                Ok(())
                            },
                        )
                        .unwrap();
                });
            }
        });
        assert_eq!(created.load(Ordering::SeqCst), 1);
        assert_eq!(
            cache
                .run(|| panic!("already cached"), |state| Ok(*state))
                .unwrap(),
            8
        );
        let replacement = SessionCache::default();
        assert_eq!(replacement.run(|| Ok(0), |state| Ok(*state)).unwrap(), 0);
        assert_eq!(
            cache
                .run(|| panic!("old generation retained"), |state| Ok(*state))
                .unwrap(),
            8
        );
    }

    #[test]
    fn cached_session_is_retained_until_model_drops_on_worker() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let (send, recv) = std::sync::mpsc::channel();
        let cache = Arc::new(SessionCache::default());
        cache.run(|| Ok(DropThread(send)), |_| Ok(())).unwrap();
        // Cancelling a recording releases only its shared handle.
        drop(DropInBackground::new(cache.clone()));
        assert!(recv.try_recv().is_err());
        drop(DropInBackground::new(cache));
        assert_ne!(
            recv.recv_timeout(Duration::from_secs(10)).unwrap(),
            std::thread::current().id()
        );
    }

    /// Needs `BONO_WHISPER` (a whisper model) and `BONO_VOICE_EN`/`_ES` (Piper voices).
    #[test]
    #[ignore]
    fn detects_the_language_spoken() {
        let whisper = std::env::var("BONO_WHISPER").unwrap();
        let threads = NonZeroUsize::new(4).unwrap();
        let ctx =
            WhisperContext::new_with_params(&whisper, WhisperContextParameters::default()).unwrap();

        let voices = BTreeMap::from([
            (
                arrstr!("en"),
                Utf8PathBuf::from(std::env::var("BONO_VOICE_EN").unwrap()),
            ),
            (
                arrstr!("es"),
                Utf8PathBuf::from(std::env::var("BONO_VOICE_ES").unwrap()),
            ),
        ]);
        let effects = Effects {
            radio: 0.0,
            beep_volume: 0.0,
            speed: 1.0,
        };

        let mut session = WhisperSession {
            state: ctx.create_state().unwrap(),
            threads,
            multilingual: ctx.is_multilingual(),
        };

        for (lang, text) in [
            (
                arrstr!("es"),
                "¿Cuánto combustible me queda y cuándo tengo que parar?",
            ),
            (
                arrstr!("en"),
                "How much fuel do I have left, and when do I need to pit?",
            ),
        ] {
            let mut voice = Voice::load(&voices[&lang], threads, false).unwrap();
            let speech = voice.synthesize(text, effects).unwrap();
            // Whisper hears at 16 kHz
            let ratio = speech.sample_rate as f64 / 16000.0;
            let samples = (0..(speech.samples.len() as f64 / ratio) as usize)
                .map(|i| speech.samples[(i as f64 * ratio) as usize])
                .collect::<Vec<_>>();

            session.state.pcm_to_mel(&samples, threads.get()).unwrap();
            let (_, probs) = session.state.lang_detect(0, threads.get()).unwrap();
            for code in ["en", "es"] {
                println!("{code}: {}", probs[get_lang_id(code).unwrap() as usize]);
            }

            let candidates = voices.keys().copied().collect::<Vec<_>>();
            let Spoken::Voiced(detected) = session.detect_language(&candidates, 0.1).unwrap()
            else {
                panic!("{lang} wasn't detected among the voices");
            };
            let heard = session.speech_to_text(detected, &samples).unwrap();
            println!("{lang} -> {detected}: {heard}");
            assert_eq!(detected, lang);
        }
    }

    #[test]
    fn fixed_languages_never_run_detection() {
        let en = arrstr!("en");
        let es = arrstr!("es");
        for (multilingual, heard, voices, expected) in [
            (true, None, vec![es], es),
            (true, Some(en), vec![es], en),
            (true, Some(es), vec![en, es], es),
            (true, Some(es), vec![en], es),
            (
                true,
                Some(arrstr!("nn")),
                vec![en, arrstr!("no")],
                arrstr!("nn"),
            ),
            (false, None, vec![en, es], en),
            (false, Some(en), vec![en, es], en),
        ] {
            let spoken = Spoken::resolve(multilingual, heard, &voices, || {
                panic!("Fixed languages must skip detection, including its mel preparation")
            })
            .unwrap();
            assert_eq!(spoken, Spoken::Voiced(expected));
        }
    }

    #[test]
    fn rejects_incompatible_model_and_voice_settings_before_transcription() {
        let [en, es] = [arrstr!("en"), arrstr!("es")];
        let (english, everything) = (Knows::English, Knows::Everything);
        assert!(
            matches!(transcription_problem(english, None, None, &[es]), Some(Heard::NoVoice(Some(lang))) if lang == en)
        );
        assert!(
            matches!(transcription_problem(english, Some(es), Some(en), &[en]), Some(Heard::UnsupportedLanguage(lang)) if lang == es)
        );
        assert!(matches!(
            transcription_problem(everything, None, None, &[]),
            Some(Heard::NoVoice(None))
        ));
        assert!(transcription_problem(english, None, Some(es), &[es]).is_none());
        assert!(transcription_problem(english, Some(en), None, &[en]).is_none());
        assert!(transcription_problem(everything, Some(es), None, &[en]).is_none());
    }

    #[test]
    fn parakeet_refuses_the_only_language_it_does_not_know() {
        let [es, ca] = [arrstr!("es"), arrstr!("ca")];
        let problem = |heard, voices: &[_]| transcription_problem(Knows::Parakeet, heard, None, voices);
        assert!(matches!(problem(Some(ca), &[es]), Some(Heard::UnsupportedLanguage(l)) if l == ca));
        assert!(matches!(problem(None, &[ca]), Some(Heard::UnsupportedLanguage(l)) if l == ca));
        // It hears the ones it knows, and says so when it knows none
        assert!(problem(None, &[ca, es]).is_none());
        let eu = arrstr!("eu");
        assert!(matches!(problem(None, &[ca, eu]), Some(Heard::UnsupportedLanguage(l)) if l == ca));
        assert!(problem(Some(es), &[ca]).is_none());
        assert!(matches!(problem(Some(es), &[]), Some(Heard::NoVoice(None))));
        // Norwegian, under any of its codes
        assert!(problem(Some(arrstr!("nb")), &[es]).is_some());
    }

    #[test]
    fn only_one_language_is_known_beforehand() {
        let [es, fr] = [arrstr!("es"), arrstr!("fr")];
        assert_eq!(only_language(Some(es), &[es, fr]), Some(es));
        assert_eq!(only_language(None, &[fr]), Some(fr));
        assert_eq!(only_language(None, &[es, fr]), None);
        assert_eq!(only_language(None, &[]), None);
    }

    /// A `Whisper` for the models in `pref_path`, without Whisper's catalog.
    fn whisper(pref_path: &Utf8Path) -> Whisper {
        Whisper {
            ctx: None,
            loaded: None,
            choosing: false,
            loaded_gpu: false,
            setup: None,
            setup_started: false,
            stream: None,
            talk_joystick: None,
            catalog: MaybeDone::Done(None),
            pref_path: Arc::from(pref_path),
            threads: NonZeroUsize::MIN,
            client: reqwest::Client::new(),
            files: super::super::ModelFiles::new(),
            parakeet: parakeet::Files::new(pref_path),
        }
    }

    async fn config(dir: &Path) -> Config {
        let path = dir.join("config.json");
        std::fs::write(&path, b"{}").unwrap();
        Config::new(&path).await.unwrap()
    }

    /// Waits until `done`, for at most 10 s.
    fn until(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        tokio::task::block_in_place(|| {
            while !done() {
                assert!(Instant::now() < deadline, "timed out");
                std::thread::sleep(Duration::from_millis(20));
            }
        });
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn automatic_loads_what_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let pref_path = Utf8Path::from_path(dir.path()).unwrap();
        let config = config(dir.path()).await;
        let whisper = whisper(pref_path);
        let (turbo, parakeet) = (whisper.turbo(), whisper.parakeet.model().clone());
        let target = || whisper.target(&config.get(), &[]);
        // English: Parakeet is wanted
        assert_eq!(whisper.wanted(&config.get(), &[]).0, Engine::Parakeet);

        until(|| target().is_some());
        assert_eq!(target(), Some(None));
        assert!(whisper.missing(&config.get(), &[]));

        // Whisper hears until Parakeet's there
        std::fs::create_dir_all(turbo.parent().unwrap()).unwrap();
        std::fs::write(&*turbo, []).unwrap();
        until(|| target() == Some(Some(turbo.clone())));
        assert!(whisper.missing(&config.get(), &[]));

        // One of Parakeet's files isn't enough
        std::fs::create_dir_all(whisper.parakeet.folder()).unwrap();
        let [encoder, rest @ ..] = whisper.parakeet.paths();
        std::fs::write(&encoder, []).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(target(), Some(Some(turbo.clone())));
        for path in rest {
            std::fs::write(path, []).unwrap();
        }
        until(|| target() == Some(Some(parakeet.clone())));
        assert!(!whisper.missing(&config.get(), &[]));

        std::fs::remove_dir_all(dir.path().join("parakeet")).unwrap();
        std::fs::remove_file(&*turbo).unwrap();
        until(|| target() == Some(None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_picked_model_that_is_gone_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let pref_path = Utf8Path::from_path(dir.path()).unwrap();
        let config = config(dir.path()).await;
        let mut whisper = whisper(pref_path);
        let rt = Handle::current();

        let small = Arc::<Utf8Path>::from(pref_path.join("whisper").join("ggml-small.bin"));
        let parakeet = whisper.parakeet.model().clone();
        // Whisper's is loaded (and fails) as before; Parakeet isn't loaded without its files
        for (picked, loaded) in [(small.clone(), Some(small)), (parakeet, None)] {
            config.update(|raw| raw.whisper_model.set(Some(picked.clone())));
            until(|| {
                tokio::task::block_in_place(|| whisper.poll(&rt, &config, &[]));
                whisper.loaded == loaded && whisper.missing(&config.get(), &[])
            });
            assert_eq!(*config.get().whisper_model(), Some(picked));
        }
    }

    /// Needs `BONO_PARAKEET` (Parakeet's folder) and `BONO_VOICE_ES` (a Piper voice).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn a_recording_keeps_the_engine_it_started_with() {
        let rt = Handle::current();
        let dir = tempfile::tempdir().unwrap();
        let pref_path = Utf8Path::from_path(dir.path()).unwrap();
        let config = config(dir.path()).await;
        let mut synth = crate::synthesizer::Synthesizer::new(&rt, &config.get());
        let es = arrstr!("es");
        let voice = Utf8PathBuf::from(std::env::var("BONO_VOICE_ES").unwrap());
        config.update(|raw| raw.set_voice(&rt, es, voice, &mut synth));

        let mut whisper = whisper(pref_path);
        let model = Arc::new(LoadedModel::new(LoadedEngine::Parakeet(
            tokio::task::block_in_place(parakeet::tests::load),
        )));
        whisper.ctx = Some(MaybeDone::Done(Some(model.clone())));
        let session = PendingSession(DropInBackground::new(model));
        // The engine changes while the talk button is held
        whisper.set_model(whisper.turbo(), false);

        let samples = parakeet::tests::spoken("BONO_VOICE_ES", "¿Cuánto combustible me queda?");
        let model = session.0.into_inner();
        let ticket = model.latest.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let heard = tokio::task::block_in_place(|| {
            model.hear(&samples, &config, ticket, Instant::now())
        });
        match heard.unwrap() {
            // Its only voice's language
            Heard::Question { text, lang, .. } => {
                println!("{text}");
                assert!(text.to_lowercase().contains("combustible"), "{text}");
                assert_eq!(lang, Some(es));
            }
            _ => panic!("not heard as a question"),
        }
        // A newer recording finished meanwhile: this one isn't heard
        model.latest.fetch_add(1, Ordering::Relaxed);
        let heard = tokio::task::block_in_place(|| {
            model.hear(&samples, &config, ticket, Instant::now())
        });
        assert!(matches!(heard.unwrap(), Heard::Nothing));
    }

    #[test]
    fn automatic_input_with_multiple_voices_runs_detection() {
        let mut called = false;
        let spoken = Spoken::resolve(true, None, &[arrstr!("en"), arrstr!("es")], || {
            called = true;
            Ok(Spoken::Voiced(arrstr!("es")))
        })
        .unwrap();
        assert!(called);
        assert_eq!(spoken, Spoken::Voiced(arrstr!("es")));
        assert!(
            Spoken::resolve(true, None, &[arrstr!("en"), arrstr!("es")], || {
                anyhow::bail!("detection failed")
            })
            .is_err()
        );
    }

    #[test]
    fn chooses_the_language_spoken() {
        let [en, es, fr, ca, pt] = [
            arrstr!("en"),
            arrstr!("es"),
            arrstr!("fr"),
            arrstr!("ca"),
            arrstr!("pt"),
        ];
        let both = [en, es];
        let choose = |likeliest: &[(ArrayStr<2>, f32)], voices: &[ArrayStr<2>]| {
            Spoken::choose(likeliest, voices, 0.1)
        };

        assert_eq!(choose(&[(es, 0.8), (en, 0.1)], &both), Spoken::Voiced(es));
        // Another language, with no voice for it
        assert_eq!(
            choose(&[(fr, 0.9), (en, 0.05)], &both),
            Spoken::Unvoiced(fr)
        );
        // A close one is answered in the voice nearest it
        assert_eq!(choose(&[(ca, 0.6), (es, 0.3)], &both), Spoken::Voiced(es));
        // Far behind it, even if it's likely enough
        assert_eq!(
            choose(&[(fr, 0.85), (en, 0.12)], &both),
            Spoken::Unvoiced(fr)
        );
        // Noise isn't any language in particular
        assert_eq!(choose(&[(fr, 0.3), (en, 0.05)], &both), Spoken::Unclear);
        assert_eq!(choose(&[], &both), Spoken::Unclear);
        // With a single voice, too
        assert_eq!(
            choose(&[(en, 0.9), (es, 0.05)], &[es]),
            Spoken::Unvoiced(en)
        );
        assert_eq!(choose(&[(es, 0.4), (pt, 0.3)], &[es]), Spoken::Voiced(es));
        assert_eq!(choose(&[(en, 0.2), (es, 0.05)], &[es]), Spoken::Unclear);
    }

    #[test]
    fn silence_isnt_transcribed() {
        // A tone `amplitude` loud, for `secs`, after a second of `floor`
        let recording = |floor: f32, amplitude: f32, secs: f32| {
            let tone = (0..(16000.0 * secs) as usize).map(|i| amplitude * (i as f32 * 0.1).sin());
            let floor = (0..16000).map(|i| floor * (i as f32 * 0.37).sin());
            floor.chain(tone).collect::<Vec<_>>()
        };
        // Unplugged, muted, or a quiet room (-63 dBFS)
        assert!(!voiced(&[0.0; 3 * 16000]));
        assert!(!voiced(&recording(0.001, 0.001, 2.0)));
        // An input whose silence is offset from zero
        let offset = recording(0.001, 0.001, 2.0)
            .iter()
            .map(|s| s + 0.05)
            .collect::<Vec<_>>();
        assert!(!voiced(&offset));
        // A click of the button
        assert!(!voiced(&recording(0.001, 0.5, 0.02)));
        // Something said, even quietly (-43 dBFS), after a quiet room's noise
        assert!(voiced(&recording(0.001, 0.01, 0.5)));
        assert!(voiced(&recording(0.001, 0.2, 0.2)));
    }

    #[test]
    fn leaves_out_what_isnt_speech() {
        assert_eq!(speech(" How are the tyres? "), "How are the tyres?");
        assert_eq!(speech("[BLANK_AUDIO]"), "");
        assert_eq!(speech(" (music) ♪ "), "");
        assert_eq!(speech(" ... "), "");
        assert_eq!(speech("[engine noise] Box  this lap?"), "Box this lap?");
        assert_eq!(
            speech("¿Cuánto (risas) combustible?"),
            "¿Cuánto combustible?"
        );
    }
}
