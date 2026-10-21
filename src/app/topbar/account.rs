use crate::{
    account::{Account, Status},
    task::Task,
};
use bono_i18n::t;
use tokio::runtime::Handle;

#[derive(Default)]
pub struct AccountTopbar {
    operation: Option<Task<anyhow::Result<()>>>,
    logging_in: bool,
    error: Option<String>,
}

impl AccountTopbar {
    /// Poll even while the menu is closed, so reopening it always shows the latest result.
    pub fn poll(&mut self, rt: &Handle) {
        if let Some(result) = self.operation.as_mut().and_then(|task| task.update(rt)) {
            self.operation = None;
            self.error = result.err().map(|error| format!("{error:#}"));
        }
    }

    pub fn update(&mut self, ui: &mut egui::Ui, account: Option<&Account>) {
        ui.set_max_width(320.0);
        let Some(account) = account else {
            ui.label(t!("account.unavailable"));
            return;
        };

        match account.status() {
            Status::SignedIn(user) => {
                ui.strong(&user.name);
                ui.label(&user.email);
            }
            Status::SignedOut => {
                ui.label(t!("account.signed_out"));
            }
        }
        ui.separator();

        if self.operation.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if self.logging_in {
                    t!("account.waiting")
                } else {
                    t!("account.signing_out")
                });
            });
            if self.logging_in && ui.button(t!("cancel")).clicked() {
                // Task aborts its future on drop, stopping browser-approval polling.
                self.operation = None;
            }
        } else {
            let signed_in = matches!(account.status(), Status::SignedIn(_));
            let label = if signed_in {
                t!("account.sign_out")
            } else {
                t!("account.log_in")
            };
            if ui.button(label).clicked() {
                self.error = None;
                self.logging_in = !signed_in;
                let account = account.clone();
                self.operation = Some(Task::spawn(async move {
                    if signed_in {
                        account.logout().await
                    } else {
                        account.login().await.map(|_| ())
                    }
                }));
            }
        }

        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }
}
