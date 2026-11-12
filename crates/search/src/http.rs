use std::sync::Mutex;
use std::time::Duration;

use tokio::time::{Instant, timeout_at};
use url::Url;

const USER_AGENT: &str = concat!("torrential/", env!("CARGO_PKG_VERSION"));

pub type Recorded = Vec<(String, Vec<u8>)>;

pub(crate) struct Http {
    client: reqwest::Client,
    recorded: Option<Mutex<Recorded>>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum FetchError {
    Timeout,
    Status(u16),
    Connect(String),
    TooLarge(usize),
    Decode(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Retry {
    Once,
    /// Asking twice can cost twice: a Torznab search fans out to the user's
    /// private trackers.
    Never,
}

impl Http {
    pub fn new(record: bool) -> Result<Http, reqwest::Error> {
        Http::named(USER_AGENT, record)
    }

    /// A client that introduces itself as `agent`.
    pub fn named(agent: &str, record: bool) -> Result<Http, reqwest::Error> {
        let client = reqwest::Client::builder()
            .user_agent(agent)
            .gzip(true)
            .brotli(true)
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Http { client, recorded: record.then(Mutex::default) })
    }

    pub fn detached(&self) -> Http {
        Http { client: self.client.clone(), recorded: None }
    }

    pub async fn get(&self, url: &Url, cap: usize, deadline: Instant) -> Result<Vec<u8>, FetchError> {
        self.fetch(url, cap, deadline, Retry::Once).await
    }

    pub async fn fetch(&self, url: &Url, cap: usize, deadline: Instant, retry: Retry) -> Result<Vec<u8>, FetchError> {
        let body =
            timeout_at(deadline, self.fetch_by(url, cap, deadline, retry)).await.unwrap_or(Err(FetchError::Timeout))?;
        if let Some(recorded) = &self.recorded {
            recorded.lock().unwrap_or_else(|e| e.into_inner()).push((url.to_string(), body.clone()));
        }
        Ok(body)
    }

    pub fn take_recorded(&self) -> Recorded {
        self.recorded
            .as_ref()
            .map(|r| std::mem::take(&mut *r.lock().unwrap_or_else(|e| e.into_inner())))
            .unwrap_or_default()
    }

    async fn fetch_by(&self, url: &Url, cap: usize, deadline: Instant, retry: Retry) -> Result<Vec<u8>, FetchError> {
        const RETRY_PAUSE: Duration = Duration::from_millis(200);
        const MIN_RETRY: Duration = Duration::from_millis(600);
        let left = || deadline.saturating_duration_since(Instant::now());
        let join_at = Instant::now() + left() * 3 / 5;
        let can_join = retry == Retry::Once && left() * 2 / 5 > MIN_RETRY;
        let first = self.respond(url);
        tokio::pin!(first);
        let (response, retried) = tokio::select! {
            response = &mut first => (response, false),
            _ = tokio::time::sleep_until(join_at), if can_join => (first_ok(first, self.respond(url)).await, true),
        };
        let response = match response {
            Err(FetchError::Status(status))
                if status >= 500 && retry == Retry::Once && !retried && left() > MIN_RETRY =>
            {
                tokio::time::sleep(RETRY_PAUSE).await;
                self.respond(url).await
            }
            other => other,
        }?;
        read(response, cap).await
    }

    async fn respond(&self, url: &Url) -> Result<reqwest::Response, FetchError> {
        let response = self.client.get(url.clone()).send().await.map_err(classify)?;
        let status = response.status();
        if !status.is_success() {
            return Err(FetchError::Status(status.as_u16()));
        }
        Ok(response)
    }
}

async fn read(mut response: reqwest::Response, cap: usize) -> Result<Vec<u8>, FetchError> {
    if response.content_length().is_some_and(|len| len > cap as u64) {
        return Err(FetchError::TooLarge(cap));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(classify)? {
        if body.len() + chunk.len() > cap {
            return Err(FetchError::TooLarge(cap));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn first_ok<T, E>(a: impl Future<Output = Result<T, E>>, b: impl Future<Output = Result<T, E>>) -> Result<T, E> {
    tokio::pin!(a, b);
    tokio::select! {
        r = &mut a => match r { Ok(v) => Ok(v), Err(_) => b.await },
        r = &mut b => match r { Ok(v) => Ok(v), Err(_) => a.await },
    }
}

fn classify(err: reqwest::Error) -> FetchError {
    if err.is_timeout() {
        return FetchError::Timeout;
    }
    let decode = err.is_decode();
    let err = err.without_url();
    let mut parts: Vec<String> = Vec::new();
    let mut cause: Option<&dyn std::error::Error> = Some(&err);
    while let Some(e) = cause {
        let text = e.to_string();
        if !parts.iter().any(|p| p.contains(&text)) {
            parts.push(text);
        }
        cause = e.source();
    }
    let reason = parts.join(": ");
    if decode { FetchError::Decode(reason) } else { FetchError::Connect(reason) }
}
