use crate::{
    apps::{App, AppWithUri},
    appstate::{ConfigMap, ConfigState},
    users::User,
    utils::{is_default, option_string_trim, string_trim},
};
use anyhow::Result;
use axum::{
    async_trait,
    extract::{FromRef, FromRequestParts},
};
use http::request::Parts;
use hyper::StatusCode;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

fn hostname() -> String {
    "atrium.io".to_owned()
}

#[derive(Deserialize, Serialize, Debug, Default, PartialEq, Eq, Clone)]
pub struct OnlyOfficeConfig {
    #[serde(default, skip_serializing_if = "is_default")]
    pub title: Option<String>,
    pub server: String,
    pub jwt_secret: String,
}

#[derive(Deserialize, Serialize, Debug, Default, PartialEq, Eq, Clone)]
pub struct OpenIdConfig {
    pub client_id: String,
    pub client_secret: String,
    pub auth_url: String,
    pub token_url: String,
    pub userinfo_url: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub admins_group: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Default, PartialEq, Eq, Clone)]
pub enum TlsMode {
    #[default]
    No,
    BehindProxy,
    Auto,
}

impl TlsMode {
    pub fn is_secure(&self) -> bool {
        match self {
            TlsMode::No => false,
            TlsMode::BehindProxy => true,
            TlsMode::Auto => true,
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Default, PartialEq, Eq, Clone)]
pub struct Config {
    #[serde(default = "hostname", deserialize_with = "string_trim")]
    pub hostname: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub domain: String,
    #[serde(default)]
    pub tls_mode: TlsMode,
    #[serde(
        default,
        skip_serializing_if = "is_default",
        deserialize_with = "string_trim"
    )]
    pub letsencrypt_email: String,
    #[serde(
        default,
        skip_serializing_if = "is_default",
        deserialize_with = "option_string_trim"
    )]
    pub cookie_key: Option<String>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub log_to_file: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub session_duration_days: Option<i64>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub apps: Vec<App>,

    #[serde(default, skip_serializing_if = "is_default")]
    pub users: Vec<User>,
}

impl Config {
    pub async fn from_file(filepath: &str) -> Result<Self> {
        let data = tokio::fs::read_to_string(filepath).await.unwrap();
        let config = serde_yaml::from_str::<Config>(&data).unwrap();
        Ok(config)
    }

    pub async fn to_file(&self, filepath: &str) -> Result<()> {
        let contents = serde_yaml::to_string::<Config>(self).unwrap();
        tokio::fs::write(filepath, contents).await.unwrap();
        Ok(())
    }

    pub async fn to_file_or_internal_server_error(
        mut self,
        filepath: &str,
    ) -> Result<(), (StatusCode, &'static str)> {
        self.apps.sort_by(|a, b| a.id.partial_cmp(&b.id).unwrap());
        self.to_file(filepath).await.map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not save configuration",
            )
        })?;
        Ok(())
    }

    pub fn scheme(&self) -> &str {
        if self.tls_mode.is_secure() {
            "https"
        } else {
            "http"
        }
    }

    pub fn full_domain(&self) -> String {
        format!(
            "{s}://{h}{p}",
            s = self.scheme(),
            h = self.domain,
            p = &(if self.tls_mode == TlsMode::No {
                format!(":{}", 8080)
            } else {
                "".to_owned()
            })
        )
    }

    pub fn domains(&self) -> Vec<String> {
        let mut domains = filter_services(&self.apps, &self.hostname, &self.domain)
            .map(|app| format!("{}.{}", trim_host(&app.host), self.hostname))
            .collect::<Vec<String>>();
        domains.insert(0, self.hostname.to_owned());
        // Insert apps subdomains
        for app in filter_services(&self.apps, &self.hostname, &self.domain) {
            for domain in app.subdomains.as_ref().unwrap_or(&Vec::new()) {
                domains.push(format!(
                    "{}.{}.{}",
                    domain,
                    trim_host(&app.host),
                    self.hostname
                ));
            }
        }
        domains
    }
}

pub async fn load_config(config_file: &str) -> Result<(ConfigState, ConfigMap), anyhow::Error> {
    let mut config = Config::from_file(config_file).await?;
    // if the cookie encryption key is not present, generate it and store it
    if config.cookie_key.is_none() {
        config.cookie_key = Some(crate::utils::random_string(64));
        config.to_file(config_file).await?;
    }
    // Allow overriding the hostname with env variable
    if let Ok(h) = std::env::var("MAIN_HOSTNAME") {
        config.hostname = h
    }
    if is_default(&config.domain) {
        config.domain = config.hostname.clone()
    };
    let port = if config.tls_mode.is_secure() {
        None
    } else {
        Some(8080)
    };
    let mut hashmap: HashMap<String, HostType> =
        filter_services(&config.apps, &config.hostname, &config.domain)
            .map(|app| {
                (
                    format!("{}.{}", trim_host(&app.host), config.hostname),
                    app_to_host_type(app, &config, port),
                )
            })
            .collect();
    // Insert apps subdomains
    for app in filter_services(&config.apps, &config.hostname, &config.domain) {
        for domain in app.subdomains.as_ref().unwrap_or(&Vec::new()) {
            hashmap.insert(
                format!("{}.{}.{}", domain, trim_host(&app.host), config.hostname),
                app_to_host_type(app, &config, port),
            );
        }
    }
    Ok((Arc::new(config), Arc::new(hashmap)))
}

pub(crate) fn trim_host(host: &str) -> String {
    host.split_once('.').unwrap_or((host, "")).0.to_owned()
}

fn app_to_host_type(app: &App, config: &Config, port: Option<u16>) -> HostType {
    if app.is_proxy {
        HostType::ReverseApp(Box::new(AppWithUri::from_app_domain_and_http_port(
            app.clone(),
            &config.hostname,
            port,
        )))
    } else {
        HostType::StaticApp(app.clone())
    }
}

pub async fn config_or_error(config_file: &str) -> Result<Config, (StatusCode, &'static str)> {
    let config = Config::from_file(config_file).await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not read config file",
        )
    })?;
    Ok(config)
}

pub trait Service {
    fn host(&self) -> &str;
}

impl Service for App {
    fn host(&self) -> &str {
        &self.host
    }
}

fn filter_services<'a, T: Service + 'a>(
    services: &'a [T],
    hostname: &'a str,
    domain: &'a str,
) -> impl Iterator<Item = &'a T> {
    services.iter().filter(move |s| {
        if hostname == domain {
            // If domain == hostname, we keep all the apps that do not contain another hostname
            !s.host().contains(hostname)
        } else {
            // else we keep only the apps that DO contain another hostname (a subdomain)
            s.host().contains(hostname)
        }
    })
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub enum HostType {
    StaticApp(App),
    ReverseApp(Box<AppWithUri>),
}

impl HostType {
    pub fn host(&self) -> &str {
        match self {
            HostType::ReverseApp(app) => &app.inner.host,
            HostType::StaticApp(app) => &app.host,
        }
    }

    pub fn roles(&self) -> &Vec<String> {
        match self {
            HostType::ReverseApp(app) => &app.inner.roles,
            HostType::StaticApp(app) => &app.roles,
        }
    }

    pub fn secured(&self) -> bool {
        match self {
            HostType::ReverseApp(app) => app.inner.secured,

            HostType::StaticApp(app) => app.secured,
        }
    }

    pub fn inject_security_headers(&self) -> bool {
        match self {
            HostType::ReverseApp(app) => app.inner.inject_security_headers,

            HostType::StaticApp(app) => app.inject_security_headers,
        }
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for HostType
where
    S: Send + Sync,
    ConfigMap: FromRef<S>,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let configmap = ConfigMap::from_ref(state);

        let host = axum::extract::Host::from_request_parts(parts, state)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;

        let hostname = host.0.split_once(':').unwrap_or((&host.0, "")).0;

        // Work out where to target to
        let target = configmap
            .get(hostname)
            .ok_or(())
            .map_err(|_| StatusCode::NOT_FOUND)?;
        let target = (*target).clone();

        Ok(target)
    }
}
