use s3s::route::S3Route;
use s3s::{Body, S3Request, S3Response, S3Result};

use axum::http;
use http::{Extensions, HeaderMap, Method, Uri};
use tower::Service;

pub struct CustomRoute {
    router: axum::Router,
}

impl CustomRoute {
    #[must_use]
    pub fn build() -> Self {
        Self {
            router: self::handlers::register(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Extra {
    pub credentials: Option<s3s::auth::Credentials>,
    pub region: Option<String>,
    pub service: Option<String>,
}

fn convert_request(req: S3Request<Body>) -> http::Request<Body> {
    let (mut parts, _) = http::Request::new(Body::empty()).into_parts();
    parts.method = req.method;
    parts.uri = req.uri;
    parts.headers = req.headers;
    parts.extensions = req.extensions;
    parts.extensions.insert(Extra {
        credentials: req.credentials,
        region: req.region,
        service: req.service,
    });
    http::Request::from_parts(parts, req.input)
}

fn convert_response(resp: http::Response<axum::body::Body>) -> S3Response<Body> {
    let (parts, body) = resp.into_parts();
    let mut s3_resp = S3Response::new(Body::http_body_unsync(body));
    s3_resp.status = Some(parts.status);
    s3_resp.headers = parts.headers;
    s3_resp.extensions = parts.extensions;
    s3_resp
}

#[async_trait::async_trait]
impl S3Route for CustomRoute {
    fn is_match(&self, _method: &Method, uri: &Uri, _headers: &HeaderMap, _extensions: &mut Extensions) -> bool {
        let path = uri.path();
        let prefix = const_str::concat!(self::handlers::PREFIX, "/");
        path.starts_with(prefix)
    }

    async fn check_access(&self, req: &mut S3Request<Body>) -> S3Result<()> {
        if req.credentials.is_none() {
            tracing::debug!("anonymous access");
        }
        Ok(()) // allow all requests
    }

    async fn call(&self, req: S3Request<Body>) -> S3Result<S3Response<Body>> {
        let mut service = self.router.clone().into_service::<Body>();
        let req = convert_request(req);
        let result = service.call(req).await;
        match result {
            Ok(resp) => Ok(convert_response(resp)),
            Err(e) => match e {},
        }
    }
}

mod handlers {
    use std::collections::HashMap;

    use axum::Json;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Path;
    use axum::extract::Query;
    use axum::extract::Request;
    use axum::http::Response;
    use axum::response;
    use axum::routing::get;
    use axum::routing::post;

    pub async fn echo(req: Request) -> Response<Body> {
        Response::new(req.into_body())
    }

    pub async fn hello() -> &'static str {
        "Hello, World!"
    }

    pub async fn show_path(Path(path): Path<String>) -> String {
        path
    }

    pub async fn show_query(Query(query): Query<HashMap<String, String>>) -> String {
        format!("{query:?}")
    }

    pub async fn show_json(Json(json): Json<serde_json::Value>) -> response::Json<serde_json::Value> {
        tracing::debug!(?json);
        response::Json(json)
    }

    pub const PREFIX: &str = "/custom";

    pub fn register() -> Router {
        let router = Router::new()
            .route("/echo", post(echo))
            .route("/hello", get(hello))
            .route("/show_path/{*path}", get(show_path))
            .route("/show_query", get(show_query))
            .route("/show_json", post(show_json));

        Router::new().nest(PREFIX, router)
    }
}

fn main() {}
