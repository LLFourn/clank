//! Images are capabilities into the retained transcript, never filesystem paths.
//! Snapshots carry descriptors; only requested thumbnails are decoded/cached.
use super::{Blocking, Feed, transcript::Image};
use base64::{Engine, engine::general_purpose::STANDARD};
use clank_web::request::{BrowserReply, Content, TranscriptImage};
use std::{collections::HashMap, io::Cursor, sync::Arc};
use tokio::sync::{Mutex, Semaphore};

#[derive(Clone, serde::Serialize)]
struct Picture {
    media_type: String,
    #[serde(serialize_with = "serialize_data")]
    data: Arc<str>,
    width: u32,
    height: u32,
}
fn serialize_data<S: serde::Serializer>(data: &Arc<str>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(data)
}

pub(super) struct Images {
    previews: Mutex<HashMap<String, Picture>>,
    decoding: Arc<Semaphore>,
}
impl Default for Images {
    fn default() -> Self {
        Self {
            previews: Mutex::new(HashMap::new()),
            decoding: Arc::new(Semaphore::new(1)),
        }
    }
}
impl Images {
    pub(super) async fn get(
        &self,
        feed: &Feed,
        blocking: &Arc<Blocking>,
        ask: TranscriptImage,
    ) -> BrowserReply {
        let Some(image) = feed.image(&ask) else {
            return error(
                409,
                "This attachment is no longer in this conversation window.",
            );
        };
        if !matches!(
            image.media_type.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) {
            return error(415, "This image format cannot be displayed here.");
        }
        let Some(data) = image.data.clone() else {
            return error(
                404,
                "The transcript did not retain this image. Reattach it to view it.",
            );
        };
        let picture = if ask.preview {
            // The owned permit stays with the blocking worker even when the
            // browser cancels, so simultaneous viewers never decode in parallel.
            let permit = self.decoding.clone().acquire_owned().await.unwrap();
            let mut previews = self.previews.lock().await;
            let keys = feed.image_keys();
            previews.retain(|key, _| keys.contains(key));
            if feed.image(&ask).is_none() {
                return error(
                    409,
                    "This attachment has changed. Reopen it from the timeline.",
                );
            }
            if let Some(cached) = previews.get(&ask.key) {
                cached.clone()
            } else {
                match blocking
                    .run(move || {
                        let _permit = permit;
                        thumbnail(&image)
                    })
                    .await
                {
                    Some(Ok(picture)) => {
                        previews.insert(ask.key.clone(), picture.clone());
                        picture
                    }
                    Some(Err(_)) => {
                        return error(
                            422,
                            "The thumbnail could not be decoded. You can still open the original image.",
                        );
                    }
                    None => return error(500, "Image preview failed. Try again."),
                }
            }
        } else {
            Picture {
                media_type: image.media_type,
                data,
                width: 0,
                height: 0,
            }
        };
        // An agent rebinding while decoding must not receive its predecessor's
        // pixels under the new window's authority.
        if feed.image(&ask).is_none() {
            return error(
                409,
                "This attachment has changed. Reopen it from the timeline.",
            );
        }
        BrowserReply {
            status: 200,
            content: Content::Json,
            body: serde_json::to_string(&picture).unwrap(),
        }
    }
}

fn thumbnail(image: &Image) -> anyhow::Result<Picture> {
    let bytes = STANDARD.decode(image.data.as_deref().unwrap_or_default())?;
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    // There is no image-size acceptance cutoff. Decode on demand, one at a
    // time; only the small presentation image is cached, originals stay intact.
    reader.no_limits();
    let decoded = reader.decode()?;
    let small = decoded.thumbnail(640, 400);
    let (width, height) = (small.width(), small.height());
    let mut png = Cursor::new(Vec::new());
    small.write_to(&mut png, image::ImageFormat::Png)?;
    Ok(Picture {
        media_type: "image/png".into(),
        data: STANDARD.encode(png.into_inner()).into(),
        width,
        height,
    })
}
fn error(status: u16, message: &str) -> BrowserReply {
    BrowserReply {
        status,
        content: Content::Plain,
        body: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::web::transcript::{Body, Turn, Who};

    fn original() -> Image {
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(40, 1200)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        Image::new("image/png", &STANDARD.encode(png.into_inner()))
    }
    fn retained(image: Image) -> (Feed, TranscriptImage) {
        let ask = TranscriptImage {
            agent: "codex".into(),
            session: "s1".into(),
            generation: 1,
            turn: "image-1".into(),
            index: 0,
            key: image.key.clone(),
            preview: true,
        };
        let feed = Feed::new();
        feed.claim_turns("codex", 1, Some("s1"));
        feed.turns_reset(
            "codex",
            1,
            "s1",
            1,
            vec![Turn {
                id: "image-1".into(),
                at: None,
                who: Who::Person,
                body: Body::Image { image },
            }],
        );
        (feed, ask)
    }
    #[tokio::test]
    async fn thumbnail_and_original_are_authorized_by_retained_window_and_content() {
        let original = original();
        let (feed, ask) = retained(original.clone());
        let images = Images::default();
        let blocking = Arc::new(Blocking::default());
        let preview = images.get(&feed, &blocking, ask.clone()).await;
        assert_eq!(preview.status, 200, "{}", preview.body);
        let value: serde_json::Value = serde_json::from_str(&preview.body).unwrap();
        assert_eq!(value["height"], 400);
        assert_eq!(value["width"], 13);
        assert_ne!(value["data"].as_str(), original.data.as_deref());
        assert_eq!(images.previews.lock().await.len(), 1);
        let full = images
            .get(
                &feed,
                &blocking,
                TranscriptImage {
                    preview: false,
                    ..ask.clone()
                },
            )
            .await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&full.body).unwrap()["data"].as_str(),
            original.data.as_deref()
        );
        for invalid in [
            TranscriptImage {
                session: "old".into(),
                ..ask.clone()
            },
            TranscriptImage {
                generation: 2,
                ..ask.clone()
            },
            TranscriptImage {
                index: 1,
                ..ask.clone()
            },
            TranscriptImage {
                key: "same-size-different-pixels".into(),
                ..ask.clone()
            },
            TranscriptImage {
                turn: "/private/file.png".into(),
                ..ask.clone()
            },
        ] {
            assert_eq!(images.get(&feed, &blocking, invalid).await.status, 409);
        }
        feed.claim_turns("codex", 2, Some("s2"));
        assert_eq!(images.get(&feed, &blocking, ask).await.status, 409);
        blocking.join_all().await;
    }
    #[tokio::test]
    async fn rebind_during_decode_refuses_the_late_result() {
        let (feed, ask) = retained(original());
        let images = Images::default();
        let blocking = Arc::new(Blocking::default());
        let permit = images.decoding.clone().acquire_owned().await.unwrap();
        let request = images.get(&feed, &blocking, ask);
        tokio::pin!(request);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut request)
                .await
                .is_err()
        );
        feed.claim_turns("codex", 2, Some("new-session"));
        drop(permit);
        assert_eq!(request.await.status, 409);
        assert!(images.previews.lock().await.is_empty());
    }
}
