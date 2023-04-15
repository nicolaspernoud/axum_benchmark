use axum::{
    middleware,
    response::IntoResponse,
    routing::{delete, get, get_service, post},
    Router,
};

use http::StatusCode;
use hyper::{Body, Request};

use tower::ServiceExt;

use tower_http::services::ServeDir;

use crate::{
    apps::{add_app, delete_app, get_apps, proxy_handler},
    appstate::AppState,
    configuration::{load_config, HostType},
    dir_server::dir_handler,
    middlewares::inject_security_headers,
    sysinfo::system_info,
    users::{add_user, delete_user, get_users, local_auth, whoami},
};

pub struct Server {
    pub router: Router,
    pub port: u16,
}

impl Server {
    pub async fn build(config_file: &str) -> Result<Self, anyhow::Error> {
        let config = load_config(config_file).await?;

        let state = AppState::new(
            axum_extra::extract::cookie::Key::from(
                config.0.cookie_key.as_ref().unwrap().as_bytes(),
            ),
            config.0,
            config.1,
            config_file.to_owned(),
        );

        let user_router: Router<AppState> = Router::new()
            .route("/api/user/whoami", get(whoami))
            .route("/api/user/system_info", get(system_info));

        let admin_router = Router::new()
            .route("/api/admin/users", get(get_users).post(add_user))
            .route("/api/admin/users/:user_login", delete(delete_user))
            .route("/api/admin/apps", get(get_apps).post(add_app))
            .route("/api/admin/apps/:app_id", delete(delete_app));

        let main_router: Router<()> = Router::new()
            .route("/auth/local", post(local_auth))
            .merge(admin_router)
            .merge(user_router)
            .fallback_service(get_service(ServeDir::new("web")).handle_error(error_500))
            .with_state(state.clone());

        let proxy_router = Router::new()
            .fallback(proxy_handler)
            .with_state(state.clone());

        let dir_router = Router::new()
            .fallback(dir_handler)
            .with_state(state.clone());

        let router = Router::new()
            .fallback(
                |hostype: Option<HostType>, request: Request<Body>| async move {
                    match hostype {
                        Some(HostType::StaticApp(_)) => dir_router.oneshot(request).await,
                        Some(HostType::ReverseApp(_)) => proxy_router.oneshot(request).await,
                        None => main_router.oneshot(request).await,
                    }
                },
            )
            .layer(middleware::from_fn_with_state(
                state.clone(),
                inject_security_headers,
            ))
            .with_state(state);

        Ok(Server { router, port: 8080 })
    }
}

async fn error_500(_err: std::convert::Infallible) -> impl IntoResponse {
    (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong...")
}
