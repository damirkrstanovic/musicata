// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::{
    io::Cursor,
    path::Path,
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
    time::{Duration, SystemTime},
};

async fn wait_for(barrier: Arc<std::sync::Barrier>) {
    tokio::task::spawn_blocking(move || barrier.wait())
        .await
        .expect("barrier waiter");
}

async fn receive<T: Send + 'static>(receiver: Receiver<T>) -> T {
    tokio::task::spawn_blocking(move || receiver.recv().expect("blocking job completion"))
        .await
        .expect("completion waiter")
}

fn test_cache() -> Arc<artwork::ArtworkCache> {
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    Arc::new(artwork::ArtworkCache::new(
        std::env::temp_dir().join(format!("musicata-cancel-{unique}")),
    ))
}

async fn assert_cancelled_job_keeps_permit<T: Send + 'static>(
    cache: Arc<artwork::ArtworkCache>,
    run: impl FnOnce(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>, mpsc::SyncSender<T>)
    + Send
    + 'static,
) -> T {
    // Occupy the other two slots. If aborting the awaiting request dropped the running job's
    // permit, another decoder could start before the blocked native work has stopped.
    let held = [
        cache.processing_permit().await,
        cache.processing_permit().await,
    ];
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let (done, completed) = mpsc::sync_channel(1);
    let permit = cache.processing_permit().await;
    let task = tokio::spawn({
        let entered = entered.clone();
        let release = release.clone();
        async move { artwork_blocking(permit, move || run(entered, release, done)).await }
    });

    wait_for(entered).await;
    task.abort();
    assert!(task.await.expect_err("aborted request").is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            true
        })
        .await
        .expect("unrelated control work remains responsive"),
        "the runtime must not wait for the cancelled native job"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), cache.processing_permit())
            .await
            .is_err(),
        "the blocking job retains its processing permit after request cancellation"
    );
    wait_for(release).await;
    let result = receive(completed).await;
    drop(held);
    let replacement = tokio::time::timeout(Duration::from_secs(1), cache.processing_permit())
        .await
        .expect("permit released when native job ended");
    drop(replacement);
    result
}

#[tokio::test]
async fn cancelling_embedded_extraction_keeps_processing_permit_until_decoder_stops() {
    let result = assert_cancelled_job_keeps_permit(test_cache(), |entered, release, done| {
        entered.wait();
        release.wait();
        // Execute the actual synchronous metadata decoder, not a stand-in sleeper.
        done.send(
            musicata_core::extract_embedded_cover_from(
                Cursor::new(b"not an audio container".to_vec()),
                Path::new("cover.flac"),
            )
            .is_err(),
        )
        .expect("report decoder completion");
    })
    .await;
    assert!(
        result,
        "the extractor ran after the cancellation gate opened"
    );
}

#[tokio::test]
async fn cancelling_resize_keeps_processing_permit_until_encoder_stops() {
    let mut source = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        1024,
        1024,
        image::Rgb([30, 60, 90]),
    ))
    .write_to(&mut Cursor::new(&mut source), image::ImageFormat::Jpeg)
    .expect("encode test image");
    let result = assert_cancelled_job_keeps_permit(test_cache(), move |entered, release, done| {
        entered.wait();
        release.wait();
        // Execute the actual image decode/downscale/JPEG encode work.
        done.send(resize_to_jpeg(&source, 300).is_some())
            .expect("report resize completion");
    })
    .await;
    assert!(result, "the resize ran after the cancellation gate opened");
}
