use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use futures::TryStreamExt as _;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use log::error;
use tokio::fs::File;
use tokio::net::TcpListener;
use tokio_util::io::ReaderStream;

fn err_response(
    err_msg: &'static str,
    status: StatusCode,
) -> Response<BoxBody<Bytes, std::io::Error>> {
    let mut resp = Response::new(
        Full::new(Bytes::from(err_msg))
            .map_err(|e| match e {})
            .boxed(),
    );
    *resp.status_mut() = status;
    resp
}

// TODO: This function returns errors, but that doesn't actually lead to the
// server returning an error.
// I dunno maybe this idea of just directly using Hyper was dumb.
async fn handle_req(
    root_dir: Arc<PathBuf>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, std::io::Error>>, anyhow::Error> {
    if req.method() != Method::GET {
        return Ok(err_response(
            "Only GET is supported",
            StatusCode::NOT_IMPLEMENTED,
        ));
    }

    // When I tried to leak data from outside the directory using "..", it
    // failed when using curl. I dunno if this is because of some pleasant
    // security feature of Hyper or of HTTP, or if curl just soft-gloved me, or
    // someting else.
    // To be sure, we reject such weirdness explicitly.
    // TODO: Can't figure out how to do this safely and concisely. So for now just reject anything with ".." - hopefully that's safe....?
    if req.uri().path().contains("..") {
        return Ok(err_response(
            "Paths with '..' not supported lmao",
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
    }
    let mut file_path = root_dir.join(req.uri().path().trim_start_matches('/'));
    if !file_path.starts_with(root_dir.as_ref()) {
        error!("Canonicalized request file path didn't start with root dir, something fishy?");
        file_path = (*root_dir).clone();
    }

    if !file_path.exists() || !file_path.is_file() {
        return Ok(err_response("File not found", StatusCode::NOT_FOUND));
    }

    let file = File::open(&file_path)
        .await
        .with_context(|| format!("opening file path {file_path:?}"))?;
    // Don't understand this crazy magic, cargo-culted it from
    // https://github.com/hyperium/hyper/blob/bb51c81b74cbbeaa922d52d4472b25eaf3c62eff/examples/send_file.rs#L49
    // Hopefully it doesn't try to buffer the whole file, nor buffer individual
    // bytes. But it's kinda weird that I haven't configured a buffer size anywhere.
    let reader_stream = ReaderStream::new(file);
    let stream_body = StreamBody::new(reader_stream.map_ok(Frame::data));
    let boxed_body = stream_body.boxed();

    Ok(Response::builder()
        .status(StatusCode::OK)
        .body(boxed_body)
        .context("constructing Body")?)
}

async fn do_serve_dir(root_dir: PathBuf) -> anyhow::Result<()> {
    let root_dir = Arc::new(root_dir);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));

    let listener = TcpListener::bind(addr).await?;

    loop {
        let (stream, _) = listener.accept().await?;

        // Use an adapter to access something implementing `tokio::io` traits as if they implement
        // `hyper::rt` IO traits.
        let io = TokioIo::new(stream);

        let root_dir = root_dir.clone();
        let service = service_fn(move |r| handle_req(root_dir.clone(), r));

        // Spawn a tokio task to serve multiple connections concurrently
        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                error!("Error serving web connection: {:?}", err);
            }
        });
    }
}

pub async fn serve_dir(root_dir: PathBuf) {
    do_serve_dir(root_dir).await.expect("error serving HTTP")
}
