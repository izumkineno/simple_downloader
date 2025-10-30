// file.rs
use std::sync::mpsc;

use crate::types::*;
use faststr::FastStr;
use tokio::fs::OpenOptions;
use tokio::io::{self, AsyncSeekExt, AsyncWriteExt};
use tokio::spawn;
use tokio::sync::broadcast;

pub(crate) async fn file_writer_task(
    mut evnet_rx: broadcast::Receiver<EventThread>,
    filepath: FastStr,
    size: u64,
) -> Result<mpsc::Sender<EventResource>> {
    let (tx, rx) = mpsc::channel::<EventResource>();

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&*filepath)
        .await?;
    file.set_len(size).await?;

    spawn(async move {
        while let Ok(command) = rx.recv() {
            match command {
                EventResource::WriteFile { offset, data } => {
                    if file.seek(io::SeekFrom::Start(offset)).await.is_err()
                        || file.write_all(&data).await.is_err()
                    {
                        break;
                    }
                    let _ = file.flush().await;
                }
                _ => {}
            }
            if let Ok(event) = evnet_rx.recv().await {
                if let EventThread::TerminateAll = event {
                    break;
                }
            }
        }
    });

    Ok(tx)
}
