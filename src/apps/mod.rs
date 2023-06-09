use axum::{
    extract::{ConnectInfo, Host, Path, State},
    http::{
        uri::{Authority, Scheme},
        Request, Response,
    },
    response::IntoResponse,
    Json,
};
use axum_extra::extract::cookie::{Cookie, SameSite};
use base64ct::Encoding;
use headers::HeaderValue;
use http::header::{AUTHORIZATION, SET_COOKIE};
use hyper::{header::LOCATION, Body, StatusCode, Uri};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

use crate::{
    appstate::{Client, ConfigFile, ConfigState},
    configuration::{config_or_error, HostType},
    users::{check_authorization, AdminToken, UserTokenWithoutXSRFCheck},
    utils::{is_default, option_vec_trim_remove_empties, string_trim, vec_trim_remove_empties},
};

pub static AUTHENTICATED_USER_MAIL_HEADER: &str = "Remote-User";

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    pub id: usize,
    #[serde(deserialize_with = "string_trim")]
    pub name: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub icon: String,
    pub color: usize,
    #[serde(default, skip_serializing_if = "is_default")]
    pub is_proxy: bool,
    #[serde(deserialize_with = "string_trim")]
    pub host: String,
    #[serde(deserialize_with = "string_trim")]
    pub target: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub secured: bool,
    #[serde(
        default,
        skip_serializing_if = "is_default",
        deserialize_with = "string_trim"
    )]
    pub login: String,
    #[serde(
        default,
        skip_serializing_if = "is_default",
        deserialize_with = "string_trim"
    )]
    pub password: String,
    #[serde(
        default,
        skip_serializing_if = "is_default",
        deserialize_with = "string_trim"
    )]
    pub openpath: String,
    #[serde(
        default,
        skip_serializing_if = "is_default",
        deserialize_with = "vec_trim_remove_empties"
    )]
    pub roles: Vec<String>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub inject_security_headers: bool,
    #[serde(
        default,
        skip_serializing_if = "is_default",
        deserialize_with = "option_vec_trim_remove_empties"
    )]
    pub subdomains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub forward_user_mail: bool,
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub struct AppWithUri {
    pub inner: App,
    pub app_scheme: Scheme,
    pub app_authority: Authority,
    pub forward_scheme: Scheme,
    pub forward_authority: Authority,
}

impl AppWithUri {
    pub fn from_app_domain_and_http_port(inner: App, domain: &str, port: Option<u16>) -> Self {
        let app_scheme = if port.is_some() {
            Scheme::HTTP
        } else {
            Scheme::HTTPS
        };
        let mut app_authority = if inner.host.contains(domain) {
            inner.host.clone()
        } else {
            format!("{}.{}", inner.host, domain)
        };
        if let Some(port) = port {
            app_authority.push_str(&format!(":{}", port));
        }
        let app_authority = app_authority
            .parse()
            .expect("could not work out authority from app configuration");
        let forward_scheme = if inner.target.starts_with("https://") {
            Scheme::HTTPS
        } else {
            Scheme::HTTP
        };
        let forward_base_uri: Uri = inner
            .target
            .parse()
            .expect("could not parse app target service");
        let forward_parts = forward_base_uri.into_parts();
        let forward_authority = forward_parts
            .authority
            .expect("could not parse app target service host");

        Self {
            inner,
            app_scheme,
            app_authority,
            forward_scheme,
            forward_authority,
        }
    }
}

pub async fn proxy_handler(
    user: Option<UserTokenWithoutXSRFCheck>,
    ConnectInfo(_): ConnectInfo<SocketAddr>,
    app: HostType,
    Host(hostname): Host,
    State(config): State<ConfigState>,
    State(_): State<Client>,
    mut req: Request<Body>,
) -> Result<Response<Body>, ()> {
    let domain = hostname.split(':').next().unwrap_or_default();
    if let Some(mut value) =
        check_authorization(&app, &user.as_ref().map(|u| &u.0), domain, req.uri().path())
    {
        // Redirect to login page if user is not logged, write where to get back after login in a cookie
        if value.status() == StatusCode::UNAUTHORIZED {
            if let Ok(hn) = HeaderValue::from_str(&config.full_domain()) {
                *value.status_mut() = StatusCode::FOUND;
                value.headers_mut().append(LOCATION, hn);
                let cookie = Cookie::build(
                    "ATRIUM_REDIRECT",
                    format!("{}://{hostname}", config.scheme()),
                )
                .domain(config.domain.clone())
                .path("/")
                .same_site(SameSite::Lax)
                .secure(false)
                .max_age(time::Duration::seconds(60))
                .http_only(false)
                .finish();
                value.headers_mut().append(
                    SET_COOKIE,
                    HeaderValue::from_str(&format!("{cookie}")).unwrap(),
                );
            }
        }
        return Ok(value);
    }

    let app = match app {
        HostType::ReverseApp(app) => app,
        _ => panic!("Service is not an app !"),
    };

    // If the target service contains a port, it is an internal service, inform the app that we are proxying to it
    if app.forward_authority.port().is_some() {
        req.headers_mut().insert(
            "X-Forwarded-Host",
            HeaderValue::from_str(app.app_authority.as_ref()).unwrap(),
        );
        req.headers_mut().insert(
            "X-Forwarded-Proto",
            HeaderValue::from_str(app.app_scheme.as_ref()).unwrap(),
        );
    }

    // If the app contains basic auth information, forge a basic auth header
    if !app.inner.login.is_empty() && !app.inner.password.is_empty() {
        let bauth = format!("{}:{}", app.inner.login, app.inner.password);
        req.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!(
                "Basic {}",
                base64ct::Base64::encode_string(bauth.as_bytes())
            ))
            .unwrap(),
        );
    }

    let response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("Hello World!"))
        .unwrap();

    Ok(response)
}

pub async fn get_apps(
    State(config_file): State<ConfigFile>,
    _admin: AdminToken,
) -> Result<Json<Vec<App>>, (StatusCode, &'static str)> {
    let config = config_or_error(&config_file).await?;
    // Return all the apps as Json
    Ok(Json(config.apps))
}

pub async fn delete_app(
    State(config_file): State<ConfigFile>,
    _admin: AdminToken,
    Path(app_id): Path<usize>,
) -> Result<impl IntoResponse, impl IntoResponse> {
    let mut config = config_or_error(&config_file).await?;
    // Find the app
    if let Some(pos) = config.apps.iter().position(|a| a.id == app_id) {
        // It is an existing app, delete it
        config.apps.remove(pos);
    } else {
        // If the app doesn't exist, respond with an error
        return Err((StatusCode::BAD_REQUEST, "app doesn't exist"));
    }

    config
        .to_file_or_internal_server_error(&config_file)
        .await?;

    Ok((StatusCode::OK, "app deleted successfully"))
}

pub async fn add_app(
    State(config_file): State<ConfigFile>,
    State(config): State<ConfigState>,
    _admin: AdminToken,
    Json(payload): Json<App>,
) -> Result<(StatusCode, &'static str), (StatusCode, &'static str)> {
    // Clone the config
    let mut config = (*config).clone();
    // Find the app
    if let Some(app) = config.apps.iter_mut().find(|a| a.id == payload.id) {
        *app = payload;
    } else {
        config.apps.push(payload);
    }

    config
        .to_file_or_internal_server_error(&config_file)
        .await?;

    Ok((StatusCode::CREATED, "app created or updated successfully"))
}
