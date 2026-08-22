//! A listener that catches a journey leaving the fake.
//!
//! Isolating `HOME` and `XDG_*` stops a journey from reading the user's
//! configuration, and it does nothing at all about the user's environment
//! still carrying a real API key: `agens chat` bills real calls with `$HOME`
//! overridden. Two things close that gap, and both live here so a journey gets
//! them by construction rather than by remembering.
//!
//! The first is removing the credential variables. The second is a proxy the
//! agent will use for anything that is not loopback, which answers nothing and
//! counts what arrived — so "the journey stayed inside the fake" becomes an
//! assertion instead of an assumption.

use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

/// The environment variables a provider reads a real API key from.
///
/// Kept in step with `API_KEY_ENVIRONMENT` in `agens-bootstrap`: a provider
/// added there without being added here is a provider a journey could
/// authenticate against for real.
pub const PROVIDER_CREDENTIAL_VARIABLES: [&str; 2] = ["OPENAI_API_KEY", "MOONSHOT_API_KEY"];

/// A proxy that accepts, records, and refuses every connection it receives.
pub struct NetworkTripwire {
    address: SocketAddr,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl NetworkTripwire {
    /// The tripwire every isolated journey command in this process points at.
    ///
    /// One per process rather than one per journey: it has to outlive each
    /// command's environment, and a journey asserts on the count it observes,
    /// not on ownership of the listener.
    pub fn shared() -> &'static Self {
        static SHARED: OnceLock<NetworkTripwire> = OnceLock::new();

        SHARED.get_or_init(Self::start)
    }

    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("network tripwire should bind");
        listener
            .set_nonblocking(true)
            .expect("network tripwire listener should be pollable");
        let address = listener
            .local_addr()
            .expect("network tripwire should have an address");

        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_connections = Arc::clone(&connections);
        let worker_stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        worker_connections.fetch_add(1, Ordering::AcqRel);
                        let mut discarded = [0_u8; 512];
                        let _ = stream.read(&mut discarded);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return,
                }
            }
        });

        Self {
            address,
            connections,
            stop,
        }
    }

    /// The proxy environment routing every non-loopback request here.
    ///
    /// Loopback is excluded so the scripted provider itself stays reachable.
    /// `webfetch` builds its client with `no_proxy` and is therefore outside
    /// what this can observe; the provider clients, which are what a journey
    /// would bill against, are not.
    pub fn environment(&self) -> [(&'static str, String); 5] {
        let proxy = format!("http://{}", self.address);

        [
            ("HTTP_PROXY", proxy.clone()),
            ("HTTPS_PROXY", proxy.clone()),
            ("ALL_PROXY", proxy),
            ("NO_PROXY", "127.0.0.1,localhost".to_owned()),
            ("no_proxy", "127.0.0.1,localhost".to_owned()),
        ]
    }

    /// How many connections the tripwire has caught.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Acquire)
    }

    /// Fails if anything reached the tripwire, meaning a journey addressed a
    /// host that was not the fake.
    pub fn assert_no_connections(&self) {
        assert_eq!(
            self.connections(),
            0,
            "a journey addressed a host outside the scripted provider"
        );
    }

    /// Stops the tripwire's accept loop.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}
