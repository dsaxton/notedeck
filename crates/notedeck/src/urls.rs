use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};

use egui::TextBuffer;
use poll_promise::Promise;

use crate::Error;

const FILE_NAME: &str = "urls.bin";
const SAVE_INTERVAL: Duration = Duration::from_secs(60);

type UrlsToMime = HashMap<String, String>;

/// caches mime type for a URL. saves to disk on interval [`SAVE_INTERVAL`]
pub struct UrlCache {
    last_saved: SystemTime,
    path: PathBuf,
    cache: Arc<RwLock<UrlsToMime>>,
    from_disk_promise: Option<Promise<Option<UrlsToMime>>>,
}

impl UrlCache {
    pub fn rel_dir() -> &'static str {
        FILE_NAME
    }

    pub fn new(path: PathBuf) -> Self {
        Self {
            last_saved: SystemTime::now(),
            path: path.clone(),
            cache: Default::default(),
            from_disk_promise: Some(read_from_disk(path)),
        }
    }

    pub fn get_type(&self, url: &str) -> Option<String> {
        self.cache.read().ok()?.get(url).cloned()
    }

    pub fn set_type(&mut self, url: String, mime_type: String) {
        if let Ok(mut locked_cache) = self.cache.write() {
            locked_cache.insert(url, mime_type);
        }
    }

    pub fn handle_io(&mut self) {
        if let Some(promise) = &mut self.from_disk_promise {
            if let Some(maybe_cache) = promise.ready_mut() {
                if let Some(cache) = maybe_cache.take() {
                    merge_cache(self.cache.clone(), cache)
                }

                self.from_disk_promise = None;
            }
        }

        if let Ok(cur_duration) = SystemTime::now().duration_since(self.last_saved) {
            if cur_duration >= SAVE_INTERVAL {
                save_to_disk(self.path.clone(), self.cache.clone());
                self.last_saved = SystemTime::now();
            }
        }
    }
}

fn merge_cache(cur_cache: Arc<RwLock<UrlsToMime>>, from_disk: UrlsToMime) {
    std::thread::spawn(move || {
        if let Ok(mut locked_cache) = cur_cache.write() {
            locked_cache.extend(from_disk);
        }
    });
}

fn read_from_disk(path: PathBuf) -> Promise<Option<UrlsToMime>> {
    let (sender, promise) = Promise::new();

    std::thread::spawn(move || {
        let result: Result<UrlsToMime, Error> = (|| {
            let mut file = File::open(path)?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)?;
            let data: UrlsToMime =
                bincode::deserialize(&buffer).map_err(|e| Error::Generic(e.to_string()))?;
            Ok(data)
        })();

        match result {
            Ok(data) => sender.send(Some(data)),
            Err(e) => {
                tracing::error!("problem deserializing UrlCache: {e}");
                sender.send(None)
            }
        }
    });

    promise
}

fn save_to_disk(path: PathBuf, cache: Arc<RwLock<UrlsToMime>>) {
    std::thread::spawn(move || {
        let result: Result<(), Error> = (|| {
            if let Ok(cache) = cache.read() {
                let cache = &*cache;
                let encoded =
                    bincode::serialize(cache).map_err(|e| Error::Generic(e.to_string()))?;
                let mut file = File::create(&path)?;
                file.write_all(&encoded)?;
                file.sync_all()?;
                tracing::info!("Saved UrlCache to disk.");
                Ok(())
            } else {
                Err(Error::Generic(
                    "Could not read UrlCache behind RwLock".to_owned(),
                ))
            }
        })();

        if let Err(e) = result {
            tracing::error!("Failed to save UrlCache: {}", e);
        }
    });
}

fn ehttp_get_mime_type(url: &str, sender: poll_promise::Sender<MimeResult>) {
    let request = ehttp::Request::head(url);

    let url = url.to_owned();
    ehttp::fetch(
        request,
        move |response: Result<ehttp::Response, String>| match response {
            Ok(resp) => {
                if let Some(content_type) = resp.headers.get("content-type") {
                    sender.send(MimeResult::Ok(extract_mime_type(content_type).to_owned()));
                } else {
                    sender.send(MimeResult::Err(HttpError::MissingHeader));
                    tracing::error!("Content-Type header not found for {url}");
                }
            }
            Err(err) => {
                sender.send(MimeResult::Err(HttpError::HttpFailure));
                tracing::error!("failed ehttp for UrlCache: {err}");
            }
        },
    );
}

#[derive(Debug)]
enum HttpError {
    HttpFailure,
    MissingHeader,
}

type MimeResult = Result<String, HttpError>;

fn extract_mime_type(content_type: &str) -> &str {
    content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
}

pub struct UrlMimes {
    pub cache: UrlCache,
    in_flight: HashMap<String, Promise<MimeResult>>,
}

impl UrlMimes {
    pub fn new(url_cache: UrlCache) -> Self {
        Self {
            cache: url_cache,
            in_flight: Default::default(),
        }
    }

    pub fn get(&mut self, url: &str) -> Option<String> {
        if let Some(mime_type) = self.cache.get_type(url) {
            Some(mime_type)
        } else if let Some(promise) = self.in_flight.get_mut(url) {
            if let Some(mime_result) = promise.ready_mut() {
                match mime_result {
                    Ok(mime_type) => {
                        let mime_type = mime_type.take();
                        self.cache.set_type(url.to_owned(), mime_type.clone());
                        self.in_flight.remove(url);
                        Some(mime_type)
                    }
                    Err(HttpError::HttpFailure) => {
                        // allow retrying
                        self.in_flight.remove(url);
                        None
                    }
                    Err(HttpError::MissingHeader) => {
                        // response was malformed, don't retry
                        None
                    }
                }
            } else {
                None
            }
        } else {
            let (sender, promise) = Promise::new();
            ehttp_get_mime_type(url, sender);
            self.in_flight.insert(url.to_owned(), promise);
            None
        }
    }
}
