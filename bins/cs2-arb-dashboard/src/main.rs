mod reports;
mod signals;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::Parser;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(about = "Read-only dashboard for CS2 and Rust mock batches")]
struct Args {
    #[arg(long, env = "CS2_ARB_DASHBOARD_BIND", default_value = "0.0.0.0:8787")]
    bind: SocketAddr,
    #[arg(
        long,
        env = "CS2_ARB_DASHBOARD_REPORTS_DIR",
        default_value = "/home/ubuntu/cs2-arb-mock-data/reports"
    )]
    reports_dir: PathBuf,
}

#[derive(Serialize)]
struct Snapshot {
    updated_at: DateTime<Utc>,
    kill_switch_file: Option<bool>,
    games: [Game; 2],
    watcher: Option<signals::Watcher>,
}

#[derive(Serialize)]
struct Game {
    id: &'static str,
    reports: reports::GameReports,
    market: signals::MarketHealth,
}

async fn snapshot(reports_dir: &Path) -> Result<Bytes, Box<dyn std::error::Error>> {
    let data = reports_dir.parent().unwrap_or_else(|| Path::new("."));
    let database = data.join("events.db");
    let (cs2, rust, watcher) = tokio::join!(
        game("cs2", reports_dir),
        game("rust", reports_dir),
        signals::watcher(&database),
    );
    Ok(Bytes::from(serde_json::to_vec(&Snapshot {
        updated_at: Utc::now(),
        kill_switch_file: data.join("KILL_SWITCH").try_exists().ok(),
        games: [cs2?, rust?],
        watcher,
    })?))
}

async fn game(id: &'static str, reports_dir: &Path) -> Result<Game, tokio::task::JoinError> {
    let path = reports_dir.join(id);
    let (reports, market) = tokio::join!(
        tokio::task::spawn_blocking(move || reports::read_game(&path)),
        signals::health(id)
    );
    Ok(Game {
        id,
        reports: reports?,
        market,
    })
}

fn route<B>(request: Request<B>, snapshot: &RwLock<Bytes>) -> Response<Full<Bytes>> {
    let text = "text/plain; charset=utf-8";
    let (status, mime, body) = if request.method() != Method::GET {
        (
            StatusCode::METHOD_NOT_ALLOWED,
            text,
            Bytes::from_static(b"GET only\n"),
        )
    } else {
        match request.uri().path() {
            "/" => (
                StatusCode::OK,
                "text/html; charset=utf-8",
                Bytes::from_static(include_bytes!("dashboard.html")),
            ),
            "/dashboard.js" => (
                StatusCode::OK,
                "text/javascript; charset=utf-8",
                Bytes::from_static(include_bytes!("dashboard.js")),
            ),
            "/dashboard.css" => (
                StatusCode::OK,
                "text/css; charset=utf-8",
                Bytes::from_static(include_bytes!("dashboard.css")),
            ),
            "/api/dashboard" => (
                StatusCode::OK,
                "application/json",
                snapshot.read().expect("snapshot lock").clone(),
            ),
            _ => (
                StatusCode::NOT_FOUND,
                text,
                Bytes::from_static(b"Not found\n"),
            ),
        }
    };
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'",
        );
    if status == StatusCode::METHOD_NOT_ALLOWED {
        response = response.header(header::ALLOW, "GET");
    }
    response.body(Full::new(body)).expect("static headers")
}

async fn serve_client<T: AsyncRead + AsyncWrite + Unpin>(
    stream: T,
    state: &RwLock<Bytes>,
    deadline: Duration,
) -> Result<Result<(), hyper::Error>, tokio::time::error::Elapsed> {
    let connection = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_TIMEOUT)
        .keep_alive(false)
        .serve_connection(
            TokioIo::new(stream),
            service_fn(|request| async move { Ok::<_, Infallible>(route(request, state)) }),
        );
    tokio::time::timeout(deadline, connection).await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let listener = TcpListener::bind(args.bind).await?;
    let state = Arc::new(RwLock::new(snapshot(&args.reports_dir).await?));
    let cache = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            match snapshot(&args.reports_dir).await {
                Ok(value) => *cache.write().expect("snapshot lock") = value,
                Err(error) => eprintln!("Dashboard refresh failed: {error}"),
            }
        }
    });
    eprintln!(
        "Mock dashboard listening on http://{}",
        listener.local_addr()?
    );
    let connections = Arc::new(Semaphore::new(64));
    loop {
        let slot = Arc::clone(&connections).acquire_owned().await?;
        let (stream, _) = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let _slot = slot;
            let _ = serve_client(stream, &state, CONNECTION_TIMEOUT).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tokio::io::{duplex, AsyncWriteExt};

    #[tokio::test]
    async fn disconnects_clients_that_do_not_read_the_response() {
        let (mut client, server) = duplex(128);
        let state = RwLock::new(Bytes::from(vec![0; 1024]));
        client
            .write_all(b"GET /api/dashboard HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        assert!(serve_client(server, &state, Duration::from_millis(50))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn serves_only_explicit_get_routes() {
        let state = RwLock::new(Bytes::from_static(br#"{"games":[]}"#));
        for path in ["/", "/dashboard.js", "/dashboard.css", "/api/dashboard"] {
            let request = Request::get(path).body(()).unwrap();
            assert_eq!(route(request, &state).status(), StatusCode::OK);
        }
        let request = Request::get("/api/dashboard").body(()).unwrap();
        let response = route(request, &state);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, br#"{"games":[]}"#[..]);

        for path in ["/", "/api/dashboard", "/not-a-route"] {
            for method in [Method::HEAD, Method::POST, Method::OPTIONS, Method::DELETE] {
                let request = Request::builder()
                    .method(method)
                    .uri(path)
                    .body(())
                    .unwrap();
                let response = route(request, &state);
                assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
                assert_eq!(response.headers()[header::ALLOW], "GET");
            }
        }
        let request = Request::get("/.env").body(()).unwrap();
        assert_eq!(route(request, &state).status(), StatusCode::NOT_FOUND);
    }
}
