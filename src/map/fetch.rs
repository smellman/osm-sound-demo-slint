//! Serves MapLibre Native's own network requests, on Android.
//!
//! Everywhere else MapLibre Native fetches its own tiles and this module is not
//! compiled. Android is the exception: there it has no certificate store it can
//! read from C++, so verification goes out to Java through
//! `rustls-platform-verifier`, whose class has to be in the APK. `cargo-apk`
//! builds Rust and a manifest and compiles no Kotlin, so the class is not there
//! and every HTTPS request fails with "failed to call native verifier".
//!
//! Rather than move the whole build to Gradle for one Java class, the requests
//! are answered here instead — through the same `ureq` this app already uses for
//! the release API, which carries its own roots and was working on the device
//! before the map was. One HTTP stack for the whole app, and no Java.
//!
//! MapLibre calls the provider from its own worker threads and asks that it
//! return quickly, so nothing is fetched inline: a request is queued and the
//! handle travels to a worker, which completes it whenever the body arrives.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use maplibre_native_ffi::{
    ResourceErrorReason, ResourceProvider, ResourceRange, ResourceRequestHandle, ResourceResponse,
    ResourceResponseStatus, Result, RuntimeHandle,
};

/// How many requests can be in flight. The map asks for a lot of tiles at once
/// and each worker holds one until its body lands, so this is what decides how
/// much of the network is used at a time.
const WORKERS: usize = 6;

/// A tile that has not arrived in this long is not worth waiting for; the map
/// will ask again when it still wants it.
const TIMEOUT: Duration = Duration::from_secs(20);

struct Job {
    url: String,
    range: Option<ResourceRange>,
    handle: ResourceRequestHandle,
}

/// Installs the provider on `runtime`.
pub fn install(runtime: &RuntimeHandle) -> Result<()> {
    let (sender, receiver) = channel::<Job>();
    let receiver = Arc::new(Mutex::new(receiver));
    for worker in 0..WORKERS {
        let receiver = Arc::clone(&receiver);
        // Named so a stack in a crash report says which thread this is.
        let _ = std::thread::Builder::new()
            .name(format!("maplibre-fetch-{worker}"))
            .spawn(move || serve(&receiver));
    }

    // The callback has to be `Sync`, and the lock keeps that true whatever the
    // channel is. It is only held for the length of a `send`, which does not
    // block.
    let sender = Mutex::new(sender);
    let provider = ResourceProvider::new(move |request, handle| {
        // The bundled glyphs and sprite need no worker: they are already here.
        let Err(handle) = super::bundled::answer(&request.resolved_url, handle) else {
            return;
        };
        let job = Job {
            url: request.resolved_url,
            range: request.range,
            handle,
        };
        let sent = match sender.lock() {
            Ok(sender) => sender.send(job).map_err(|unsent| unsent.0),
            Err(_) => Err(job),
        };
        // Falling back to native networking would only fail the way this
        // exists to avoid, but it is the honest answer: this provider is no
        // longer able to serve the request.
        if let Err(job) = sent
            && let Err((_, error)) = job.handle.pass_through()
        {
            eprintln!("handing a map request to the network failed: {error}");
        }
    });
    let installed = runtime.set_resource_provider(provider)?;
    match installed.wait(Duration::from_secs(5)) {
        Ok(result) => result.map(|_| ()),
        Err(_) => {
            eprintln!("the runtime did not take the fetch provider in time");
            Ok(())
        }
    }
}

/// What a worker found, held until the request is answered: the response the
/// binding takes only borrows its bytes and message.
enum Reply {
    Ok(Vec<u8>),
    NoContent,
    NotModified,
    Error(ResourceErrorReason, String),
}

impl Reply {
    fn error(reason: ResourceErrorReason, message: impl Into<String>) -> Self {
        Self::Error(reason, message.into())
    }

    fn response(&self) -> ResourceResponse<'_> {
        match self {
            Self::Ok(bytes) => ResourceResponse {
                status: ResourceResponseStatus::Ok,
                bytes,
                ..Default::default()
            },
            Self::NoContent => ResourceResponse {
                status: ResourceResponseStatus::NoContent,
                ..Default::default()
            },
            Self::NotModified => ResourceResponse {
                status: ResourceResponseStatus::NotModified,
                ..Default::default()
            },
            Self::Error(reason, message) => ResourceResponse {
                status: ResourceResponseStatus::Error,
                error_reason: *reason,
                error_message: message,
                ..Default::default()
            },
        }
    }
}

fn serve(receiver: &Mutex<Receiver<Job>>) {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build()
        .new_agent();

    loop {
        // The lock is released before the request runs, so the other workers
        // are free to take the next job rather than queueing behind this one.
        let job = {
            let Ok(receiver) = receiver.lock() else {
                return;
            };
            match receiver.recv() {
                Ok(job) => job,
                // The sender is gone with the runtime; nothing more will come.
                Err(_) => return,
            }
        };

        // The map gives up on tiles it has scrolled away from. Answering one
        // anyway is wasted bandwidth on a phone.
        if job.handle.is_cancelled().unwrap_or(false) {
            let _ = job.handle.release();
            continue;
        }

        let reply = fetch(&agent, &job);
        if let Err((_, error)) = job.handle.complete(&reply.response()) {
            eprintln!("completing a map request failed: {error}");
        }
    }
}

fn fetch(agent: &ureq::Agent, job: &Job) -> Reply {
    let mut request = agent.get(&job.url);
    if let Some(range) = job.range {
        // Inclusive, as HTTP byte ranges are.
        request = request.header("Range", format!("bytes={}-{}", range.start, range.end));
    }

    let mut response = match request.call() {
        Ok(response) => response,
        Err(error) => return failure(&error),
    };

    let status = response.status().as_u16();
    match status {
        // 206 as well as 200: a ranged request is answered with partial
        // content, and that is exactly what was asked for.
        200 | 206 => match response.body_mut().read_to_vec() {
            Ok(bytes) => Reply::Ok(bytes),
            Err(error) => Reply::error(ResourceErrorReason::Connection, error.to_string()),
        },
        204 => Reply::NoContent,
        304 => Reply::NotModified,
        404 | 410 => Reply::error(ResourceErrorReason::NotFound, format!("HTTP {status}")),
        429 => Reply::error(ResourceErrorReason::RateLimit, format!("HTTP {status}")),
        500..=599 => Reply::error(ResourceErrorReason::Server, format!("HTTP {status}")),
        _ => Reply::error(ResourceErrorReason::Other, format!("HTTP {status}")),
    }
}

/// Turns a transport failure into the reason MapLibre understands.
///
/// The distinction earns its keep: a `Connection` failure is retried, where
/// `Other` is not, and a phone that has just walked out of range should get its
/// tiles back when it walks in again.
fn failure(error: &ureq::Error) -> Reply {
    let reason = match error {
        ureq::Error::StatusCode(404) => ResourceErrorReason::NotFound,
        ureq::Error::StatusCode(429) => ResourceErrorReason::RateLimit,
        ureq::Error::StatusCode(code) if (500..600).contains(code) => ResourceErrorReason::Server,
        ureq::Error::Timeout(_) | ureq::Error::Io(_) => ResourceErrorReason::Connection,
        _ => ResourceErrorReason::Other,
    };
    Reply::error(reason, error.to_string())
}
