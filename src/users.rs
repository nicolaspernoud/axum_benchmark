use crate::{
    appstate::{ConfigFile, ConfigState},
    configuration::{config_or_error, Config, HostType},
    headers::XSRFToken,
    utils::{is_default, random_string, raw_query_pairs, string_trim, vec_trim_remove_empties},
};

use axum::{
    async_trait,
    extract::{ConnectInfo, FromRef, FromRequestParts, Host, Path, RawQuery, State},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension, Json, RequestPartsExt, TypedHeader,
};
use axum_extra::extract::cookie::{Cookie, Key, PrivateCookieJar};
use headers::{authorization::Basic, Authorization, HeaderName};
use http::{header::CONTENT_LENGTH, request::Parts, HeaderValue, Request, StatusCode};
use hyper::Body;

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use time::{Duration, OffsetDateTime};

pub static AUTH_COOKIE: &str = "ATRIUM_AUTH";
static SHARE_TOKEN: &str = "SHARE_TOKEN";
static WWWAUTHENTICATE: HeaderName = HeaderName::from_static("www-authenticate");
pub static ADMINS_ROLE: &str = "ADMINS";
pub static REDACTED: &str = "REDACTED";

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfo {
    #[serde(
        skip_serializing_if = "is_default",
        default,
        deserialize_with = "string_trim"
    )]
    pub firstname: String,
    #[serde(
        skip_serializing_if = "is_default",
        default,
        deserialize_with = "string_trim"
    )]
    pub lastname: String,
    #[serde(
        skip_serializing_if = "is_default",
        default,
        deserialize_with = "string_trim"
    )]
    pub email: String,
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    #[serde(deserialize_with = "string_trim")]
    pub login: String,
    #[serde(
        skip_serializing_if = "is_default",
        default,
        deserialize_with = "string_trim"
    )]
    pub password: String,
    #[serde(
        default,
        skip_serializing_if = "is_default",
        deserialize_with = "vec_trim_remove_empties"
    )]
    pub roles: Vec<String>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub info: Option<UserInfo>,
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub hostname: String,
    pub path: String,
    pub share_with: Option<String>,
    pub share_for_days: Option<i64>,
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserToken {
    pub login: String,
    pub roles: Vec<String>,
    pub xsrf_token: String,
    pub share: Option<Share>,
    pub expires: i64,
    pub info: Option<UserInfo>,
}

impl UserToken {
    fn from_json(serialized_user_token: &str) -> Result<Self, (StatusCode, &'static str)> {
        let user_token = serde_json::from_str::<Self>(serialized_user_token).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not deserialize user token",
            )
        })?;
        user_token.check_expires()
    }

    fn check_expires(self) -> Result<Self, (StatusCode, &'static str)> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        if now > self.expires {
            Err((StatusCode::FORBIDDEN, "user token is expired"))
        } else {
            Ok(self)
        }
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for UserToken
where
    S: Send + Sync,
    Key: FromRef<S>,
    ConfigState: FromRef<S>,
{
    type Rejection = (StatusCode, &'static str);
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let jar = PrivateCookieJar::from_request_parts(parts, state)
            .await
            .expect("Could not find cookie jar");

        // Get the serialized user_token from the cookie jar, and check the xsrf token
        if let Some(cookie) = jar.get(AUTH_COOKIE) {
            if let Ok(TypedHeader(XSRFToken(xsrf_token))) =
                TypedHeader::<XSRFToken>::from_request_parts(parts, state).await
            {
                // Deserialize the user_token and return him/her
                let serialized_user_token = cookie.value();
                let user_token = UserToken::from_json(serialized_user_token)?;

                if user_token.xsrf_token != xsrf_token {
                    return Err((StatusCode::FORBIDDEN, "xsrf token doesn't match"));
                }
                return Ok(user_token);
            }
        }

        // OR Try to get user_token from the query
        if let Ok(query) = RawQuery::from_request_parts(parts, state).await {
            if let Some(Some(password)) = raw_query_pairs(query.0.as_deref())
                .ok()
                .map(|hm| hm.get("token").map(|v| v.to_owned()))
            {
                let res = cookie_from_password(AUTH_COOKIE, &jar, password);
                if res.is_ok() {
                    return res;
                } else {
                    return cookie_from_password(SHARE_TOKEN, &jar, password);
                }
            }
        }

        // OR Try to get user_token from basic auth headers

        if let Ok(TypedHeader(Authorization(basic))) =
            TypedHeader::<Authorization<Basic>>::from_request_parts(parts, state).await
        {
            match cookie_from_password(AUTH_COOKIE, &jar, basic.password()) {
                Ok(token) => return Ok(token),
                Err(_) => {
                    let config = ConfigState::from_ref(state);

                    let Extension(addr) = parts
                        .extract::<Extension<ConnectInfo<SocketAddr>>>()
                        .await
                        .expect("Could not find socket address");
                    return match authenticate_local_user(
                        &config,
                        LocalAuth {
                            login: basic.username().to_string(),
                        },
                        addr.0,
                    ) {
                        Ok(user) => Ok(user.1),
                        Err(e) => Err((e.0, "no user found in basic auth")),
                    };
                }
            }
        }

        Err((
            StatusCode::UNAUTHORIZED,
            "no user found or xsrf token not provided",
        ))
    }
}

fn cookie_from_password(
    cookie_name: &str,
    jar: &PrivateCookieJar,
    password: &str,
) -> Result<UserToken, (StatusCode, &'static str)> {
    let cookie = Cookie::parse_encoded(format!("{}={}", cookie_name, password)).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not find user token",
        )
    })?;
    let decrypted_cookie = jar.decrypt(cookie).ok_or(()).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not decrypt user token",
        )
    })?;
    let serialized_user_token = decrypted_cookie.value();
    UserToken::from_json(serialized_user_token)
}

#[derive(Serialize, Deserialize)]
pub struct AdminToken(UserToken);

#[async_trait]
impl<S> FromRequestParts<S> for AdminToken
where
    S: Send + Sync,
    Key: FromRef<S>,
    ConfigState: FromRef<S>,
{
    type Rejection = (StatusCode, &'static str);
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = UserToken::from_request_parts(parts, state).await?;
        if !user.roles.contains(&ADMINS_ROLE.to_owned()) {
            return Err((StatusCode::UNAUTHORIZED, "user is not in admin group"));
        }
        Ok(AdminToken(user))
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserTokenWithoutXSRFCheck(pub UserToken);

#[async_trait]
impl<S> FromRequestParts<S> for UserTokenWithoutXSRFCheck
where
    S: Send + Sync,
    Key: FromRef<S>,
{
    type Rejection = (StatusCode, &'static str);
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let jar: PrivateCookieJar = PrivateCookieJar::from_request_parts(parts, state)
            .await
            .expect("Could not find cookie jar");

        // Get the serialized user_token from the cookie jar, and check the xsrf token
        if let Some(cookie) = jar.get(AUTH_COOKIE) {
            // Deserialize the user_token and return him/her
            let serialized_user_token = cookie.value();
            let user_token = UserToken::from_json(serialized_user_token)?;
            return Ok(UserTokenWithoutXSRFCheck(user_token));
        }
        Err((StatusCode::UNAUTHORIZED, "no user found"))
    }
}

#[derive(Deserialize)]
pub struct LocalAuth {
    login: String,
}

#[derive(Deserialize, Serialize)]
pub struct AuthResponse {
    pub is_admin: bool,
    pub xsrf_token: String,
}

pub async fn local_auth(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    jar: PrivateCookieJar,
    State(config): State<ConfigState>,
    Host(hostname): Host,
    Json(payload): Json<LocalAuth>,
) -> Result<(PrivateCookieJar, Json<AuthResponse>), (StatusCode, &'static str)> {
    // Find the user in configuration
    let (user, user_token) = authenticate_local_user(&config, payload, addr)?;
    let cookie = create_user_cookie(&user_token, hostname, &config, addr, user)?;

    Ok((
        jar.add(cookie),
        Json(AuthResponse {
            is_admin: user.roles.contains(&ADMINS_ROLE.to_owned()),
            xsrf_token: user_token.xsrf_token,
        }),
    ))
}

pub(crate) fn create_user_cookie(
    user_token: &UserToken,
    hostname: String,
    config: &Config,
    _addr: SocketAddr,
    _user: &User,
) -> Result<Cookie<'static>, (StatusCode, &'static str)> {
    let encoded = serde_json::to_string(user_token)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "could not encode user"))?;
    let domain = hostname
        .split(':')
        .next()
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "could not find domain"))?
        .to_owned();
    let cookie = Cookie::build(AUTH_COOKIE, encoded)
        .domain(domain)
        .path("/")
        .same_site(axum_extra::extract::cookie::SameSite::Lax)
        .secure(config.tls_mode.is_secure())
        .max_age(Duration::days(config.session_duration_days.unwrap_or(1)))
        .http_only(true)
        .finish();

    Ok(cookie)
}

pub fn authenticate_local_user(
    config: &Config,
    payload: LocalAuth,
    _addr: SocketAddr,
) -> Result<(&User, UserToken), (StatusCode, &'static str)> {
    let user = config
        .users
        .iter()
        .find(|u| u.login == payload.login)
        .ok_or(StatusCode::UNAUTHORIZED)
        .map_err(|e| (e, "user does not exist"))?;

    // Create a token payload from the user
    let user_token = user_to_token(user, config);
    Ok((user, user_token))
}

pub(crate) fn user_to_token(user: &User, config: &Config) -> UserToken {
    UserToken {
        login: user.login.to_owned(),
        roles: user.roles.to_owned(),
        xsrf_token: random_string(16),
        share: None,
        expires: (OffsetDateTime::now_utc()
            + Duration::days(config.session_duration_days.unwrap_or(1)))
        .unix_timestamp(),
        info: user.info.clone(),
    }
}

pub async fn get_users(
    State(config_file): State<ConfigFile>,
    _admin: AdminToken,
) -> Result<Json<Vec<User>>, (StatusCode, &'static str)> {
    let config = config_or_error(&config_file).await?;
    // Return all the users as Json
    Ok(Json(config.users))
}

pub async fn delete_user(
    State(config_file): State<ConfigFile>,
    _admin: AdminToken,
    Path(user_login): Path<String>,
) -> Result<impl IntoResponse, impl IntoResponse> {
    let mut config = config_or_error(&config_file).await?;
    // Find the user
    if let Some(pos) = config.users.iter().position(|u| u.login == user_login) {
        // It is an existing user, delete it
        config.users.remove(pos);
    } else {
        // If the user does not exist, respond with an error
        return Err((StatusCode::BAD_REQUEST, "user does not exist"));
    }

    config
        .to_file_or_internal_server_error(&config_file)
        .await?;

    Ok((StatusCode::OK, "user deleted successfully"))
}

pub async fn add_user(
    State(config_file): State<ConfigFile>,
    State(config): State<ConfigState>,
    _admin: AdminToken,
    Json(mut payload): Json<User>,
) -> Result<impl IntoResponse, impl IntoResponse> {
    // Clone the config
    let mut config = (*config).clone();
    // Find the user
    if let Some(user) = config.users.iter_mut().find(|u| u.login == payload.login) {
        // It is an existing user, we only hash the password if it is not empty
        if !payload.password.is_empty() {
        } else {
            payload.password = user.password.clone();
        }
        *user = payload;
    } else {
        // It is a new user, we need to hash the password
        if payload.password.is_empty() {
            return Err((StatusCode::NOT_ACCEPTABLE, "password is required"));
        }

        config.users.push(payload);
    }

    config
        .to_file_or_internal_server_error(&config_file)
        .await?;

    Ok((StatusCode::CREATED, "user created or updated successfully"))
}

pub async fn whoami(token: UserToken) -> Json<User> {
    let user = User {
        login: token.login,
        password: REDACTED.to_owned(),
        roles: token.roles,
        info: token.info,
    };
    Json(user)
}

pub async fn cookie_to_body<B>(
    req: Request<B>,
    next: Next<B>,
) -> Result<impl IntoResponse, StatusCode> {
    let res = next.run(req).await;
    let (mut parts, _) = res.into_parts();
    if parts.status == StatusCode::OK {
        let cookie = parts
            .headers
            .get("set-cookie")
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
            .as_bytes()
            .to_owned();
        parts
            .headers
            .insert(CONTENT_LENGTH, HeaderValue::from(cookie.len()));
        let res = Response::from_parts(parts, Body::from(cookie));
        Ok(res)
    } else {
        Ok(Response::from_parts(parts, Body::empty()))
    }
}

pub fn check_user_has_role(user: &UserToken, roles: &[String]) -> bool {
    for user_role in user.roles.iter() {
        for role in roles.iter() {
            if user_role == role {
                return true;
            }
        }
    }
    false
}

pub fn check_user_has_role_or_forbid(
    user: &Option<&UserToken>,
    target: &HostType,
    hostname: &str,
    path: &str,
) -> Option<Response<Body>> {
    if let Some(user) = user {
        if !check_user_has_role(user, target.roles())
            || (user.share.is_some()
                && (user.share.as_ref().unwrap().path != path
                    || user.share.as_ref().unwrap().hostname != hostname))
        {
            return Some(
                Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body(Body::empty())
                    .unwrap(),
            );
        }
        return None;
    }
    Some(
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(&WWWAUTHENTICATE, r#"Basic realm="server""#)
            .body(Body::empty())
            .unwrap(),
    )
}

pub fn check_authorization(
    app: &HostType,
    user: &Option<&UserToken>,
    hostname: &str,
    path: &str,
) -> Option<Response<Body>> {
    if app.secured() {
        if let Some(response) = check_user_has_role_or_forbid(user, app, hostname, path) {
            return Some(response);
        }
    }
    None
}

#[cfg(test)]
mod check_expires_test {
    use super::UserToken;
    use time::{Duration, OffsetDateTime};

    #[test]
    fn test_expires_ok() {
        let user = UserToken {
            expires: (OffsetDateTime::now_utc() + Duration::seconds(1)).unix_timestamp(),
            ..Default::default()
        };
        assert!(user.check_expires().is_ok());
    }

    #[test]
    fn test_expires_error() {
        let user = UserToken {
            expires: (OffsetDateTime::now_utc() - Duration::seconds(1)).unix_timestamp(),
            ..Default::default()
        };
        assert!(user.check_expires().is_err());
    }
}

#[cfg(test)]
mod check_user_has_role_or_forbid_tests {
    use crate::{
        apps::{App, AppWithUri},
        configuration::HostType,
        users::{check_user_has_role_or_forbid, UserToken},
    };

    #[test]
    fn test_no_user() {
        let user = &None;
        let app: App = App {
            target: "www.example.com".to_string(), // to prevent failing when parsing url
            roles: vec!["role1".to_string(), "role2".to_string()],
            ..Default::default()
        };
        let app = AppWithUri::from_app_domain_and_http_port(app, "atrium.io", None);
        let target = HostType::ReverseApp(Box::new(app));
        assert!(check_user_has_role_or_forbid(user, &target, "", "").is_some());
    }

    #[test]
    fn test_user_has_all_roles() {
        let user = UserToken {
            roles: vec!["role1".to_string(), "role2".to_string()],
            ..Default::default()
        };
        let app: App = App {
            target: "www.example.com".to_string(), // to prevent failing when parsing url
            roles: vec!["role1".to_string(), "role2".to_string()],
            ..Default::default()
        };
        let app = AppWithUri::from_app_domain_and_http_port(app, "atrium.io", None);
        let target = HostType::ReverseApp(Box::new(app));
        assert!(check_user_has_role_or_forbid(&Some(&user), &target, "", "").is_none());
    }

    #[test]
    fn test_user_has_one_role() {
        let user = UserToken {
            roles: vec!["role1".to_string()],
            ..Default::default()
        };
        let app: App = App {
            target: "www.example.com".to_string(), // to prevent failing when parsing url
            roles: vec!["role1".to_string(), "role2".to_string()],
            ..Default::default()
        };
        let app = AppWithUri::from_app_domain_and_http_port(app, "atrium.io", None);
        let target = HostType::ReverseApp(Box::new(app));
        assert!(check_user_has_role_or_forbid(&Some(&user), &target, "", "").is_none());
    }

    #[test]
    fn test_user_has_no_role() {
        let user = UserToken {
            roles: vec!["role3".to_string(), "role4".to_string()],
            ..Default::default()
        };
        let app: App = App {
            target: "www.example.com".to_string(), // to prevent failing when parsing url
            roles: vec!["role1".to_string(), "role2".to_string()],
            ..Default::default()
        };
        let app = AppWithUri::from_app_domain_and_http_port(app, "atrium.io", None);
        let target = HostType::ReverseApp(Box::new(app));
        assert!(check_user_has_role_or_forbid(&Some(&user), &target, "", "").is_some());
    }

    #[test]
    fn test_user_roles_are_empty() {
        let user = UserToken::default();
        let app = App {
            target: "www.example.com".to_string(), // to prevent failing when parsing url
            roles: vec!["role1".to_string(), "role2".to_string()],
            ..Default::default()
        };
        let app = AppWithUri::from_app_domain_and_http_port(app, "atrium.io", None);
        let target = HostType::ReverseApp(Box::new(app));
        assert!(check_user_has_role_or_forbid(&Some(&user), &target, "", "").is_some());
    }

    #[test]
    fn test_allowed_roles_are_empty() {
        let user = UserToken {
            roles: vec!["role1".to_string(), "role2".to_string()],
            ..Default::default()
        };
        let app = App {
            target: "www.example.com".to_string(), // to prevent failing when parsing url
            ..Default::default()
        };
        let app = AppWithUri::from_app_domain_and_http_port(app, "atrium.io", None);
        let target = HostType::ReverseApp(Box::new(app));
        assert!(check_user_has_role_or_forbid(&Some(&user), &target, "", "").is_some());
    }

    #[test]
    fn test_all_roles_are_empty() {
        let user = UserToken::default();
        let app = App {
            target: "www.example.com".to_string(), // to prevent failing when parsing url
            ..Default::default()
        };
        let app = AppWithUri::from_app_domain_and_http_port(app, "atrium.io", None);
        let target = HostType::ReverseApp(Box::new(app));
        assert!(check_user_has_role_or_forbid(&Some(&user), &target, "", "").is_some());
    }
}
