//! Beacon's server: the HTTP API in `spec/api.md` on top of the store, the background jobs that
//! wake owed work (`spec/delivery.md`, "Background work"), and the push sender (`spec/apns.md`).
//!
//! [`App`] is everything but sockets and threads: its entry points read the time from a
//! [`Clock`], so tests run the whole server with a fake one. [`Server`] wraps an `App` with an
//! accepting thread, one thread per connection, the job thread and, with `[sink]`, the Trail copy.
//!
//! Beacon is crash-only, as Trail is: there is no shutdown step to get right. Every accepted
//! write is committed to SQLite before it is answered, owed work is a time stored beside what
//! caused it, and a push that was in flight is retried at start. So SIGTERM's default action,
//! ending the process at once, loses nothing Beacon answered for; that is how it stops.
//!
//! - `http`: requests off the socket, replies onto it.
//! - `api`: routing, access rules, and one handler per route.
//! - `routes`: the route list `GET /v1` sends, checked against `spec/api.md` by a test.
//! - `push`: draining the push queue through Apple, and Live Activities.
//! - `sink`: the copy of every item change to Trail.

mod api;
mod http;
mod push;
mod routes;
mod sink;

pub use http::{Reply, Request};
pub use routes::{Access, ROUTES, Route};

use beacon_apns::{Client, HttpTransport, Transport};
use beacon_core::Problem;
use beacon_core::config::Config;
use beacon_store::Store;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

/// The version `GET /v1` and `GET /v1/health` report.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The job thread looks at least this often (`spec/delivery.md`: "at least once a second").
const MAX_SLEEP_MS: i64 = 1_000;

/// Locks a mutex; a thread that panicked holding it doesn't take the server down.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where the time comes from: the system's, or a test's.
pub trait Clock: Send + Sync {
    /// Milliseconds since 1970 UTC.
    fn now(&self) -> i64;
}

/// The system clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }
}

/// How to start a server. [`Options::new`] is what `beacon serve` uses.
pub struct Options {
    pub config: Config,
    pub clock: Arc<dyn Clock>,
    /// Replaces the HTTP/2 connection to Apple: tests pass a fake.
    pub transport: Option<Box<dyn Transport>>,
    /// Run the job thread. Tests that drive jobs with [`Server::tick`] may turn it off.
    pub jobs: bool,
}

impl Options {
    pub fn new(config: Config) -> Options {
        Options {
            config,
            clock: Arc::new(SystemClock),
            transport: None,
            jobs: true,
        }
    }
}

/// The server without its sockets and threads.
pub struct App {
    pub config: Config,
    pub store: Store,
    pub clock: Arc<dyn Clock>,
    pub(crate) apns: Option<Client>,
    /// When this process started, for `GET /v1/health`.
    pub started: i64,
    /// Serializes job runs: the job thread and [`Server::tick`] never send the same push twice.
    jobs: Mutex<()>,
    /// Set when a request may have queued work, to wake the job thread early.
    nudged: Mutex<bool>,
    nudge: Condvar,
    stop: AtomicBool,
}

impl App {
    /// Opens the store and the push client.
    pub fn open(options: Options) -> Result<App, Vec<Problem>> {
        let Options {
            config,
            clock,
            transport,
            ..
        } = options;
        let (db_mode, dir_mode) = if config.readable_db {
            (0o644, 0o755)
        } else {
            (0o600, 0o700)
        };
        prepare_dir(&config.data_dir, dir_mode)?;
        if let Some(parent) = config.devices_db.parent()
            && !parent.as_os_str().is_empty()
        {
            prepare_dir(parent, 0o700)?;
        }
        let db = config.data_dir.join("beacon.db");
        db_file(&db, db_mode);
        db_file(&config.devices_db, 0o600);
        let store = Store::open_with_devices(&db, &config.devices_db, config.settings.clone())
            .map_err(|e| e.problems)?;
        store.set_trail_open(&config.trail_open).map_err(|e| e.problems)?;
        let apns = match &config.apns {
            None => None,
            Some(apns) => {
                let loaded = beacon_apns::Config::load(
                    &apns.key_path,
                    apns.key_id.as_deref(),
                    &apns.team_id,
                    &apns.topic,
                )
                .map_err(|p| vec![p])?;
                let transport: Box<dyn Transport> = match transport {
                    Some(t) => t,
                    None => Box::new(HttpTransport::new().map_err(|p| vec![p])?),
                };
                Some(Client::new(loaded, transport))
            }
        };
        let started = clock.now();
        Ok(App {
            config,
            store,
            clock,
            apns,
            started,
            jobs: Mutex::new(()),
            nudged: Mutex::new(false),
            nudge: Condvar::new(),
            stop: AtomicBool::new(false),
        })
    }

    pub fn now(&self) -> i64 {
        self.clock.now()
    }

    /// Wakes the job thread: a request may have queued a push.
    pub(crate) fn wake(&self) {
        *lock(&self.nudged) = true;
        self.nudge.notify_all();
    }

    /// Runs what is due now and sends what is queued. Returns when the next work is due.
    pub fn tick(&self) -> Option<i64> {
        let _one = lock(&self.jobs);
        let now = self.now();
        if let Err(e) = self.store.run_due(now) {
            eprintln!("beacon: owed work failed: {e}");
        }
        if let Err(e) = push::drain(self, now) {
            eprintln!("beacon: sending pushes failed: {e}");
        }
        self.store.next_due(self.now()).ok().flatten()
    }

    fn job_loop(&self) {
        while !self.stop.load(Ordering::Relaxed) {
            let next = self.tick();
            let now = self.now();
            let wait = next
                .map_or(MAX_SLEEP_MS, |at| (at - now).clamp(10, MAX_SLEEP_MS))
                .max(10);
            let mut nudged = lock(&self.nudged);
            if !*nudged {
                nudged = self
                    .nudge
                    .wait_timeout(nudged, Duration::from_millis(u64::try_from(wait).unwrap_or(1000)))
                    .map_or_else(|e| e.into_inner().0, |(guard, _)| guard);
            }
            *nudged = false;
        }
    }
}

/// Creates a data folder (mode 700, or 755 with `readable_db`; `spec/config.md`, "Storage").
fn prepare_dir(dir: &Path, mode: u32) -> Result<(), Vec<Problem>> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, mode);
    #[cfg(not(unix))]
    let _ = mode;
    builder.create(dir).map_err(|e| {
        vec![Problem::new(
            "data_dir",
            format!("can't create {}: {e}", dir.display()),
            "create it yourself, writable by the user Beacon runs as, or set data_dir elsewhere",
        )]
    })
}

/// A database file at `mode`: 600 (Beacon's user only), or 644 for beacon.db with `readable_db`.
/// Set before SQLite opens it, so SQLite gives its log and shared-memory files the same mode, and
/// set again on every start, so changing `readable_db` takes effect on a restart.
fn db_file(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(mode)
            .open(path);
        for suffix in ["", "-wal", "-shm"] {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            let _ = std::fs::set_permissions(&name, std::fs::Permissions::from_mode(mode));
        }
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

/// A running server.
pub struct Server {
    pub app: Arc<App>,
    addr: SocketAddr,
    open: Arc<http::Open>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Server {
    /// Opens everything, binds `listen`, and starts the threads.
    pub fn start(options: Options) -> Result<Server, Vec<Problem>> {
        let jobs = options.jobs;
        let app = Arc::new(App::open(options)?);
        let listener = TcpListener::bind(&app.config.listen).map_err(|e| {
            vec![Problem::new(
                "listen",
                format!("can't listen on {}: {e}", app.config.listen),
                "stop whatever holds that port (another Beacon?), or set listen to a free one",
            )]
        })?;
        let addr = listener.local_addr().map_err(|e| {
            vec![Problem::new(
                "listen",
                format!("no local address: {e}"),
                "report it as a bug",
            )]
        })?;
        let open = Arc::new(http::Open::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread_failed = |e: std::io::Error| {
            vec![Problem::new(
                "server",
                format!("can't start a thread: {e}"),
                "the system is out of threads or memory; free some and start again",
            )]
        };
        let mut threads = Vec::new();
        let handler: Arc<dyn http::Handler> = Arc::new(api::Api { app: app.clone() });
        threads.push(http::serve(handler, listener, open.clone(), stop.clone()).map_err(thread_failed)?);
        if jobs {
            let app = app.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("beacon-jobs".into())
                    .spawn(move || app.job_loop())
                    .map_err(thread_failed)?,
            );
        }
        if let Some(sink) = app.config.sink.clone() {
            let app = app.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("beacon-sink".into())
                    .stack_size(512 * 1024)
                    .spawn(move || sink::run(&app, &sink))
                    .map_err(thread_failed)?,
            );
        }
        Ok(Server {
            app,
            addr,
            open,
            stop,
            threads,
        })
    }

    /// The address it listens on (the port, when `listen` asked for port 0).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Runs the jobs once, now: what a test does after moving its clock.
    pub fn tick(&self) -> Option<i64> {
        self.app.tick()
    }

    /// Stops accepting, hangs up on every caller and stops the threads.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.app.stop.store(true, Ordering::Relaxed);
        self.app.wake();
        // The accepting thread wakes on its next connection: make one.
        let _ = std::net::TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        self.open.close_all();
        for thread in self.threads {
            let _ = thread.join();
        }
    }

    /// Serves until the process is killed.
    pub fn wait(self) {
        for thread in self.threads {
            let _ = thread.join();
        }
    }
}

/// `beacon serve`: runs Beacon with `config` until the process is stopped.
pub fn run(config: Config) -> Result<(), Vec<Problem>> {
    let push = if config.apns.is_some() {
        "pushing through Apple"
    } else {
        "no APNs key: decisions are recorded, nothing is sent"
    };
    let server = Server::start(Options::new(config))?;
    eprintln!("beacon {VERSION}: listening on http://{} ({push})", server.addr());
    server.wait();
    Ok(())
}
