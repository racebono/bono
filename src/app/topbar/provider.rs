//! The language model the engineer talks through: who runs it, which model, and how.

use crate::{
    app::topbar::{pick, setup_label},
    config::{
        ClaudeConfig, Config, ModelConfig, ModelProvider, OllamaConfig, OpenAiConfig, Setting,
    },
    provider::{self, ModelOption},
    task::MaybeDone,
};
use bono_i18n::t;
use std::num::NonZeroU32;
use tokio::runtime::Handle;

const CLAUDE_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const OLLAMA_EFFORTS: [&str; 4] = ["none", "low", "medium", "high"];
/// Empty: `reasoning_effort` left out, the model's default
const OPENAI_EFFORTS: [&str; 5] = ["", "none", "low", "medium", "high"];

/// How long Ollama keeps the model loaded, in seconds (`None`: Ollama's own)
fn keep_alive_options() -> [(Option<f64>, &'static str); 5] {
    [
        (None, t!("provider.keep_default")),
        (Some(300.0), t!("provider.keep_5_minutes")),
        (Some(1800.0), t!("provider.keep_30_minutes")),
        (Some(3600.0), t!("provider.keep_1_hour")),
        (Some(-1.0), t!("provider.keep_forever")),
    ]
}

/// An effort level as the pilot reads it: the API's own name for one it doesn't know.
fn effort_name(effort: &str) -> String {
    match effort {
        "" => t!("effort.default"),
        "none" => t!("effort.none"),
        "low" => t!("effort.low"),
        "medium" => t!("effort.medium"),
        "high" => t!("effort.high"),
        "xhigh" => t!("effort.xhigh"),
        "max" => t!("effort.max"),
        other => return other.to_owned(),
    }
    .to_owned()
}

pub struct ProviderTopbar {
    client: reqwest::Client,
    /// The provider's models, fetched when the tab first shows them
    models: Option<Models>,
    /// The address being typed (Ollama's host, or the API's URL): set once it's done (Enter, or
    /// clicking away)
    host: Option<String>,
    /// Claude's API key being typed, likewise
    api_key: Option<String>,
    /// Claude's workspace being typed, likewise
    workspace_id: Option<String>,
}

/// The models one provider offers.
struct Models {
    provider: ModelProvider,
    /// Where they were asked: Ollama's host, or the API's URL
    host: Option<String>,
    list: MaybeDone<anyhow::Result<Vec<ModelOption>>>,
}

impl ProviderTopbar {
    pub fn new(client: &reqwest::Client) -> Self {
        Self {
            client: client.clone(),
            models: None,
            host: None,
            api_key: None,
            workspace_id: None,
        }
    }

    pub fn update(&mut self, rt: &Handle, ui: &mut egui::Ui, config: &mut Config) {
        let model = config.get().model.clone();
        let provider = *model.provider();
        let host = location(&model);

        // Asked again for another provider, or another address
        let stale = self
            .models
            .as_ref()
            .is_none_or(|models| models.provider != provider || models.host != host);
        if stale {
            self.fetch(model.clone());
        }

        config.maybe_update(|raw| {
            let mut changed = raw.model.provider.edit(provider, |provider| {
                ui.horizontal(|ui| {
                    ui.label(t!("provider.label"));
                    let no_credentials = !provider::claude::has_credentials(&model.claude);
                    let mut claude = ui
                        .selectable_value(
                            provider,
                            ModelProvider::Claude,
                            setup_label(no_credentials, "Claude"),
                        )
                        .on_hover_text(t!("provider.claude_hover"));
                    if no_credentials {
                        claude = claude.on_hover_text(t!("provider.no_claude_credentials"));
                    }
                    let ollama = ui
                        .selectable_value(provider, ModelProvider::Ollama, "Ollama")
                        .on_hover_text(t!("provider.ollama_hover"));
                    let openai = ui
                        .selectable_value(provider, ModelProvider::OpenAi, "OpenAI")
                        .on_hover_text(t!("provider.openai_hover"));
                    claude.clicked() || ollama.clicked() || openai.clicked()
                })
                .inner
            });
            ui.separator();

            changed |= match provider {
                ModelProvider::Claude => self.claude(rt, ui, &mut raw.model.claude),
                ModelProvider::Ollama => self.ollama(rt, ui, &mut raw.model.ollama),
                ModelProvider::OpenAi => self.openai(rt, ui, &mut raw.model.openai),
            };
            changed
        });

        ui.separator();
        ui.weak(t!("provider.applies_next"));
    }

    fn claude(&mut self, rt: &Handle, ui: &mut egui::Ui, config: &mut ClaudeConfig) -> bool {
        let mut changed = false;
        if let Some(api_key) = text_field(
            &mut self.api_key,
            ui,
            config.api_key().clone(),
            t!("provider.api_key"),
            t!("provider.api_key_hint"),
            t!("provider.api_key_hover"),
            true,
        ) {
            config.api_key.set(api_key);
            changed = true;
        }
        if let Some(workspace_id) = text_field(
            &mut self.workspace_id,
            ui,
            config.workspace_id().clone(),
            t!("provider.workspace_id"),
            t!("provider.workspace_id_hint"),
            t!("provider.workspace_id_hover"),
            false,
        ) {
            config.workspace_id.set(workspace_id);
            changed = true;
        }
        changed |= self.model(rt, ui, config.model().clone(), &mut config.model);
        changed |= pick(
            ui,
            t!("provider.effort"),
            config.effort().clone(),
            &mut config.effort,
            CLAUDE_EFFORTS.map(|effort| (effort.to_owned(), effort_name(effort))),
            |effort| effort_name(effort),
        )
        .on_hover_text(t!("provider.claude_effort_hover"))
        .changed();
        changed |= tokens(
            ui,
            t!("provider.max_tokens"),
            *config.max_tokens(),
            &mut config.max_tokens,
            128_000,
        )
        .on_hover_text(t!("provider.max_tokens_hover"))
        .changed();
        changed |= config.compaction.edit(*config.compaction(), |compaction| {
            ui.checkbox(compaction, t!("provider.compaction"))
                .on_hover_text(t!("provider.compaction_hover"))
                .changed()
        });
        changed
    }

    fn openai(&mut self, rt: &Handle, ui: &mut egui::Ui, config: &mut OpenAiConfig) -> bool {
        let mut changed = false;
        if let Some(base_url) = text_field(
            &mut self.host,
            ui,
            config.base_url().clone(),
            t!("provider.base_url"),
            provider::OPENAI_DEFAULT_BASE_URL,
            t!("provider.base_url_hover"),
            false,
        ) {
            config.base_url.set(base_url);
            changed = true;
        }
        changed |= self.model(rt, ui, config.model().clone(), &mut config.model);
        changed |= pick(
            ui,
            t!("provider.effort"),
            config.effort().clone(),
            &mut config.effort,
            OPENAI_EFFORTS.map(|effort| (effort.to_owned(), effort_name(effort))),
            |effort| effort_name(effort),
        )
        .on_hover_text(t!("provider.openai_effort_hover"))
        .changed();
        changed |= tokens(
            ui,
            t!("provider.max_tokens"),
            *config.max_tokens(),
            &mut config.max_tokens,
            1_048_576,
        )
        .on_hover_text(t!("provider.max_tokens_hover"))
        .changed();
        changed
    }

    fn ollama(&mut self, rt: &Handle, ui: &mut egui::Ui, config: &mut OllamaConfig) -> bool {
        let mut changed = false;
        if let Some(host) = text_field(
            &mut self.host,
            ui,
            config.host().clone(),
            t!("provider.host"),
            t!("provider.host_hint"),
            t!("provider.host_hover"),
            false,
        ) {
            config.host.set(host);
            changed = true;
        }
        changed |= self.model(rt, ui, config.model().clone(), &mut config.model);
        changed |= pick(
            ui,
            t!("provider.effort"),
            config.effort().clone(),
            &mut config.effort,
            OLLAMA_EFFORTS.map(|effort| (effort.to_owned(), effort_name(effort))),
            |effort| effort_name(effort),
        )
        .on_hover_text(t!("provider.ollama_effort_hover"))
        .changed();
        changed |= tokens(
            ui,
            t!("provider.context"),
            *config.context_length(),
            &mut config.context_length,
            1_048_576,
        )
        .on_hover_text(t!("provider.context_hover"))
        .changed();
        changed |= tokens(
            ui,
            t!("provider.max_tokens"),
            *config.max_tokens(),
            &mut config.max_tokens,
            1_048_576,
        )
        .on_hover_text(t!("provider.max_tokens_hover"))
        .changed();
        changed |= pick(
            ui,
            t!("provider.keep_loaded"),
            *config.keep_alive(),
            &mut config.keep_alive,
            keep_alive_options(),
            |keep_alive| match keep_alive {
                Some(seconds) if *seconds < 0.0 => t!("provider.keep_forever").into(),
                Some(seconds) => t!("provider.keep_seconds", seconds = seconds),
                None => t!("provider.keep_default").into(),
            },
        )
        .on_hover_text(t!("provider.keep_loaded_hover"))
        .changed();
        changed
    }

    /// The model, picked from the provider's.
    fn model(
        &mut self,
        rt: &Handle,
        ui: &mut egui::Ui,
        current: String,
        setting: &mut Setting<String>,
    ) -> bool {
        let mut refresh = false;
        let changed = setting.edit(current, |model| {
            ui.menu_button(t!("provider.model", model = model), |ui| {
                let before = model.clone();
                match self
                    .models
                    .as_mut()
                    .and_then(|models| models.list.update(rt))
                {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(t!("provider.looking_for_models"));
                        });
                    }
                    Some(Err(e)) => {
                        ui.colored_label(ui.visuals().error_fg_color, format!("{e:#}"));
                    }
                    Some(Ok(models)) if models.is_empty() => {
                        ui.label(t!("provider.no_models"));
                    }
                    Some(Ok(models)) => {
                        for option in models.iter() {
                            let text = match option.size {
                                Some(size) => format!("{} ({})", option.name, size_text(size)),
                                None => option.name.clone(),
                            };
                            ui.selectable_value(model, option.name.clone(), text);
                        }
                    }
                }

                ui.separator();
                refresh = ui.button(t!("provider.refresh")).clicked();
                *model != before
            })
            .inner
            .unwrap_or(false)
        });

        // Asked again next frame
        if refresh {
            self.models = None;
        }
        changed
    }

    fn fetch(&mut self, model: ModelConfig) {
        let client = self.client.clone();
        self.models = Some(Models {
            provider: *model.provider(),
            host: location(&model),
            list: MaybeDone::spawn(async move { provider::models(&model, &client).await }),
        });
    }
}

/// Whether the provider picked can't be used yet: Claude, without credentials.
pub fn needs_setup(config: &Config) -> bool {
    match config.get().model.provider() {
        ModelProvider::Claude => !provider::claude::has_credentials(&config.get().model.claude),
        ModelProvider::Ollama | ModelProvider::OpenAi => false,
    }
}

/// Where the provider's models are listed from: Ollama's host, or the API's URL. For Claude, the
/// credentials they were listed with.
fn location(model: &ModelConfig) -> Option<String> {
    match model.provider() {
        ModelProvider::Claude => {
            let claude = &model.claude;
            (claude.api_key().is_some() || claude.workspace_id().is_some()).then(|| {
                let api_key = claude.api_key().as_deref().unwrap_or_default();
                let workspace_id = claude.workspace_id().as_deref().unwrap_or_default();
                format!("{api_key}\n{workspace_id}")
            })
        }
        ModelProvider::Ollama => model.ollama.host().clone(),
        ModelProvider::OpenAi => model.openai.base_url().clone(),
    }
}

/// A line of text (Ollama's host, the API's URL, a key...): empty for its default, `password` to
/// hide what's typed. `typing` holds it while it's being typed. The new one once it's been typed
/// (Enter, or clicking away), if it changed.
fn text_field(
    typing: &mut Option<String>,
    ui: &mut egui::Ui,
    current: Option<String>,
    label: &str,
    hint: &str,
    hover: &str,
    password: bool,
) -> Option<Option<String>> {
    let mut text = match typing {
        Some(text) => text.clone(),
        None => current.clone().unwrap_or_default(),
    };

    let edit = ui
        .horizontal(|ui| {
            ui.label(label);
            ui.add(
                egui::TextEdit::singleline(&mut text)
                    .hint_text(hint)
                    .password(password)
                    .desired_width(200.0),
            )
        })
        .inner
        .on_hover_text(hover);

    if edit.has_focus() {
        *typing = Some(text);
        return None;
    }
    *typing = None;

    let text = Some(text.trim().to_owned()).filter(|text| !text.is_empty());
    (edit.lost_focus() && text != current).then_some(text)
}

/// A number of tokens, from 1 to `max`. Returns the field, changed if it did.
fn tokens(
    ui: &mut egui::Ui,
    prefix: &str,
    current: NonZeroU32,
    setting: &mut Setting<NonZeroU32>,
    max: u32,
) -> egui::Response {
    let mut response = None;
    setting.edit(current, |value| {
        let mut tokens = value.get();
        let field = ui.add(
            egui::DragValue::new(&mut tokens)
                .range(1..=max)
                .speed(64.0)
                .prefix(prefix),
        );
        let changed = field.changed() && tokens != value.get();
        if let Some(tokens) = NonZeroU32::new(tokens) {
            *value = tokens;
        }
        response = Some(field);
        changed
    });
    response.unwrap()
}

/// Bytes as Ollama shows them, e.g. `6.6 GB`.
fn size_text(bytes: u64) -> String {
    let bytes = bytes as f64;
    match bytes >= 1e9 {
        true => format!("{:.1} GB", bytes / 1e9),
        false => format!("{:.0} MB", bytes / 1e6),
    }
}
