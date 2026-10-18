//! Cookies, the account check and `ApplySettings`.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, PoisonError};

use super::runner::{Controller, Internal, base_options, current_client};
use crate::app::{self, NO_COOKIES_WARNING, Options};
use crate::config::Settings;
use crate::download::Downloader;
use crate::error::{Error, ErrorKind, Result};
use crate::gui::protocol::{AccountStatus, LogLevel, Update};
use crate::instagram::{Client, UserSummary};
use crate::redact;

/// A client built from the configured cookie file, and what to tell the user.
pub(super) struct ClientSetup {
    pub(super) client: Arc<Client>,
    pub(super) account: AccountStatus,
    /// A cookie jar was loaded.
    pub(super) has_jar: bool,
    /// Cookie warnings and errors for the log.
    pub(super) logs: Vec<(LogLevel, String)>,
}

/// Loads the cookie file (if any) and builds a client. A cookie file that cannot
/// be used gives an anonymous client and an `Invalid` account; only a bad client
/// configuration (API base, user agent) is an error.
pub(super) fn build_client(options: &Options) -> Result<ClientSetup> {
    let mut logs = Vec::new();
    let (jar, account) = match app::load_cookies(options.cookies.as_deref()) {
        Ok(None) => {
            logs.push((LogLevel::Warn, NO_COOKIES_WARNING.to_string()));
            (None, AccountStatus::NoCookies)
        }
        Ok(Some((jar, warnings))) => {
            for warning in warnings {
                logs.push((LogLevel::Warn, format!("cookies: {warning}")));
            }
            (Some(jar), AccountStatus::Loaded)
        }
        Err(err) => {
            let message = redact::redact(&err.to_string());
            logs.push((LogLevel::Error, message.clone()));
            (None, AccountStatus::Invalid(message))
        }
    };
    let has_jar = jar.is_some();
    let client = Client::new(options.client.clone(), jar)?;
    Ok(ClientSetup {
        client: Arc::new(client),
        account,
        has_jar,
        logs,
    })
}

impl Controller {
    /// Sends an account status.
    pub(super) fn set_account(&self, account: AccountStatus) {
        self.sink.send(Update::Account(account));
    }

    /// Replaces the client (grabs pick it up at their next link) and reports the
    /// new account status.
    pub(super) fn install_client(&mut self, setup: ClientSetup) {
        let ClientSetup {
            client,
            account,
            has_jar,
            logs,
        } = setup;
        *self.client.write().unwrap_or_else(PoisonError::into_inner) = client;
        self.has_jar = has_jar;
        self.client_generation += 1;
        self.set_account(account);
        for (level, text) in logs {
            self.sink.log(level, text);
        }
    }

    /// `CheckAccount`: asks Instagram who the cookies belong to.
    pub(super) fn check_account(&mut self) {
        if self.options.cookies.is_none() {
            self.set_account(AccountStatus::NoCookies);
            self.sink.warn(NO_COOKIES_WARNING);
            return;
        }
        if !self.has_jar {
            // The cookie file was unusable; it may have been replaced since.
            match build_client(&self.options) {
                Ok(setup) => self.install_client(setup),
                Err(err) => {
                    self.sink.error(format!("cannot check the account: {err}"));
                    return;
                }
            }
            if !self.has_jar {
                return;
            }
        }
        self.set_account(AccountStatus::Checking);
        let client = current_client(&self.client);
        let generation = self.client_generation;
        let results = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = client.current_user().await;
            let _ = results.send(Internal::AccountChecked { generation, result });
        });
    }

    /// The result of a `CheckAccount`; ignored if the client changed meanwhile.
    pub(super) fn account_checked(&mut self, generation: u64, result: Result<UserSummary>) {
        if generation != self.client_generation {
            return;
        }
        match result {
            Ok(user) => {
                let text = format!("logged in as @{}", user.username);
                self.set_account(AccountStatus::LoggedIn {
                    username: user.username,
                });
                self.sink.info(text);
            }
            Err(err) => {
                let message = redact::redact(&err.to_string());
                if matches!(err.kind(), ErrorKind::Auth | ErrorKind::Cookies) {
                    self.set_account(AccountStatus::Invalid(message.clone()));
                    self.sink.error(format!("account check failed: {message}"));
                } else {
                    self.set_account(AccountStatus::Unverified(message.clone()));
                    self.sink.warn(format!(
                        "could not verify the login ({message}); the cookies may still be fine — try grabbing a post"
                    ));
                }
            }
        }
    }

    /// `ApplySettings`: validate, save to `config.toml`, rebuild the client and
    /// (when idle) the downloader. Invalid settings are refused and the current
    /// ones re-sent so the form can reset.
    pub(super) fn apply_settings(&mut self, settings: Settings) {
        let prepared = settings.validate().and_then(|()| {
            let options = base_options(&self.global, &settings)?;
            let setup = build_client(&options)?;
            let downloader = Downloader::new(options.download.clone())?;
            Ok((options, setup, downloader))
        });
        let (mut options, setup, downloader) = match prepared {
            Ok(prepared) => prepared,
            Err(err) => {
                self.sink.error(format!("settings not applied: {err}"));
                self.send_settings();
                return;
            }
        };

        match &self.config_path {
            Some(path) => match write_settings(path, &settings) {
                Ok(()) => self
                    .sink
                    .info(format!("settings saved to {}", path.display())),
                Err(err) => self.sink.error(format!(
                    "cannot save the settings ({err}); they are used for this session only"
                )),
            },
            None => self.sink.warn("settings are used for this session only"),
        }
        if options.data_dir != self.options.data_dir {
            self.sink.warn(format!(
                "the data folder {} is used after igdl-gui restarts",
                options.data_dir.display()
            ));
            options.data_dir = self.options.data_dir.clone();
        }
        self.settings = settings;
        self.options = options;
        self.install_client(setup);
        if self.active_runs == 0 {
            self.downloader = Arc::new(downloader);
        } else {
            self.pending_downloader = Some(downloader);
            self.sink
                .info("the new download settings apply once the running downloads finish");
        }
        self.send_settings();
    }
}

/// Writes `settings` to `path` as TOML, atomically (temp file + rename).
fn write_settings(path: &Path, settings: &Settings) -> Result<()> {
    let text = toml::to_string_pretty(settings)
        .map_err(|err| Error::Config(format!("cannot serialise the settings: {err}")))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| Error::Config(format!("{} is not a file path", path.display())))?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|err| Error::io(format!("cannot create {}", parent.display()), err))?;
    }
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let written = std::fs::File::create(&tmp).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()
    });
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::io(format!("cannot write {}", tmp.display()), err));
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::io(format!("cannot replace {}", path.display()), err));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn settings_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new dir").join("config.toml");
        let settings = Settings {
            out_dir: Some(PathBuf::from("/downloads/insta gram")),
            jobs: Some(6),
            api_delay: Some(2.5),
            include: Some(vec!["posts".into(), "stories".into()]),
            set_mtime: Some(false),
            ..Settings::default()
        };
        write_settings(&path, &settings).unwrap();
        assert_eq!(crate::config::load(Some(&path)).unwrap(), settings);
        assert!(!path.with_file_name("config.toml.tmp").exists());

        write_settings(&path, &Settings::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "");
    }
}
