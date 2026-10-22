//! A fixed pool of reusable buffers, filled by one I/O worker. Dropping the
//! reader closes both channels before joining, including on consumer errors.
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

pub struct Prefetch<T> {
    ready: Option<Receiver<Result<T, String>>>,
    free: Option<SyncSender<T>>,
    worker: Option<JoinHandle<Result<(), String>>>,
}

impl<T: Send + 'static> Prefetch<T> {
    pub fn new(
        buffers: Vec<T>,
        mut fill: impl FnMut(&mut T) -> Result<bool, String> + Send + 'static,
    ) -> Result<Self, String> {
        if buffers.is_empty() {
            return Err("prefetch requires at least one buffer".into());
        }
        let (free, available) = mpsc::sync_channel(buffers.len());
        for buffer in buffers {
            free.send(buffer)
                .map_err(|_| "prefetch buffer channel closed")?;
        }
        let (send, ready) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("radelta-read".into())
            .spawn(move || {
                while let Ok(mut buffer) = available.recv() {
                    match fill(&mut buffer) {
                        Ok(true) => {
                            if send.send(Ok(buffer)).is_err() {
                                break;
                            }
                        }
                        Ok(false) => break,
                        Err(e) => {
                            let _ = send.send(Err(e.clone()));
                            return Err(e);
                        }
                    }
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            ready: Some(ready),
            free: Some(free),
            worker: Some(worker),
        })
    }

    pub fn next(&mut self) -> Result<Option<T>, String> {
        match self
            .ready
            .as_ref()
            .ok_or("prefetch already completed")?
            .recv()
        {
            Ok(result) => result.map(Some),
            Err(_) => {
                self.worker
                    .take()
                    .ok_or("prefetch already completed")?
                    .join()
                    .map_err(|_| "Radelta read worker panicked")??;
                Ok(None)
            }
        }
    }

    pub fn recycle(&self, buffer: T) {
        // The producer may already have reached EOF or reported an error.
        if let Some(free) = &self.free {
            let _ = free.send(buffer);
        }
    }
}

impl<T> Drop for Prefetch<T> {
    fn drop(&mut self) {
        self.ready.take();
        self.free.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn ordered_buffers_are_reused_and_producer_errors_propagate() {
        let mut next = 0;
        let mut reader = Prefetch::new(vec![vec![0u8; 8], vec![0; 8]], move |buf| {
            if next == 9 {
                return Err("damaged input".into());
            }
            buf.fill(next);
            next += 1;
            Ok(true)
        })
        .unwrap();
        let mut addresses = std::collections::HashSet::new();
        for i in 0..9 {
            let buf = reader.next().unwrap().unwrap();
            assert_eq!(buf, vec![i; 8]);
            addresses.insert(buf.as_ptr() as usize);
            reader.recycle(buf);
        }
        assert_eq!(addresses.len(), 2);
        assert_eq!(reader.next().unwrap_err(), "damaged input");
    }

    #[test]
    fn cancellation_joins_a_producer_with_a_full_output_channel() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut reader = Prefetch::new(vec![0; 2], move |value| {
            *value += 1;
            count.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        })
        .unwrap();
        let buffer = reader.next().unwrap().unwrap();
        reader.recycle(buffer);
        drop(reader);
        let finished = calls.load(Ordering::SeqCst);
        assert!((1..=3).contains(&finished));
    }

    #[test]
    fn producer_panics_are_errors_and_eof_is_clean() {
        let mut broken = Prefetch::new(vec![0u8], |_| panic!("read panic")).unwrap();
        assert!(broken.next().unwrap_err().contains("panicked"));
        let mut empty = Prefetch::new(vec![0u8], |_| Ok(false)).unwrap();
        assert_eq!(empty.next().unwrap(), None);
    }
}
