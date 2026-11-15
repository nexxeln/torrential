use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::{Instant, timeout_at};
use url::Url;

pub(crate) const USER_AGENT: &str = concat!("torrential/", env!("CARGO_PKG_VERSION"));

pub type Recorded = Vec<(String, Vec<u8>)>;

pub(crate) struct Http {
    client: reqwest::Client,
    recorded: Option<Mutex<Recorded>>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum FetchError {
    /// No answer came in the time the fetch waited.
    Timeout(Duration),
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
        Http::build(agent, record, reqwest::redirect::Policy::default(), false)
    }

    /// A client for the public internet that introduces itself as `agent` and follows only the
    /// redirects `policy` allows. It resolves each host name once and connects only to the public
    /// addresses it found.
    pub fn public(agent: &str, policy: reqwest::redirect::Policy) -> Result<Http, reqwest::Error> {
        Http::build(agent, false, policy, true)
    }

    fn build(
        agent: &str,
        record: bool,
        policy: reqwest::redirect::Policy,
        public: bool,
    ) -> Result<Http, reqwest::Error> {
        let mut builder = reqwest::Client::builder()
            .user_agent(agent)
            .gzip(true)
            .brotli(true)
            .connect_timeout(Duration::from_secs(5))
            .redirect(policy);
        if public {
            builder = builder.dns_resolver(PublicOnly);
        }
        Ok(Http { client: builder.build()?, recorded: record.then(Mutex::default) })
    }

    pub fn detached(&self) -> Http {
        Http { client: self.client.clone(), recorded: None }
    }

    pub async fn get(&self, url: &Url, cap: usize, deadline: Instant) -> Result<Vec<u8>, FetchError> {
        self.fetch(url, cap, deadline, Retry::Once).await
    }

    pub async fn fetch(&self, url: &Url, cap: usize, deadline: Instant, retry: Retry) -> Result<Vec<u8>, FetchError> {
        let started = Instant::now();
        let body = timeout_at(deadline, self.fetch_by(url, cap, deadline, retry, started))
            .await
            .unwrap_or_else(|_| Err(FetchError::Timeout(started.elapsed())))?;
        if let Some(recorded) = &self.recorded {
            recorded.lock().unwrap_or_else(|e| e.into_inner()).push((url.to_string(), body.clone()));
        }
        Ok(body)
    }

    /// Where a request for the first byte of `url` lands once its redirects are followed.
    pub async fn landing(&self, url: &Url, deadline: Instant) -> Result<Url, FetchError> {
        let started = Instant::now();
        let asked = self.client.get(url.clone()).header(reqwest::header::RANGE, "bytes=0-0").send();
        let response = timeout_at(deadline, asked)
            .await
            .map_err(|_| FetchError::Timeout(started.elapsed()))?
            .map_err(|e| classify(e, started))?;
        if !response.status().is_success() {
            return Err(FetchError::Status(response.status().as_u16()));
        }
        Ok(response.url().clone())
    }

    pub fn take_recorded(&self) -> Recorded {
        self.recorded
            .as_ref()
            .map(|r| std::mem::take(&mut *r.lock().unwrap_or_else(|e| e.into_inner())))
            .unwrap_or_default()
    }

    async fn fetch_by(
        &self,
        url: &Url,
        cap: usize,
        deadline: Instant,
        retry: Retry,
        started: Instant,
    ) -> Result<Vec<u8>, FetchError> {
        const RETRY_PAUSE: Duration = Duration::from_millis(200);
        const MIN_RETRY: Duration = Duration::from_millis(600);
        let left = || deadline.saturating_duration_since(Instant::now());
        let join_at = Instant::now() + left() * 3 / 5;
        let can_join = retry == Retry::Once && left() * 2 / 5 > MIN_RETRY;
        let first = self.respond(url, started);
        tokio::pin!(first);
        let (response, retried) = tokio::select! {
            response = &mut first => (response, false),
            _ = tokio::time::sleep_until(join_at), if can_join => {
                (first_ok(first, self.respond(url, started)).await, true)
            }
        };
        let response = match response {
            Err(FetchError::Status(status))
                if status >= 500 && retry == Retry::Once && !retried && left() > MIN_RETRY =>
            {
                tokio::time::sleep(RETRY_PAUSE).await;
                self.respond(url, started).await
            }
            other => other,
        }?;
        read(response, cap, started).await
    }

    async fn respond(&self, url: &Url, started: Instant) -> Result<reqwest::Response, FetchError> {
        let response = self.client.get(url.clone()).send().await.map_err(|e| classify(e, started))?;
        let status = response.status();
        if !status.is_success() {
            return Err(FetchError::Status(status.as_u16()));
        }
        Ok(response)
    }
}

async fn read(mut response: reqwest::Response, cap: usize, started: Instant) -> Result<Vec<u8>, FetchError> {
    if response.content_length().is_some_and(|len| len > cap as u64) {
        return Err(FetchError::TooLarge(cap));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| classify(e, started))? {
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

/// `err` as a fetch error. A timeout, such as connecting taking longer than the client allows,
/// says how long the fetch begun at `started` waited.
/// Whether `ip` is on the public internet: not this machine, the home network, a link-local or
/// shared address, or a range no server is at.
pub(crate) fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [first, second, ..] = ip.octets();
            let shared = first == 100 && (64..128).contains(&second);
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_multicast()
                || shared
                || first == 0
                || first >= 240)
        }
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => public(IpAddr::V4(mapped)),
            None => {
                !(ip.is_loopback()
                    || ip.is_unspecified()
                    || ip.is_multicast()
                    || ip.is_unique_local()
                    || ip.is_unicast_link_local())
            }
        },
    }
}

/// Resolves a host name and keeps only its public addresses, which the client then connects
/// to, so no name leads a request to this machine or the home network.
struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let reachable: Vec<SocketAddr> = found.into_iter().filter(|at| public(at.ip())).collect();
            if reachable.is_empty() {
                return Err(format!("{host} has no address on the public internet").into());
            }
            Ok(Box::new(reachable.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn classify(err: reqwest::Error, started: Instant) -> FetchError {
    if err.is_timeout() {
        return FetchError::Timeout(started.elapsed());
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

#[cfg(test)]
mod tests {
    use reqwest::dns::Resolve;

    use super::*;

    #[test]
    fn only_addresses_on_the_public_internet_are_public() {
        let cases = [
            ("93.184.215.14", true),
            ("1.1.1.1", true),
            ("2606:4700:4700::1111", true),
            ("127.0.0.1", false),
            ("10.0.0.7", false),
            ("172.16.4.1", false),
            ("192.168.1.31", false),
            ("169.254.169.254", false),
            ("100.64.0.1", false),
            ("0.0.0.0", false),
            ("255.255.255.255", false),
            ("224.0.0.251", false),
            ("::1", false),
            ("::", false),
            ("fe80::1", false),
            ("fd12:3456::1", false),
            ("::ffff:127.0.0.1", false),
            ("::ffff:93.184.215.14", true),
        ];
        let read = cases.map(|(ip, _)| (ip, public(ip.parse().unwrap())));
        assert_eq!(read, cases);
    }

    #[tokio::test]
    async fn a_name_that_leads_only_to_this_machine_resolves_to_nothing() {
        let error = PublicOnly.resolve("localhost".parse().unwrap()).await.err().map(|e| e.to_string());
        assert_eq!(error.as_deref(), Some("localhost has no address on the public internet"));
    }

    #[tokio::test]
    async fn a_fetch_with_no_answer_says_how_long_it_waited() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/a.torrent", listener.local_addr().unwrap())).unwrap();
        let held = tokio::spawn(async move { listener.accept().await });
        let http = Http::new(false).unwrap();
        let error = http.fetch(&url, 1 << 20, Instant::now() + Duration::from_millis(1200), Retry::Never).await;
        let Err(FetchError::Timeout(waited)) = error else { panic!("no answer is a timeout, not {error:?}") };
        assert!((1200..1500).contains(&waited.as_millis()), "{waited:?}");
        assert_eq!(crate::fetch_reason(FetchError::Timeout(waited)), "no response in 1 second");
        held.abort();
    }
}
