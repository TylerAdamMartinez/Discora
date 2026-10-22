//! Poster I/O and image encoding stay off the terminal event thread.
use std::{
    collections::HashMap,
    io::{Cursor, Read},
    sync::mpsc,
    thread,
    time::Duration,
};

use image::DynamicImage;
use ratatui::{
    Frame,
    layout::{Rect, Size},
    widgets::Paragraph,
};
use ratatui_image::{Image, Resize, picker::Picker, protocol::Protocol};
use reqwest::blocking::Client;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Request {
    generation: u64,
    path: String,
    size: Size,
}
struct Response {
    request: Request,
    result: Result<Protocol, String>,
}

#[derive(Default)]
pub struct Preview {
    generation: u64,
    tx: Option<mpsc::SyncSender<Request>>,
    rx: Option<mpsc::Receiver<Response>>,
    requested: Option<Request>,
    size: Size,
    cache: HashMap<String, Result<Protocol, String>>,
}

impl Preview {
    pub fn start(&mut self, picker: Picker) {
        self.start_with_fetch(picker, {
            let mut client = None;
            move |path| {
                let client = client.get_or_insert_with(|| {
                    Client::builder().timeout(Duration::from_secs(5)).build()
                });
                client
                    .as_ref()
                    .map_err(|e| e.to_string())
                    .and_then(|client| download(client, path))
            }
        });
    }

    fn start_with_fetch(
        &mut self,
        picker: Picker,
        mut fetch: impl FnMut(&str) -> Result<DynamicImage, String> + Send + 'static,
    ) {
        let (tx, requests) = mpsc::sync_channel::<Request>(1);
        let (results, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut generation = 0;
            let mut images = HashMap::<String, Result<DynamicImage, String>>::new();
            while let Ok(mut request) = requests.recv() {
                // Skip requests superseded while the previous image was loading.
                while let Ok(newer) = requests.try_recv() {
                    request = newer;
                }
                if generation != request.generation {
                    generation = request.generation;
                    images.clear();
                }
                let image = images
                    .entry(request.path.clone())
                    .or_insert_with(|| fetch(&request.path));
                let result = match image {
                    Ok(image) => picker
                        .new_protocol(image.clone(), request.size, Resize::Fit(None))
                        .map_err(|e| e.to_string()),
                    Err(error) => Err(error.clone()),
                };
                if results.send(Response { request, result }).is_err() {
                    break;
                }
            }
        });
        self.tx = Some(tx);
        self.rx = Some(rx);
    }

    pub fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.cache.clear();
        self.requested = None;
    }

    fn receive(&mut self) {
        if let Some(rx) = &self.rx {
            while let Ok(response) = rx.try_recv() {
                if self.requested.as_ref() == Some(&response.request) {
                    self.requested = None;
                }
                if response.request.generation == self.generation
                    && response.request.size == self.size
                {
                    self.cache.insert(response.request.path, response.result);
                }
            }
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, path: Option<&str>) {
        let size = Size::new(area.width, area.height);
        if size != self.size {
            self.size = size;
            self.cache.clear();
        }
        self.receive();
        let Some(path) = path else {
            frame.render_widget(Paragraph::new("No poster available"), area);
            return;
        };
        if area.is_empty() {
            return;
        }
        if let Some(result) = self.cache.get(path) {
            match result {
                Ok(protocol) => frame.render_widget(Image::new(protocol), area),
                Err(_) => frame.render_widget(Paragraph::new("Poster unavailable"), area),
            }
            return;
        }
        let request = Request {
            generation: self.generation,
            path: path.to_owned(),
            size,
        };
        if self.requested.as_ref() != Some(&request)
            && let Some(tx) = &self.tx
            && tx.try_send(request.clone()).is_ok()
        {
            self.requested = Some(request);
        }
        frame.render_widget(Paragraph::new("Loading poster…"), area);
    }
}

fn download(client: &Client, path: &str) -> Result<DynamicImage, String> {
    // Images are fetched from the public CDN, without the TMDB bearer token.
    let response = client
        .get(format!("https://image.tmdb.org/t/p/w342{path}"))
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    response
        .take(5 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 5 * 1024 * 1024 {
        return Err("Poster exceeds download limit".into());
    }
    decode(&bytes)
}

fn decode(bytes: &[u8]) -> Result<DynamicImage, String> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    reader.limits(limits);
    reader.decode().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn draw(preview: &mut Preview, path: Option<&str>, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 10)).unwrap();
        terminal.draw(|f| preview.draw(f, f.area(), path)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn stale_searches_and_sizes_cannot_replace_current_posters() {
        let (tx, rx) = mpsc::channel();
        let mut preview = Preview {
            rx: Some(rx),
            size: Size::new(30, 10),
            ..Preview::default()
        };
        let old = Request {
            generation: 0,
            path: "/old.jpg".into(),
            size: preview.size,
        };
        preview.reset();
        tx.send(Response {
            request: old,
            result: Err("old error".into()),
        })
        .unwrap();
        tx.send(Response {
            request: Request {
                generation: 1,
                path: "/wrong-size.jpg".into(),
                size: Size::new(20, 10),
            },
            result: Err("error".into()),
        })
        .unwrap();
        tx.send(Response {
            request: Request {
                generation: 1,
                path: "/current.jpg".into(),
                size: preview.size,
            },
            result: Err("offline".into()),
        })
        .unwrap();
        preview.receive();
        assert_eq!(preview.cache.len(), 1);
        assert!(draw(&mut preview, Some("/current.jpg"), 30).contains("Poster unavailable"));
        assert!(draw(&mut preview, None, 30).contains("No poster available"));
        assert!(draw(&mut preview, Some("/other.jpg"), 30).contains("Loading poster"));
    }

    #[test]
    fn delayed_previous_selection_does_not_replace_highlighted_poster() {
        let (tx, rx) = mpsc::channel();
        let mut preview = Preview {
            rx: Some(rx),
            ..Preview::default()
        };
        assert!(draw(&mut preview, Some("/a.jpg"), 30).contains("Loading poster"));
        assert!(draw(&mut preview, Some("/b.jpg"), 30).contains("Loading poster"));
        let protocol = Picker::halfblocks()
            .new_protocol(
                DynamicImage::new_rgb8(20, 30),
                preview.size,
                Resize::Fit(None),
            )
            .unwrap();
        tx.send(Response {
            request: Request {
                generation: 0,
                path: "/a.jpg".into(),
                size: preview.size,
            },
            result: Ok(protocol),
        })
        .unwrap();
        // A completed after navigation to B; B must still show its loading state.
        assert!(draw(&mut preview, Some("/b.jpg"), 30).contains("Loading poster"));
        // Navigating back to A uses its ready image without another download.
        assert!(!draw(&mut preview, Some("/a.jpg"), 30).contains("Loading poster"));
        assert!(preview.cache.contains_key("/a.jpg"));
    }

    #[test]
    fn worker_reuses_downloads_for_resizing_and_failures_until_a_new_search() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let mut preview = Preview::default();
        preview.start_with_fetch(Picker::halfblocks(), move |path| {
            counter.fetch_add(1, Ordering::Relaxed);
            if path == "/missing.jpg" {
                Err("404".into())
            } else {
                Ok(DynamicImage::new_rgb8(20, 30))
            }
        });
        let tx = preview.tx.as_ref().unwrap();
        let rx = preview.rx.as_ref().unwrap();
        for (generation, path, width, expected_calls) in [
            (0, "/a.jpg", 30, 1),
            (0, "/a.jpg", 40, 1),
            (0, "/missing.jpg", 30, 2),
            (0, "/missing.jpg", 40, 2),
            (1, "/a.jpg", 30, 3),
        ] {
            tx.send(Request {
                generation,
                path: path.into(),
                size: Size::new(width, 10),
            })
            .unwrap();
            let response = rx.recv_timeout(Duration::from_secs(3)).unwrap();
            assert_eq!(response.result.is_err(), path == "/missing.jpg");
            assert_eq!(calls.load(Ordering::Relaxed), expected_calls);
        }
    }

    #[test]
    fn decoding_rejects_corrupt_images_and_accepts_png() {
        assert!(decode(b"broken image").is_err());
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(10, 15)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let image = decode(bytes.get_ref()).unwrap();
        assert_eq!((image.width(), image.height()), (10, 15));
    }
}
