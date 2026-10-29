//! The language model the engineer talks through: who runs it, which model, and how.

use crate::{
    account::{Account, Status},
    app::topbar::{pick, setup_label, size_text},
    config::{
        ClaudeConfig, Config, ModelConfig, ModelProvider, OllamaConfig, OpenAiConfig, Setting,
    },
    provider::{self, ModelOption, bono_cloud::Entitlement},
    task::MaybeDone,
};
use bono_i18n::t;
use egui::IntoAtoms;
use std::{
    num::NonZeroU32,
    time::{Duration, Instant, SystemTime},
};
use tokio::runtime::Handle;

const CLAUDE_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const OLLAMA_EFFORTS: [&str; 4] = ["none", "low", "medium", "high"];
/// Empty: `reasoning_effort` left out, the model's default
const OPENAI_EFFORTS: [&str; 5] = ["", "none", "low", "medium", "high"];

/// Where Claude API keys are made
const CLAUDE_KEYS_URL: &str = "https://platform.claude.com/settings/keys";
/// Where Ollama is downloaded from
const OLLAMA_DOWNLOAD_URL: &str = "https://ollama.com/download";

/// Bono Cloud's allowance is asked again this long after a turn, at the soonest
const ALLOWANCE_DEBOUNCE: Duration = Duration::from_secs(30);
/// ...and this long after it was last asked, turns or not (others may use the account)
const ALLOWANCE_MAX_AGE: Duration = Duration::from_mins(5);
/// Below this many hours of driving left, the allowance is said to be running low
const ALLOWANCE_LOW_HOURS: f64 = 2.0;

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
    /// Who Bono Cloud is used as
    account: Option<Account>,
    /// Whether the signed-in account may use Bono Cloud
    cloud: Option<CloudAccess>,
    /// What's left of its Bono Cloud allowance
    allowance: Option<Allowance>,
    /// The provider's models, fetched when the tab first shows them
    models: Option<Models>,
    /// The address being typed (Ollama's host, or the API's URL): set once it's done (Enter, or
    /// clicking away)
    host: Option<String>,
    /// Claude's API key being typed, likewise
    api_key: Option<String>,
    /// The OpenAI-compatible API key being typed, likewise
    openai_api_key: Option<String>,
    /// Claude's workspace being typed, likewise
    workspace_id: Option<String>,
}

/// Whether one account may use Bono Cloud.
struct CloudAccess {
    /// The account's user id
    user: String,
    authorized: MaybeDone<anyhow::Result<bool>>,
}

/// What's left of one account's Bono Cloud allowance.
struct Allowance {
    /// The account's user id
    user: String,
    /// Bono Cloud's turns when it was asked
    turns: u64,
    asked: Instant,
    entitlement: MaybeDone<anyhow::Result<Entitlement>>,
    /// The last answer, shown while it's asked again
    last: Option<Entitlement>,
}

/// The models one provider offers.
struct Models {
    provider: ModelProvider,
    /// Where they were asked: Ollama's host, or the API's URL
    host: Option<String>,
    list: MaybeDone<anyhow::Result<Vec<ModelOption>>>,
}

impl ProviderTopbar {
    pub fn new(client: &reqwest::Client, account: Option<&Account>) -> Self {
        Self {
            client: client.clone(),
            account: account.cloned(),
            cloud: None,
            allowance: None,
            models: None,
            host: None,
            api_key: None,
            openai_api_key: None,
            workspace_id: None,
        }
    }

    pub fn update(&mut self, rt: &Handle, ui: &mut egui::Ui, config: &mut Config) {
        let model = config.get().model.clone();
        let provider = *model.provider();
        self.refetch(&model);
        let cloud = self.cloud_access(rt);

        config.maybe_update(|raw| {
            let mut changed = raw.model.edit_provider(provider, |provider| {
                ui.horizontal(|ui| {
                    ui.label(t!("provider.label"));
                    let mut cloud_option = ui
                        .selectable_value(
                            provider,
                            ModelProvider::BonoCloud,
                            setup_label(cloud == Some(false), "Bono Cloud"),
                        )
                        .on_hover_text(t!("provider.bono_cloud_hover"));
                    if cloud == Some(false) {
                        cloud_option = cloud_option.on_hover_text(self.cloud_unavailable());
                    }
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
                    cloud_option.clicked()
                        || claude.clicked()
                        || ollama.clicked()
                        || openai.clicked()
                })
                .inner
            });
            ui.separator();

            changed |= match provider {
                ModelProvider::BonoCloud => {
                    self.bono_cloud(rt, ui, cloud);
                    false
                }
                ModelProvider::Claude => self.claude(rt, ui, &mut raw.model.claude),
                ModelProvider::Ollama => self.ollama(rt, ui, &mut raw.model.ollama),
                ModelProvider::OpenAi => self.openai(rt, ui, &mut raw.model.openai),
            };
            changed
        });

        ui.separator();
        ui.weak(t!("provider.applies_next"));
    }

    /// The setup guide's take on the tab: the provider, what it takes to reach it, and the
    /// model, then whether it could be reached.
    pub fn guide(&mut self, rt: &Handle, ui: &mut egui::Ui, config: &mut Config) {
        let model = config.get().model.clone();
        let provider = *model.provider();
        self.refetch(&model);
        let cloud = self.cloud_access(rt);

        config.maybe_update(|raw| {
            let mut changed = raw.model.edit_provider(provider, |provider| {
                let no_credentials = !provider::claude::has_credentials(&model.claude);
                let mut cloud_option = ui.radio_value(
                    provider,
                    ModelProvider::BonoCloud,
                    setup_label(cloud == Some(false), "Bono Cloud"),
                );
                if cloud == Some(false) {
                    cloud_option = cloud_option.on_hover_text(self.cloud_unavailable());
                }
                let mut changed = cloud_option.changed();
                let about = t!("onboarding.bono_cloud");
                ui.indent(about, |ui| ui.weak(about));
                let options = [
                    (
                        ModelProvider::Claude,
                        setup_label(no_credentials, "Claude"),
                        t!("onboarding.claude"),
                    ),
                    (
                        ModelProvider::Ollama,
                        "Ollama".into_atoms(),
                        t!("onboarding.ollama"),
                    ),
                    (
                        ModelProvider::OpenAi,
                        "OpenAI".into_atoms(),
                        t!("onboarding.openai"),
                    ),
                ];
                for (option, label, about) in options {
                    changed |= ui.radio_value(provider, option, label).changed();
                    ui.indent(about, |ui| ui.weak(about));
                }
                changed
            });
            ui.add_space(8.0);

            changed |= match provider {
                ModelProvider::BonoCloud => {
                    self.bono_cloud(rt, ui, cloud);
                    false
                }
                ModelProvider::Claude => {
                    let claude = &mut raw.model.claude;
                    let mut changed = self.api_key(ui, claude);
                    ui.hyperlink_to(t!("onboarding.claude_key_link"), CLAUDE_KEYS_URL);
                    changed |= self.model(rt, ui, claude.model().clone(), &mut claude.model);
                    changed
                }
                ModelProvider::Ollama => {
                    let ollama = &mut raw.model.ollama;
                    let mut changed = self.host(ui, ollama);
                    ui.hyperlink_to(t!("onboarding.ollama_link"), OLLAMA_DOWNLOAD_URL);
                    changed |= self.model(rt, ui, ollama.model().clone(), &mut ollama.model);
                    changed
                }
                ModelProvider::OpenAi => {
                    let openai = &mut raw.model.openai;
                    let mut changed = self.base_url(ui, openai);
                    changed |= self.openai_api_key(ui, openai);
                    changed |= self.model(rt, ui, openai.model().clone(), &mut openai.model);
                    changed
                }
            };
            changed
        });

        ui.add_space(4.0);
        self.status(rt, ui, &model);
    }

    /// Whether the provider could be reached, and offers the model picked.
    fn status(&mut self, rt: &Handle, ui: &mut egui::Ui, model: &ModelConfig) {
        let picked = match model.provider() {
            // Whichever the website picks
            ModelProvider::BonoCloud => "",
            ModelProvider::Claude => model.claude.model(),
            ModelProvider::Ollama => model.ollama.model(),
            ModelProvider::OpenAi => model.openai.model(),
        };
        let Some(models) = self.models.as_mut() else {
            return;
        };

        let warn = ui.visuals().warn_fg_color;
        let mut check = false;
        ui.horizontal_wrapped(|ui| match models.list.update(rt) {
            None => {
                ui.spinner();
                ui.label(t!("onboarding.provider_checking"));
            }
            // Just what went wrong: why, on hover
            Some(Err(e)) => {
                ui.colored_label(warn, format!("{} {e}", egui_phosphor::bold::WARNING))
                    .on_hover_text(format!("{e:#}"));
                check = ui.button(t!("onboarding.provider_check")).clicked();
            }
            // One serving a single model takes any, but the rest need one they have
            Some(Ok(list)) if !picked.is_empty() && !list.iter().any(|o| o.name == *picked) => {
                let text = match model.provider() {
                    ModelProvider::Ollama => t!("onboarding.ollama_no_model", model = picked),
                    _ => t!("onboarding.provider_no_model", model = picked),
                };
                ui.colored_label(warn, format!("{} {text}", egui_phosphor::bold::WARNING));
                check = ui.button(t!("onboarding.provider_check")).clicked();
            }
            Some(Ok(_)) => {
                ui.label(format!(
                    "{} {}",
                    egui_phosphor::bold::CHECK,
                    t!("onboarding.provider_ready")
                ));
            }
        });

        // Asked again next frame
        if check {
            self.models = None;
            self.cloud = None;
        }
    }

    fn claude(&mut self, rt: &Handle, ui: &mut egui::Ui, config: &mut ClaudeConfig) -> bool {
        let mut changed = self.api_key(ui, config);
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
            t!("provider.cache_ttl"),
            *config.cache_ttl(),
            &mut config.cache_ttl,
            [
                crate::config::ClaudeCacheTtl::FiveMinutes,
                crate::config::ClaudeCacheTtl::OneHour,
            ]
            .map(|ttl| (ttl, ttl.as_str().to_owned())),
            |ttl| ttl.as_str().to_owned(),
        )
        .changed();
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
        let mut changed = self.base_url(ui, config);
        changed |= self.openai_api_key(ui, config);
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
        let mut changed = self.host(ui, config);
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

    /// Claude's API key.
    fn api_key(&mut self, ui: &mut egui::Ui, config: &mut ClaudeConfig) -> bool {
        let Some(api_key) = text_field(
            &mut self.api_key,
            ui,
            config.api_key().clone(),
            t!("provider.api_key"),
            t!("provider.api_key_hint"),
            t!("provider.api_key_hover"),
            true,
        ) else {
            return false;
        };
        config.api_key.set(api_key);
        true
    }

    /// The OpenAI-compatible API's key.
    fn openai_api_key(&mut self, ui: &mut egui::Ui, config: &mut OpenAiConfig) -> bool {
        let Some(api_key) = text_field(
            &mut self.openai_api_key,
            ui,
            config.api_key().clone(),
            t!("provider.api_key"),
            "OPENAI_API_KEY",
            t!("provider.openai_api_key_hover"),
            true,
        ) else {
            return false;
        };
        config.api_key.set(api_key);
        true
    }

    /// Where Ollama listens.
    fn host(&mut self, ui: &mut egui::Ui, config: &mut OllamaConfig) -> bool {
        let Some(host) = text_field(
            &mut self.host,
            ui,
            config.host().clone(),
            t!("provider.host"),
            t!("provider.host_hint"),
            t!("provider.host_hover"),
            false,
        ) else {
            return false;
        };
        config.host.set(host);
        true
    }

    /// The OpenAI-compatible API's URL.
    fn base_url(&mut self, ui: &mut egui::Ui, config: &mut OpenAiConfig) -> bool {
        let Some(base_url) = text_field(
            &mut self.host,
            ui,
            config.base_url().clone(),
            t!("provider.base_url"),
            provider::OPENAI_DEFAULT_BASE_URL,
            t!("provider.base_url_hover"),
            false,
        ) else {
            return false;
        };
        config.base_url.set(base_url);
        true
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
            self.cloud = None;
        }
        changed
    }

    /// Asks for the provider's models again if it's another provider, or another address.
    fn refetch(&mut self, model: &ModelConfig) {
        let host = location(model, self.account.as_ref());
        let stale = self
            .models
            .as_ref()
            .is_none_or(|models| models.provider != *model.provider() || models.host != host);
        if stale {
            self.fetch(model.clone());
        }
    }

    fn fetch(&mut self, model: ModelConfig) {
        let client = self.client.clone();
        let account = self.account.clone();
        self.models = Some(Models {
            provider: *model.provider(),
            host: location(&model, account.as_ref()),
            list: MaybeDone::spawn(async move {
                provider::models(&model, account.as_ref(), &client).await
            }),
        });
    }

    /// Whether the account may use Bono Cloud: `None` while it's being asked. Asked again for
    /// another account.
    fn cloud_access(&mut self, rt: &Handle) -> Option<bool> {
        let user = match self.account.as_ref().map(Account::status) {
            Some(Status::SignedIn(user)) => user,
            Some(Status::SignedOut) | None => {
                self.cloud = None;
                return Some(false);
            }
        };

        if self
            .cloud
            .as_ref()
            .is_none_or(|cloud| cloud.user != user.id)
        {
            let account = self.account.clone().unwrap();
            let client = self.client.clone();
            self.cloud = Some(CloudAccess {
                user: user.id,
                authorized: MaybeDone::spawn(async move {
                    let authorized = provider::bono_cloud::authorized(&account, &client).await;
                    // Once per check, not every frame it's looked at
                    if let Err(e) = &authorized {
                        log::debug!("Couldn't tell whether Bono Cloud may be used: {e:#}");
                    }
                    authorized
                }),
            });
        }

        let authorized = self.cloud.as_mut()?.authorized.update(rt)?;
        Some(matches!(authorized, Ok(true)))
    }

    /// Why Bono Cloud can't be used: signed out, or no access.
    fn cloud_unavailable(&self) -> &'static str {
        match self.cloud {
            None => t!("provider.bono_cloud_sign_in"),
            Some(_) => t!("provider.bono_cloud_unavailable"),
        }
    }

    /// Bono Cloud's tab: nothing to set, just whether it may be used, and why not. Asking again
    /// is offered whenever the answer may change without signing in again: access granted
    /// since, or the website reachable again.
    fn bono_cloud(&mut self, rt: &Handle, ui: &mut egui::Ui, cloud: Option<bool>) {
        let warn = ui.visuals().warn_fg_color;
        let mut check = false;
        match cloud {
            Some(true) => {
                ui.label(t!("provider.bono_cloud_about"));
                self.show_allowance(rt, ui);
            }
            Some(false) => {
                // Signed out, signing in asks again
                let signed_in = self.cloud.is_some();
                let unavailable = self.cloud_unavailable();
                let failed = self
                    .cloud
                    .as_mut()
                    .and_then(|cloud| cloud.authorized.update(rt))
                    .and_then(|authorized| authorized.as_ref().err())
                    .map(|e| (e.to_string(), format!("{e:#}")));
                ui.horizontal_wrapped(|ui| {
                    // Just what went wrong: why, on hover
                    match failed {
                        Some((error, why)) => {
                            ui.colored_label(
                                warn,
                                format!("{} {error}", egui_phosphor::bold::WARNING),
                            )
                            .on_hover_text(why);
                        }
                        None => {
                            ui.colored_label(
                                warn,
                                format!("{} {}", egui_phosphor::bold::WARNING, unavailable),
                            );
                        }
                    }
                    if signed_in {
                        check = ui.button(t!("onboarding.provider_check")).clicked();
                    }
                });
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t!("onboarding.provider_checking"));
                });
            }
        }

        // Asked again next frame
        if check {
            self.cloud = None;
            self.models = None;
        }
    }

    /// What's left of the account's allowance this week, warning when it runs low.
    fn show_allowance(&mut self, rt: &Handle, ui: &mut egui::Ui) {
        let Some(entitlement) = self.allowance(rt) else {
            return;
        };
        let Some(hours) = entitlement.estimated_hours_left else {
            return;
        };
        let resets = (entitlement.week.as_ref())
            .and_then(|week| week.resets_at.as_deref())
            .and_then(resets_in)
            .unwrap_or_else(|| "?".into());
        let hours_left = format!("{:.1}", hours.max(0.0));
        if hours < ALLOWANCE_LOW_HOURS {
            let text = t!(
                "provider.bono_cloud_allowance_low",
                hours = hours_left,
                resets = resets
            );
            let warn = ui.visuals().warn_fg_color;
            ui.colored_label(warn, format!("{} {text}", egui_phosphor::bold::WARNING));
        } else {
            ui.weak(t!(
                "provider.bono_cloud_hours_left",
                hours = hours_left,
                resets = resets
            ));
        }
    }

    /// What's left of the signed-in account's Bono Cloud allowance, as last asked: asked again
    /// a while after each turn, and every few minutes. `None` until it's known.
    fn allowance(&mut self, rt: &Handle) -> Option<&Entitlement> {
        let user = match self.account.as_ref().map(Account::status) {
            Some(Status::SignedIn(user)) => user,
            Some(Status::SignedOut) | None => {
                self.allowance = None;
                return None;
            }
        };

        let turns = provider::bono_cloud::turns();
        let stale = self.allowance.as_ref().is_none_or(|allowance| {
            let age = allowance.asked.elapsed();
            allowance.user != user.id
                || age >= ALLOWANCE_MAX_AGE
                || (allowance.turns != turns && age >= ALLOWANCE_DEBOUNCE)
        });
        if stale {
            let last = self
                .allowance
                .take()
                .filter(|allowance| allowance.user == user.id)
                .and_then(|allowance| allowance.last);
            let account = self.account.clone();
            let client = self.client.clone();
            self.allowance = Some(Allowance {
                user: user.id,
                turns,
                asked: Instant::now(),
                entitlement: MaybeDone::spawn(async move {
                    let entitlement =
                        provider::bono_cloud::entitlement(account.as_ref(), &client).await;
                    // Once per check, not every frame it's looked at
                    if let Err(e) = &entitlement {
                        log::debug!("Couldn't tell what's left of Bono Cloud's allowance: {e:#}");
                    }
                    entitlement
                }),
                last,
            });
        }

        let allowance = self.allowance.as_mut()?;
        if let Some(Ok(entitlement)) = allowance.entitlement.update(rt) {
            allowance.last = Some(entitlement.clone());
        }
        allowance.last.as_ref()
    }

    /// Whether the provider picked can't be used yet: Claude, without credentials, or Bono Cloud,
    /// without an account that may use it.
    pub fn needs_setup(&mut self, rt: &Handle, config: &Config) -> bool {
        match config.get().model.provider() {
            ModelProvider::BonoCloud => self.cloud_access(rt) == Some(false),
            ModelProvider::Claude => !provider::claude::has_credentials(&config.get().model.claude),
            ModelProvider::Ollama | ModelProvider::OpenAi => false,
        }
    }
}

/// How long until `at` (RFC 3339), roughly, as the pilot reads it.
fn resets_in(at: &str) -> Option<String> {
    let at = humantime::parse_rfc3339_weak(at).ok()?;
    let left = at.duration_since(SystemTime::now()).unwrap_or_default();
    let hours = left.as_secs().div_ceil(3600);
    Some(match hours / 24 {
        0 => t!("provider.bono_cloud_resets_hours", hours = hours),
        days => t!(
            "provider.bono_cloud_resets_days",
            days = days,
            hours = hours % 24
        ),
    })
}

/// Where the provider's models are listed from: Ollama's host, or the API's URL. For Claude, the
/// credentials they were listed with, and for Bono Cloud, the account.
fn location(model: &ModelConfig, account: Option<&Account>) -> Option<String> {
    match model.provider() {
        ModelProvider::BonoCloud => match account.map(Account::status) {
            Some(Status::SignedIn(user)) => Some(user.id),
            Some(Status::SignedOut) | None => None,
        },
        ModelProvider::Claude => {
            let claude = &model.claude;
            (claude.api_key().is_some() || claude.workspace_id().is_some()).then(|| {
                let api_key = claude.api_key().as_deref().unwrap_or_default();
                let workspace_id = claude.workspace_id().as_deref().unwrap_or_default();
                format!("{api_key}\n{workspace_id}")
            })
        }
        ModelProvider::Ollama => model.ollama.host().clone(),
        ModelProvider::OpenAi => {
            let openai = &model.openai;
            (openai.base_url().is_some() || openai.api_key().is_some()).then(|| {
                let base_url = openai.base_url().as_deref().unwrap_or_default();
                let api_key = openai.api_key().as_deref().unwrap_or_default();
                format!("{base_url}\n{api_key}")
            })
        }
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
