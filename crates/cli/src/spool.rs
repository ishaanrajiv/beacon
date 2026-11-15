//! The spool (`spec/clients.md`, "The spool"): a request Beacon couldn't take is kept in a folder,
//! one file each, and sent first by the next command that uses the folder, or by `beacon flush`.
//! The same files as the Python and TypeScript emitters write, so any of them flushes the others'.

use crate::args::Args;
use crate::http::Http;
use beacon_core::Problem;
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

/// At most this many requests wait; the oldest go first to make room.
const MAX_FILES: usize = 1000;
/// And at most this many bytes.
const MAX_BYTES: u64 = 8 * 1024 * 1024;
/// A request older than this is dropped: an hour inside Beacon's 24-hour idempotency window
/// (`spec/delivery.md`), which began when Beacon first took the request. That can be before it
/// was queued here, when only the answer was lost.
const MAX_AGE_MS: i64 = 23 * 60 * 60 * 1000;
/// A temporary file this old was left by a process that stopped mid-write.
const STALE_TEMPORARY_MS: i64 = 10 * 60 * 1000;
/// A snapshot older than this would be refused for its `sent_at` (`spec/api.md`).
const SNAPSHOT_MAX_AGE_MS: i64 = 5 * 60 * 1000;

const SNAPSHOTS: &str = "/v1/snapshots";
/// The highest sequence within one millisecond; the next request takes the next millisecond.
const LAST_SEQUENCE: u32 = 999_999;
/// Names a request tries, each taken by another process just first, before the spool gives up.
const NAME_TRIES: usize = 20;

pub struct Spool {
    pub dir: PathBuf,
}

/// What a flush did.
pub struct Flushed {
    pub sent: usize,
    pub left: usize,
    pub dropped: usize,
    /// Why the first of `left` couldn't go.
    pub problem: Option<String>,
}

/// `408`, `429` or `5xx`: worth trying again later, like no answer at all.
pub fn retryable(status: u16) -> bool {
    matches!(status, 408 | 429) || status >= 500
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if getrandom::fill(&mut buf).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        buf = nanos.to_be_bytes().iter().copied().cycle().take(bytes).collect();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// The spool's byte limit; a test may lower it.
fn max_bytes() -> u64 {
    #[cfg(test)]
    if let Some(bytes) = fault::max_bytes() {
        return bytes;
    }
    MAX_BYTES
}

/// When a request file was queued and its place among those queued in that millisecond, from its
/// name: `<ms>-<sequence>.json`, or the older `<ms>-<sequence>-<hex>.json`.
fn position(name: &str) -> Option<(i64, u32)> {
    let stem = name.strip_suffix(".json")?;
    let (ms, rest) = (stem.get(..15)?, stem.get(15..)?);
    let rest = rest.strip_prefix('-')?;
    let (sequence, tail) = (rest.get(..6)?, rest.get(6..)?);
    let digits = |text: &str| text.bytes().all(|b| b.is_ascii_digit());
    let random = tail.is_empty()
        || tail
            .strip_prefix('-')
            .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()));
    if !(digits(ms) && digits(sequence) && random) {
        return None;
    }
    Some((ms.parse().ok()?, sequence.parse().ok()?))
}

/// Creates `path`, which must not exist yet, holding `data`, and syncs it to disk. Readable by this
/// user only (mode 600): it holds notification text. A hard link to it shares the mode. A file
/// that couldn't be written whole is removed, so a full disk leaves nothing behind.
fn create_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    fault::check()?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    let written = write_all(&mut file, data).and_then(|()| {
        #[cfg(test)]
        fault::note("sync file");
        file.sync_all()
    });
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written
}

fn write_all(file: &mut std::fs::File, data: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    if fault::write_fails_part_way() {
        file.write_all(data.get(..data.len() / 2).unwrap_or_default())?;
        return Err(std::io::Error::other("no space left on device"));
    }
    file.write_all(data)
}

/// Asks the system to keep a folder's names through a power cut. Fails when it tried and couldn't
/// (a failing disk); a platform or file system that doesn't sync folders is left to what it
/// promises on its own.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    fault::note_synced(dir);
    match try_sync(dir) {
        Err(e)
            if !matches!(
                e.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::InvalidInput
            ) =>
        {
            Err(e)
        }
        _ => Ok(()),
    }
}

fn try_sync(dir: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    fault::folder_sync()?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// A temporary file's name, as every client makes it: `.<16 hex>.tmp`.
fn temporary(name: &str) -> bool {
    name.strip_prefix('.')
        .and_then(|rest| rest.strip_suffix(".tmp"))
        .is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Two snapshots of one source covering the same kinds: the newer says all the older did.
fn same_snapshot(a: &Value, b: &Value) -> bool {
    let kinds = |v: &Value| {
        v.get("kinds").and_then(Value::as_array).map(|list| {
            let mut names: Vec<String> = list
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            names.sort();
            names
        })
    };
    a.get("source") == b.get("source") && kinds(a) == kinds(b)
}

impl Spool {
    /// `--spool DIR`, else `BEACON_SPOOL_DIR`; none means no spool.
    pub fn from_args(args: &Args) -> Option<Spool> {
        args.value("spool")
            .map(str::to_string)
            .or_else(|| std::env::var("BEACON_SPOOL_DIR").ok())
            .filter(|dir| !dir.is_empty())
            .map(|dir| Spool {
                dir: PathBuf::from(dir),
            })
    }

    fn problem(&self, message: String) -> Problem {
        Problem::new(
            format!("spool {}", self.dir.display()),
            message,
            "check the folder can be written by this user, or leave out --spool and BEACON_SPOOL_DIR",
        )
    }

    /// The request files, oldest first: by the time and sequence their names start with, as
    /// numbers. A name that has neither comes last.
    fn names(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".json") && !n.starts_with('.'))
            .collect();
        names.sort_by_cached_key(|name| (position(name).is_none(), position(name), name.clone()));
        names
    }

    /// The name the next request takes: after every file already there, whatever process wrote
    /// it, and never before the clock, so a clock that steps back doesn't reorder.
    fn next_name(&self, now_ms: i64) -> String {
        let last = self.names().iter().filter_map(|name| position(name)).max();
        let (mut ms, mut sequence) = match last {
            Some((ms, sequence)) if ms >= now_ms => (ms, sequence.saturating_add(1)),
            _ => (now_ms, 0),
        };
        if sequence > LAST_SEQUENCE {
            ms += 1;
            sequence = 0;
        }
        format!("{ms:015}-{sequence:06}.json")
    }

    /// Writes the request `data` (whose Idempotency-Key is `key`) so that it outlasts a power cut:
    /// under a temporary name, synced, then hard-linked to the next name, then the folder synced.
    /// Only after this returns is anything the request replaces removed. If the folder can't be
    /// synced the request is taken back, so the caller is told and what was waiting still waits.
    fn publish(&self, data: &[u8], key: &str, now_ms: i64) -> Result<(), Problem> {
        let name = match self.write_request(data, now_ms) {
            // The temporary file was gone before it had its name: another process swept it as
            // left behind (this one was paused that long, or the clock jumped). Once more.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.write_request(data, now_ms),
            written => written,
        }
        .map_err(|e| self.problem(format!("can't be written: {e}")))?;
        if let Err(e) = sync_dir(&self.dir) {
            self.drop_same(&name, Some(key));
            return Err(self.problem(format!("can't be written: the folder can't be synced: {e}")));
        }
        #[cfg(test)]
        fault::after_publish();
        Ok(())
    }

    /// Writes `data` under a temporary name and syncs it, then hard-links it to the next name,
    /// which fails if another process took that name first; then it looks again. So no two
    /// processes take one name, and no reader sees half a file. Answers the name.
    fn write_request(&self, data: &[u8], now_ms: i64) -> std::io::Result<String> {
        let temporary = self.path(&format!(".{}.tmp", random_hex(8)));
        create_file(&temporary, data)?;
        let published = self.link(&temporary, now_ms);
        #[cfg(test)]
        fault::note("remove temporary");
        let _ = std::fs::remove_file(&temporary);
        published
    }

    /// Removes temporary files a process left when it stopped mid-write: nothing lists or sends
    /// them, so they would only pile up.
    fn sweep(&self, now_ms: i64) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            if !temporary(&entry.file_name().to_string_lossy()) {
                continue;
            }
            let written = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|since| i64::try_from(since.as_millis()).ok());
            if written.is_some_and(|at| now_ms - at > STALE_TEMPORARY_MS) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    fn link(&self, temporary: &Path, now_ms: i64) -> std::io::Result<String> {
        for _ in 0..NAME_TRIES {
            let name = self.next_name(now_ms);
            let path = self.path(&name);
            #[cfg(test)]
            fault::before_publish(&path, temporary);
            match std::fs::hard_link(temporary, &path) {
                Ok(()) => {
                    #[cfg(test)]
                    fault::note("publish");
                    return Ok(name);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other(format!(
            "another process took each of {NAME_TRIES} names in a row"
        )))
    }

    /// Makes the folder, and any folder above it that is missing, for this user only (mode 700),
    /// and syncs the folder each was made in, so they outlast a power cut too. One that is
    /// already there keeps its mode.
    fn make_dir(&self) -> std::io::Result<()> {
        let mut missing = Vec::new();
        let mut dir = self.dir.as_path();
        while !dir.is_dir() {
            missing.push(dir);
            match dir.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => dir = parent,
                _ => break,
            }
        }
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        for made in missing.into_iter().rev() {
            match builder.create(made) {
                Ok(()) => {}
                // Another process made it just first.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
            let parent = made.parent().filter(|parent| !parent.as_os_str().is_empty());
            sync_dir(parent.unwrap_or(Path::new(".")))?;
        }
        Ok(())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// A spooled request, if it reads as one.
    fn read(&self, name: &str) -> Option<Value> {
        let text = std::fs::read(self.path(name)).ok()?;
        let value: Value = serde_json::from_slice(&text).ok()?;
        let fits = value.get("path").is_some_and(Value::is_string)
            && value.get("key").is_some_and(Value::is_string)
            && value.get("queued_at").is_some_and(Value::is_i64)
            && value.get("body").is_some_and(Value::is_object);
        fits.then_some(value)
    }

    /// The Idempotency-Key of the request a file holds now; none when it is gone or doesn't read
    /// as one.
    fn key_of(&self, name: &str) -> Option<String> {
        self.read(name)?.get("key")?.as_str().map(str::to_string)
    }

    /// Removes a file only while it still holds the request that was read. Another process may
    /// have sent and removed that one, and a new request taken its name (after the clock stepped
    /// back): that one stays.
    fn drop_same(&self, name: &str, key: Option<&str>) {
        if self.key_of(name).as_deref() == key {
            #[cfg(test)]
            fault::note("remove replaced");
            let _ = std::fs::remove_file(self.path(name));
        }
    }

    /// Writes a file under a temporary name, then renames it over `name`, so no reader sees half
    /// of it.
    fn write(&self, name: &str, data: &[u8]) -> std::io::Result<()> {
        let temporary = self.path(&format!(".{}.tmp", random_hex(8)));
        create_file(&temporary, data)?;
        std::fs::rename(&temporary, self.path(name)).inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary);
        })
    }

    /// Keeps the request to send later. Answers how many of the oldest were dropped to make room.
    /// A request larger than the whole spool is not kept, and drops nothing.
    pub fn queue(&self, path: &str, key: &str, body: &Value, now_ms: i64) -> Result<usize, Problem> {
        let data = json!({"path": path, "key": key, "queued_at": now_ms, "body": body}).to_string();
        let incoming = u64::try_from(data.len()).unwrap_or(u64::MAX);
        if incoming > max_bytes() {
            return Err(Problem::new(
                format!("spool {}", self.dir.display()),
                format!("can't keep it: it is {incoming} bytes"),
                format!(
                    "the request is larger than the spool's limit of {} bytes on its own; send less in it",
                    max_bytes()
                ),
            ));
        }
        self.make_dir()
            .map_err(|e| self.problem(format!("can't be made: {e}")))?;
        self.sweep(now_ms);
        // Removed only once the new file is written and synced, so a failed write or a power cut
        // loses nothing; and each by the key it held, so a new request under its name stays.
        let mut replaced: Vec<(String, Option<String>)> = Vec::new();
        let mut names = Vec::new();
        for name in self.names() {
            let same = (path == SNAPSHOTS)
                .then(|| self.read(&name))
                .flatten()
                .filter(|old| old["path"] == SNAPSHOTS && same_snapshot(&old["body"], body));
            match same {
                Some(old) => replaced.push((name, old["key"].as_str().map(str::to_string))),
                None => names.push(name),
            }
        }
        let mut files: Vec<(String, u64)> = names
            .into_iter()
            .map(|n| {
                let size = std::fs::metadata(self.path(&n)).map_or(0, |m| m.len());
                (n, size)
            })
            .collect();
        let mut total: u64 = files.iter().map(|(_, size)| size).sum();
        let mut evicted = Vec::new();
        while !files.is_empty() && (files.len() >= MAX_FILES || total.saturating_add(incoming) > max_bytes())
        {
            let (name, size) = files.remove(0);
            total -= size;
            let held = self.key_of(&name);
            evicted.push((name, held));
        }
        self.publish(data.as_bytes(), key, now_ms)?;
        for (old, held) in replaced.iter().chain(&evicted) {
            self.drop_same(old, held.as_deref());
        }
        Ok(evicted.len())
    }

    /// Past 23 hours, or a snapshot past what `sent_at` allows.
    fn expired(request: &Value, now_ms: i64) -> bool {
        let queued_at = request["queued_at"].as_i64().unwrap_or(0);
        if now_ms - queued_at > MAX_AGE_MS {
            return true;
        }
        let sent_at = request["body"].get("sent_at").and_then(Value::as_i64);
        request["path"] == SNAPSHOTS && sent_at.is_some_and(|at| now_ms - at > SNAPSHOT_MAX_AGE_MS)
    }

    /// Sends the spooled requests oldest first, once each, and stops at the first that still can't
    /// go, so order is kept. A refusal (4xx) will never be taken: it is dropped and counted.
    pub fn flush(&self, http: &Http, now_ms: i64) -> Flushed {
        // Two snapshots of one source and kinds can both be here for a moment (a newer one is written
        // before the older one it replaces is removed): sending the older, then the newer, is right.
        self.sweep(now_ms);
        // The clock goes on while a long flush runs: each request is aged as it comes up.
        let started = std::time::Instant::now();
        let now = || now_ms.saturating_add(i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX));
        let names = self.names();
        let mut done = Flushed {
            sent: 0,
            left: 0,
            dropped: 0,
            problem: None,
        };
        for (index, name) in names.iter().enumerate() {
            let Some(request) = self.read(name) else {
                if self.path(name).exists() {
                    done.dropped += 1;
                    self.drop_same(name, None);
                }
                continue;
            };
            let path = request["path"].as_str().unwrap_or_default();
            let key = request["key"].as_str().unwrap_or_default();
            if Self::expired(&request, now()) {
                done.dropped += 1;
                self.drop_same(name, Some(key));
                continue;
            }
            let answer = http.once(path, &request["body"].to_string(), key);
            #[cfg(test)]
            fault::after_send(&self.path(name));
            if answer.as_ref().map_or(true, |a| retryable(a.status)) {
                done.left = names.len() - index;
                done.problem = Some(match answer {
                    Err(why) => format!("no answer from Beacon: {why}"),
                    Ok(answer) => format!("Beacon answered {}", answer.status),
                });
                return done;
            }
            // Only while it is still this request: another flush may have sent it, and a new one
            // have its name.
            self.drop_same(name, Some(key));
            match answer {
                Ok(answer) if answer.ok() => done.sent += 1,
                _ => done.dropped += 1,
            }
        }
        done
    }

    fn kinds_file(&self, source: &str) -> PathBuf {
        self.path(&format!(".kinds-{source}.json"))
    }

    /// Keeps the declarations last read from Beacon, so a command can still check its values
    /// while Beacon is away.
    pub fn save_kinds(&self, source: &str, decl: &Value) {
        let name = format!(".kinds-{source}.json");
        let _ = self.make_dir();
        let _ = self.write(&name, decl.to_string().as_bytes());
    }

    pub fn kinds(&self, source: &str) -> Option<Value> {
        let text = std::fs::read(self.kinds_file(source)).ok()?;
        serde_json::from_slice(&text).ok()
    }
}

/// Faults the tests inject, one thread at a time, so tests running beside each other don't see them.
#[cfg(test)]
pub mod fault {
    use std::cell::{Cell, RefCell};
    use std::path::Path;

    thread_local! {
        static FAIL_WRITE: Cell<bool> = const { Cell::new(false) };
        static FAIL_PART_WAY: Cell<bool> = const { Cell::new(false) };
        static TAKE_NAME: RefCell<Option<String>> = const { RefCell::new(None) };
        static MAX_BYTES: Cell<Option<u64>> = const { Cell::new(None) };
        static STEPS: RefCell<Option<Vec<&'static str>>> = const { RefCell::new(None) };
        static AFTER_PUBLISH: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        static AFTER_SEND: RefCell<Option<String>> = const { RefCell::new(None) };
        static SWEEP_TEMPORARY: Cell<bool> = const { Cell::new(false) };
        static FOLDER_SYNC: Cell<Option<std::io::ErrorKind>> = const { Cell::new(None) };
        static SYNCED: RefCell<Option<Vec<std::path::PathBuf>>> = const { RefCell::new(None) };
    }

    /// From now on this thread's spool notes each step it takes; `steps` gives them and stops.
    pub fn record_steps() {
        STEPS.with(|steps| *steps.borrow_mut() = Some(Vec::new()));
    }

    pub fn steps() -> Vec<&'static str> {
        STEPS.with(|steps| steps.borrow_mut().take().unwrap_or_default())
    }

    pub(super) fn note(step: &'static str) {
        STEPS.with(|steps| {
            if let Some(steps) = steps.borrow_mut().as_mut() {
                steps.push(step);
            }
        });
    }

    /// The next file the spool writes on this thread gets half its bytes, then fails, as on a
    /// disk that fills while it is written.
    pub fn fail_next_write_part_way() {
        FAIL_PART_WAY.with(|fail| fail.set(true));
    }

    pub(super) fn write_fails_part_way() -> bool {
        FAIL_PART_WAY.with(|fail| fail.replace(false))
    }

    /// Runs once, after the next request file is published and before what it replaces is removed.
    pub fn after_next_publish(then: impl FnOnce() + 'static) {
        AFTER_PUBLISH.with(|after| *after.borrow_mut() = Some(Box::new(then)));
    }

    pub(super) fn after_publish() {
        if let Some(then) = AFTER_PUBLISH.with(|after| after.borrow_mut().take()) {
            then();
        }
    }

    /// After the next request a flush sends, and before it removes the file, another process has
    /// sent and removed that file and a new request, `contents`, has taken its name.
    pub fn retake_name_after_next_send(contents: String) {
        AFTER_SEND.with(|after| *after.borrow_mut() = Some(contents));
    }

    pub(super) fn after_send(path: &Path) {
        if let Some(contents) = AFTER_SEND.with(|after| after.borrow_mut().take()) {
            let _ = std::fs::write(path, contents);
        }
    }

    /// The spool's byte limit, on this thread, is `bytes`.
    pub fn limit_bytes(bytes: u64) {
        MAX_BYTES.with(|max| max.set(Some(bytes)));
    }

    pub(super) fn max_bytes() -> Option<u64> {
        MAX_BYTES.with(Cell::get)
    }

    /// Another process writes `contents` under the name the next request file is about to get,
    /// between this one choosing the name and publishing it.
    pub fn take_next_name(contents: String) {
        TAKE_NAME.with(|take| *take.borrow_mut() = Some(contents));
    }

    pub(super) fn before_publish(path: &Path, temporary: &Path) {
        let request = path
            .file_name()
            .is_some_and(|name| !name.to_string_lossy().starts_with('.'));
        if let Some(contents) = TAKE_NAME.with(|take| request.then(|| take.borrow_mut().take()).flatten()) {
            let _ = std::fs::write(path, contents);
        }
        if SWEEP_TEMPORARY.with(|sweep| sweep.replace(false)) {
            let _ = std::fs::remove_file(temporary);
        }
    }

    /// Another process, whose clock says the next temporary file is old, removes it as left
    /// behind, between this one writing it and giving it its name.
    pub fn sweep_next_temporary() {
        SWEEP_TEMPORARY.with(|sweep| sweep.set(true));
    }

    /// From now on, on this thread, syncing a folder fails as on a failing disk (`Other`), or as
    /// on a file system that doesn't sync folders (`Unsupported`). `None` ends it.
    pub fn fail_folder_sync(kind: Option<std::io::ErrorKind>) {
        FOLDER_SYNC.with(|fail| fail.set(kind));
    }

    pub(super) fn folder_sync() -> std::io::Result<()> {
        match FOLDER_SYNC.with(Cell::get) {
            None => Ok(()),
            Some(kind) => Err(std::io::Error::new(kind, "the folder can't be synced")),
        }
    }

    /// From now on this thread's spool notes each folder it syncs; `synced` gives them and stops.
    pub fn record_synced() {
        SYNCED.with(|synced| *synced.borrow_mut() = Some(Vec::new()));
    }

    pub fn synced() -> Vec<std::path::PathBuf> {
        SYNCED.with(|synced| synced.borrow_mut().take().unwrap_or_default())
    }

    pub(super) fn note_synced(dir: &Path) {
        note("sync folder");
        SYNCED.with(|synced| {
            if let Some(synced) = synced.borrow_mut().as_mut() {
                synced.push(dir.to_path_buf());
            }
        });
    }

    /// The next file the spool writes on this thread fails, as on a full disk.
    pub fn fail_next_write() {
        FAIL_WRITE.with(|fail| fail.set(true));
    }

    pub(super) fn check() -> std::io::Result<()> {
        if FAIL_WRITE.with(|fail| fail.replace(false)) {
            return Err(std::io::Error::other("no space left on device"));
        }
        Ok(())
    }
}
