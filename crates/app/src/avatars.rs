//! Account avatars: an in-memory LRU of decoded images over a disk cache
//! (`<data dir>/avatars/<sha256 of url>.png`), filled from the forge hub.
//!
//! The UI only calls [`Avatars::image`] while drawing: it answers from
//! memory at once and, on a miss, queues the URL (once, however many rows
//! ask) for a driver task that reads the disk, then the network, off the
//! main thread. When images arrive, the windows are redrawn once. Rows
//! outside the viewport are never drawn, so they never ask.
//!
//! Which URLs may be fetched is decided in `forge::avatar`; the markdown
//! policy in [`crate::untrusted`] (no images in forge text) is separate and
//! unchanged.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use forge::{Avatar, ForgeHub};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AsyncApp, Global, RenderImage};
use image::{Frame, RgbaImage};
use sha2::{Digest as _, Sha256};

/// A cached avatar older than this is shown, then fetched again.
pub const REFETCH_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// After a failed refetch the old image stays and the next try is this much later.
const RETRY_REFETCH: Duration = Duration::from_secs(60 * 60);
/// A URL that could not be loaded at all is not asked for again for this long.
const RETRY_FAILED: Duration = Duration::from_secs(5 * 60);
/// Decoded images kept in memory.
const MEMORY_CAPACITY: usize = 256;
/// Newest cache files read at launch, so rows can show avatars on the first frame.
const PRELOAD_LIMIT: usize = 128;
/// Arrivals within this window cause one redraw.
const REDRAW_DELAY: Duration = Duration::from_millis(30);
/// Largest cache file read.
const MAX_FILE: u64 = forge::avatar::MAX_BYTES as u64;

/// URLs of the form `bundled:N` name an image built into the binary; only a
/// fixtures run honors them.
const BUNDLED: &str = "bundled:";
const BUNDLED_PNGS: [&[u8]; 6] = [
    include_bytes!("../assets/avatars/0.png"),
    include_bytes!("../assets/avatars/1.png"),
    include_bytes!("../assets/avatars/2.png"),
    include_bytes!("../assets/avatars/3.png"),
    include_bytes!("../assets/avatars/4.png"),
    include_bytes!("../assets/avatars/5.png"),
];

/// The URL of the built-in avatar for `login`, for fixture users.
#[must_use]
pub fn bundled_url(login: &str) -> String {
    let hash = login.bytes().fold(0usize, |h, b| {
        h.wrapping_mul(31).wrapping_add(usize::from(b))
    });
    format!("{BUNDLED}{}", hash % BUNDLED_PNGS.len())
}

/// A least-recently-used map. Eviction scans, which is cheap at this size.
struct Lru<V> {
    map: HashMap<String, (V, u64)>,
    clock: u64,
    capacity: usize,
}

impl<V> Lru<V> {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            clock: 0,
            capacity,
        }
    }

    fn get(&mut self, key: &str) -> Option<&V> {
        self.clock += 1;
        let now = self.clock;
        self.map.get_mut(key).map(|(value, used)| {
            *used = now;
            &*value
        })
    }

    fn insert(&mut self, key: String, value: V) {
        self.clock += 1;
        self.map.insert(key, (value, self.clock));
        if self.map.len() > self.capacity
            && let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key.clone())
        {
            self.map.remove(&oldest);
        }
    }

    fn retain(&mut self, mut keep: impl FnMut(&V) -> bool) {
        self.map.retain(|_, (value, _)| keep(value));
    }
}

#[derive(Clone)]
enum Slot {
    Ready {
        image: Arc<RenderImage>,
        /// When the pixels were last fetched or confirmed current.
        fetched: SystemTime,
    },
    Failed(Instant),
}

struct Want {
    host: String,
    url: String,
    /// Fetch again even though an image is already shown.
    refetch: bool,
}

#[derive(Clone)]
struct Backend {
    /// The cache directory; `None` for a run that keeps nothing on disk.
    dir: Option<PathBuf>,
    hub: Option<Arc<ForgeHub>>,
    fixtures: bool,
}

struct State {
    memory: Lru<Slot>,
    /// URLs that are queued or loading, so each is asked for once.
    in_flight: HashSet<String>,
    wanted: Vec<Want>,
    /// Images read at launch, by the hash of their URL.
    preloaded: HashMap<String, (Arc<RenderImage>, SystemTime)>,
    backend: Option<Backend>,
    gravatar: bool,
    redraw_scheduled: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: mpsc::UnboundedSender<()>,
}

/// The app's avatar store; a cheap handle, kept as a gpui global.
#[derive(Clone)]
pub struct Avatars {
    shared: Arc<Shared>,
}

impl Global for Avatars {}

impl Avatars {
    /// Creates the store and starts its driver. Requests wait until
    /// [`Self::attach`] or [`Self::attach_fixtures`].
    pub fn install(cx: &mut App) -> Self {
        let (wake, mut woken) = mpsc::unbounded();
        let avatars = Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    memory: Lru::new(MEMORY_CAPACITY),
                    in_flight: HashSet::new(),
                    wanted: Vec::new(),
                    preloaded: HashMap::new(),
                    backend: None,
                    gravatar: false,
                    redraw_scheduled: false,
                }),
                wake,
            }),
        };
        cx.set_global(avatars.clone());
        let driver = avatars.clone();
        cx.spawn(async move |cx| {
            while woken.next().await.is_some() {
                // Many wakes for one batch of requests are one dispatch.
                while woken.try_recv().is_ok() {}
                driver.dispatch(cx);
            }
        })
        .detach();
        avatars
    }

    /// Connects the store to the forge hub and the cache directory, with
    /// the images [`preload`] read.
    pub fn attach(&self, dir: PathBuf, hub: Option<Arc<ForgeHub>>, preloaded: Preloaded) {
        let mut state = self.lock();
        if let Some(hub) = &hub {
            hub.set_allow_gravatar(state.gravatar);
        }
        for (hash, avatar, fetched) in preloaded.0 {
            if let Some(image) = render_image(&avatar) {
                state.preloaded.insert(hash, (image, fetched));
            }
        }
        state.backend = Some(Backend {
            dir: Some(dir),
            hub,
            fixtures: false,
        });
        drop(state);
        self.wake();
    }

    /// For a fixtures run: only the built-in images load, nothing touches
    /// the disk or the network.
    pub fn attach_fixtures(&self) {
        self.lock().backend = Some(Backend {
            dir: None,
            hub: None,
            fixtures: true,
        });
        self.wake();
    }

    /// Whether Gravatar images may load. Turning it on retries the URLs
    /// that were refused.
    pub fn set_gravatar(&self, allow: bool, cx: &mut App) {
        let mut state = self.lock();
        if state.gravatar == allow {
            return;
        }
        state.gravatar = allow;
        if let Some(hub) = state.backend.as_ref().and_then(|b| b.hub.as_ref()) {
            hub.set_allow_gravatar(allow);
        }
        state.memory.retain(|slot| !matches!(slot, Slot::Failed(_)));
        drop(state);
        cx.refresh_windows();
    }

    /// The decoded image for `url`, if it is in memory. Otherwise queues
    /// it (once) and returns `None`: the caller draws its placeholder and
    /// is redrawn when the image arrives. Never blocks on disk or network.
    ///
    /// `host` is the forge the account belongs to.
    pub fn image(&self, host: &str, url: &str) -> Option<Arc<RenderImage>> {
        let mut state = self.lock();
        match state.memory.get(url).cloned() {
            Some(Slot::Ready { image, fetched }) => {
                if is_stale(fetched) && state.queue(host, url, true) {
                    drop(state);
                    self.wake();
                }
                return Some(image);
            }
            Some(Slot::Failed(at)) if at.elapsed() < RETRY_FAILED => return None,
            Some(Slot::Failed(_)) | None => {}
        }
        if state.in_flight.contains(url) {
            return None;
        }
        if !state.preloaded.is_empty()
            && let Some((image, fetched)) = state.preloaded.remove(&url_hash(url))
        {
            state.memory.insert(
                url.to_owned(),
                Slot::Ready {
                    image: image.clone(),
                    fetched,
                },
            );
            if is_stale(fetched) && state.queue(host, url, true) {
                drop(state);
                self.wake();
            }
            return Some(image);
        }
        state.queue(host, url, false);
        drop(state);
        self.wake();
        None
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn wake(&self) {
        let _ = self.shared.wake.unbounded_send(());
    }

    /// Starts a load for each queued URL, once there is a backend.
    fn dispatch(&self, cx: &mut AsyncApp) {
        let (wants, backend) = {
            let mut state = self.lock();
            let Some(backend) = state.backend.clone() else {
                return;
            };
            (std::mem::take(&mut state.wanted), backend)
        };
        for want in wants {
            let (this, backend) = (self.clone(), backend.clone());
            cx.spawn(async move |cx| {
                let url = want.url.clone();
                this.load(&want, &backend, cx).await;
                this.lock().in_flight.remove(&url);
            })
            .detach();
        }
    }

    async fn load(&self, want: &Want, backend: &Backend, cx: &mut AsyncApp) {
        let executor = cx.background_executor().clone();
        if backend.fixtures {
            let decoded = executor.spawn({
                let url = want.url.clone();
                async move { bundled(&url) }
            });
            match decoded.await {
                Some(avatar) => self.store(&want.url, &avatar, SystemTime::now(), cx),
                None => self.fail(&want.url, false),
            }
            return;
        }
        let path = backend.dir.as_deref().map(|dir| cache_file(dir, &want.url));
        let mut shown = want.refetch;
        if !want.refetch
            && let Some(path) = path.clone()
            && let Some((avatar, modified)) =
                executor.spawn(async move { read_cached(&path) }).await
        {
            self.store(&want.url, &avatar, modified, cx);
            shown = true;
            if !is_stale(modified) {
                return;
            }
        }
        let Some(hub) = &backend.hub else {
            self.fail(&want.url, shown);
            return;
        };
        match hub.avatar(&want.host, &want.url).await {
            Ok(avatar) => {
                if let Some(path) = path {
                    let to_write = avatar.clone();
                    executor
                        .spawn(async move {
                            // A cache that cannot be written only costs a refetch.
                            let _ = write_cached(&path, &to_write);
                        })
                        .detach();
                }
                self.store(&want.url, &avatar, SystemTime::now(), cx);
            }
            Err(_) => self.fail(&want.url, shown),
        }
    }

    fn store(&self, url: &str, avatar: &Avatar, fetched: SystemTime, cx: &mut AsyncApp) {
        let Some(image) = render_image(avatar) else {
            self.fail(url, false);
            return;
        };
        let schedule = {
            let mut state = self.lock();
            state
                .memory
                .insert(url.to_owned(), Slot::Ready { image, fetched });
            !std::mem::replace(&mut state.redraw_scheduled, true)
        };
        if schedule {
            let this = self.clone();
            cx.spawn(async move |cx| {
                cx.background_executor().timer(REDRAW_DELAY).await;
                this.lock().redraw_scheduled = false;
                cx.update(App::refresh_windows);
            })
            .detach();
        }
    }

    /// Records a load that did not produce an image. An image already shown
    /// stays, and its refetch waits a while.
    fn fail(&self, url: &str, shown: bool) {
        let mut state = self.lock();
        let kept = match state.memory.get(url).cloned() {
            Some(Slot::Ready { image, .. }) if shown => Some(image),
            _ => None,
        };
        let slot = match kept {
            Some(image) => Slot::Ready {
                image,
                fetched: SystemTime::now() - REFETCH_AFTER + RETRY_REFETCH,
            },
            None => Slot::Failed(Instant::now()),
        };
        state.memory.insert(url.to_owned(), slot);
    }
}

impl State {
    /// Queues `url` unless it is already queued or loading.
    fn queue(&mut self, host: &str, url: &str, refetch: bool) -> bool {
        if !self.in_flight.insert(url.to_owned()) {
            return false;
        }
        self.wanted.push(Want {
            host: host.to_owned(),
            url: url.to_owned(),
            refetch,
        });
        true
    }
}

fn is_stale(fetched: SystemTime) -> bool {
    SystemTime::now()
        .duration_since(fetched)
        .is_ok_and(|age| age > REFETCH_AFTER)
}

fn url_hash(url: &str) -> String {
    Sha256::digest(url.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn cache_file(dir: &Path, url: &str) -> PathBuf {
    dir.join(format!("{}.png", url_hash(url)))
}

/// The cache directory under the app's data dir.
#[must_use]
pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("avatars")
}

fn bundled(url: &str) -> Option<Avatar> {
    let index: usize = url.strip_prefix(BUNDLED)?.parse().ok()?;
    Avatar::decode(BUNDLED_PNGS.get(index)?).ok()
}

/// A cache file's image and its age; `None` if it is missing or not an image.
fn read_cached(path: &Path) -> Option<(Avatar, SystemTime)> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE {
        return None;
    }
    let avatar = Avatar::decode(&fs::read(path).ok()?).ok()?;
    Some((avatar, meta.modified().ok()?))
}

fn write_cached(path: &Path, avatar: &Avatar) -> io::Result<()> {
    let png = avatar
        .to_png()
        .map_err(|error| io::Error::other(error.to_string()))?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("png.tmp");
    fs::write(&tmp, png)?;
    fs::rename(&tmp, path)
}

/// Images read from the cache directory before the app starts.
#[derive(Default)]
pub struct Preloaded(Vec<(String, Avatar, SystemTime)>);

/// Reads the newest cached avatars. Blocks on the disk: call it on the
/// boot thread, never the UI thread.
#[must_use]
pub fn preload(dir: &Path) -> Preloaded {
    let Ok(entries) = fs::read_dir(dir) else {
        return Preloaded::default();
    };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "png"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files.truncate(PRELOAD_LIMIT);
    Preloaded(
        files
            .into_iter()
            .filter_map(|(_, path)| {
                let hash = path.file_stem()?.to_str()?.to_owned();
                let (avatar, modified) = read_cached(&path)?;
                Some((hash, avatar, modified))
            })
            .collect(),
    )
}

/// The avatar as a gpui image (which wants BGRA).
fn render_image(avatar: &Avatar) -> Option<Arc<RenderImage>> {
    let mut bytes = avatar.rgba.to_vec();
    for pixel in bytes.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let buffer = RgbaImage::from_raw(avatar.width, avatar.height, bytes)?;
    Some(Arc::new(RenderImage::new(vec![Frame::new(buffer)])))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the workspace's git-ignored `.scratch`.
    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.scratch")
            .join(format!("avatars-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn lru_evicts_the_least_recently_used() {
        let mut lru = Lru::new(2);
        lru.insert("a".into(), 1);
        lru.insert("b".into(), 2);
        assert_eq!(lru.get("a"), Some(&1));
        lru.insert("c".into(), 3);
        assert_eq!(lru.get("b"), None);
        assert_eq!(lru.get("a"), Some(&1));
        assert_eq!(lru.get("c"), Some(&3));
    }

    #[test]
    fn the_cache_name_is_the_sha256_of_the_url() {
        // sha256("abc")
        assert_eq!(
            url_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let file = cache_file(Path::new("d"), "abc");
        assert!(
            file.ends_with("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad.png")
        );
    }

    #[test]
    fn age_decides_when_to_refetch() {
        let now = SystemTime::now();
        assert!(!is_stale(now));
        assert!(!is_stale(now - REFETCH_AFTER + Duration::from_secs(60)));
        assert!(is_stale(now - REFETCH_AFTER - Duration::from_secs(60)));
    }

    #[test]
    fn every_bundled_image_decodes() {
        for index in 0..BUNDLED_PNGS.len() {
            let avatar = bundled(&format!("{BUNDLED}{index}")).unwrap();
            assert_eq!((avatar.width, avatar.height), (64, 64));
        }
        assert!(bundled("bundled:99").is_none());
        assert!(bundled("https://example.org/a.png").is_none());
        assert!(bundled(&bundled_url("mira")).is_some());
    }

    #[test]
    fn cache_files_round_trip_and_preload_newest_first() {
        let dir = scratch("cache");
        let avatar = bundled("bundled:0").unwrap();
        let path = cache_file(&dir, "https://example.org/a");
        write_cached(&path, &avatar).unwrap();
        let (read, modified) = read_cached(&path).unwrap();
        assert_eq!(read, avatar);
        assert!(!is_stale(modified));
        fs::write(dir.join("junk.png"), b"not an image").unwrap();
        let preloaded = preload(&dir);
        assert_eq!(preloaded.0.len(), 1);
        assert_eq!(preloaded.0[0].0, url_hash("https://example.org/a"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_oversized_cache_file_is_ignored() {
        let dir = scratch("big");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.png");
        fs::write(&path, vec![0u8; usize::try_from(MAX_FILE).unwrap() + 1]).unwrap();
        assert!(read_cached(&path).is_none());
        fs::remove_dir_all(&dir).unwrap();
    }
}
