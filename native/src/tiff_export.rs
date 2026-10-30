//! TIFF export borrows planes from owned decoded units. Jobs share the codec
//! pool; byte credits reserve payloads and an estimate of decoder working space.
use crate::tiff_metadata::{self, PlaneSource};
use radelta_native::{Dims5, RadeltaFileHandle, DEFAULT_BLOCK_DEPTH, DEFAULT_FILE_CACHE_BYTES};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{mpsc, Arc};

type Result<T> = std::result::Result<T, String>;

struct Unit {
    coords: (usize, usize, usize),
    last_page: usize,
    bytes: usize,
    blocks: usize,
}
struct Page {
    canonical: usize,
    unit: usize,
    offset: usize,
}
struct Plan {
    units: Vec<Unit>,
    pages: Vec<Page>,
    peak_bytes: usize,
    peak_units: usize,
}
impl Plan {
    fn new(file: &RadeltaFileHandle, d: Dims5, mapping: &[usize]) -> Result<Self> {
        let plane = d.x.checked_mul(d.y).ok_or("plane size overflow")?;
        let mut index = HashMap::new();
        let mut units: Vec<Unit> = Vec::new();
        let mut pages = Vec::with_capacity(mapping.len());
        for (page, &canonical) in mapping.iter().enumerate() {
            let coords = (
                canonical / (d.c * d.z),
                canonical / d.z % d.c,
                canonical % d.z,
            );
            let info = file.decode_unit_info(coords.0, coords.1, coords.2)?;
            let unit = match index.get(&info.key) {
                Some(&unit) => unit,
                None => {
                    let bytes = plane
                        .checked_mul(info.depth)
                        .and_then(|n| n.checked_mul(2))
                        // Match the streaming writer's conservative allowance
                        // for codec scratch, decoded pixels and allocator reuse.
                        .and_then(|n| n.checked_mul(4))
                        .and_then(|n| n.checked_add(usize::try_from(info.encoded_bytes).ok()?))
                        .ok_or("decode unit size overflow")?;
                    let unit = units.len();
                    units.push(Unit {
                        coords,
                        last_page: page,
                        bytes,
                        blocks: info.depth.div_ceil(DEFAULT_BLOCK_DEPTH),
                    });
                    index.insert(info.key, unit);
                    unit
                }
            };
            units[unit].last_page = page;
            pages.push(Page {
                canonical,
                unit,
                offset: (coords.2 - info.z0) * plane,
            });
        }
        // Interleaved output can keep several units alive until later pages.
        // Verify that these mandatory residents fit before enabling lookahead.
        let (mut next, mut bytes, mut count) = (0, 0usize, 0usize);
        let (mut peak_bytes, mut peak_units) = (0, 0);
        for (page, p) in pages.iter().enumerate() {
            if p.unit == next {
                bytes = bytes
                    .checked_add(units[p.unit].bytes)
                    .ok_or("decode working size overflow")?;
                count += 1;
                next += 1;
                peak_bytes = peak_bytes.max(bytes);
                peak_units = peak_units.max(count);
            }
            if units[p.unit].last_page == page {
                bytes -= units[p.unit].bytes;
                count -= 1;
            }
        }
        Ok(Self {
            units,
            pages,
            peak_bytes,
            peak_units,
        })
    }

    fn fits(&self, budget: usize) -> bool {
        // Use the direct reader for an oversized unit as well as over-budget
        // interleaving. This avoids a succession of large worker-local
        // allocations when only one unit can fit at a time.
        self.peak_bytes <= budget
    }
}

struct Completion {
    unit: usize,
    pixels: Result<Vec<u16>>,
}
struct UnitSource {
    file: Arc<RadeltaFileHandle>,
    plan: Plan,
    ready: HashMap<usize, Vec<u16>>,
    send: mpsc::Sender<Completion>,
    receive: mpsc::Receiver<Completion>,
    next: usize,
    page: usize,
    finished: Option<usize>,
    pending: usize,
    blocks: usize,
    bytes: usize,
    residents: usize,
    budget: usize,
    workers: usize,
    resident_limit: usize,
    plane: usize,
}
impl UnitSource {
    fn new(file: Arc<RadeltaFileHandle>, plan: Plan, budget: usize, plane: usize) -> Self {
        let (send, receive) = mpsc::channel();
        let workers = rayon::current_num_threads().max(1);
        let resident_limit = plan
            .peak_units
            .saturating_add(workers.saturating_mul(2))
            .min(plan.units.len());
        Self {
            file,
            plan,
            ready: HashMap::new(),
            send,
            receive,
            next: 0,
            page: 0,
            finished: None,
            pending: 0,
            blocks: 0,
            bytes: 0,
            residents: 0,
            budget,
            workers,
            resident_limit,
            plane,
        }
    }

    fn schedule(&mut self) {
        while let Some(unit) = self.plan.units.get(self.next) {
            let blocks = unit.blocks.min(self.workers);
            if self.residents == self.resident_limit
                || (self.pending > 0 && blocks > self.workers - self.blocks)
                || (self.residents > 0 && unit.bytes > self.budget.saturating_sub(self.bytes))
            {
                break;
            }
            let coords = unit.coords;
            let index = self.next;
            let file = self.file.clone();
            let send = self.send.clone();
            rayon::spawn(move || {
                let pixels = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    file.decode_unit(coords.0, coords.1, coords.2)
                }))
                .unwrap_or_else(|_| Err("Radelta decode worker panicked".into()));
                drop(file);
                let _ = send.send(Completion {
                    unit: index,
                    pixels,
                });
            });
            self.next += 1;
            self.pending += 1;
            self.blocks += blocks;
            self.bytes += unit.bytes;
            self.residents += 1;
        }
    }

    fn wait(&self) -> Result<Completion> {
        // A caller inside a one-worker Rayon pool must help execute queued
        // codec jobs rather than block that pool, including during cleanup.
        if rayon::current_thread_index().is_some() {
            loop {
                match self.receive.try_recv() {
                    Ok(done) => return Ok(done),
                    Err(mpsc::TryRecvError::Empty) => {
                        rayon::yield_now();
                        std::thread::yield_now();
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err("decode worker disconnected".into())
                    }
                }
            }
        }
        self.receive
            .recv()
            .map_err(|_| "decode worker disconnected".into())
    }

    fn completed(&mut self, done: Completion) -> Result<()> {
        self.pending -= 1;
        self.blocks -= self.plan.units[done.unit].blocks.min(self.workers);
        self.ready.insert(done.unit, done.pixels?);
        Ok(())
    }
}
impl PlaneSource for UnitSource {
    fn plane(&mut self, canonical: usize) -> Result<&[u16]> {
        if let Some(unit) = self.finished.take() {
            self.ready.remove(&unit);
            self.bytes -= self.plan.units[unit].bytes;
            self.residents -= 1;
        }
        while let Ok(done) = self.receive.try_recv() {
            self.completed(done)?;
        }
        let page = self
            .plan
            .pages
            .get(self.page)
            .ok_or("too many TIFF planes requested")?;
        if page.canonical != canonical {
            return Err("TIFF writer requested unexpected plane order".into());
        }
        let (unit, offset) = (page.unit, page.offset);
        self.schedule();
        while !self.ready.contains_key(&unit) {
            if self.pending == 0 {
                return Err("decode read-ahead cannot make progress within its allowance".into());
            }
            let done = self.wait()?;
            self.completed(done)?;
            self.schedule();
        }
        if self.plan.units[unit].last_page == self.page {
            self.finished = Some(unit);
        }
        self.page += 1;
        self.ready[&unit]
            .get(offset..offset + self.plane)
            .ok_or_else(|| "decoded unit has too few pixels".into())
    }
}
impl Drop for UnitSource {
    fn drop(&mut self) {
        // All submitted jobs must release their file handles before an output
        // failure returns or the finished TIFF is published. No new jobs enter.
        while self.pending > 0 {
            match self.wait() {
                Ok(_) => self.pending -= 1,
                Err(_) => break,
            }
        }
    }
}

struct DirectSource {
    file: Arc<RadeltaFileHandle>,
    dims: Dims5,
    pixels: Vec<u16>,
}
impl PlaneSource for DirectSource {
    fn plane(&mut self, canonical: usize) -> Result<&[u16]> {
        let d = self.dims;
        self.file.read_plane(
            canonical / (d.c * d.z),
            canonical / d.z % d.c,
            canonical % d.z,
            &mut self.pixels,
        )?;
        Ok(&self.pixels)
    }
}

pub fn write(
    file: Arc<RadeltaFileHandle>,
    d: Dims5,
    order: Option<&str>,
    memory_mib: usize,
    path: &Path,
) -> Result<()> {
    let budget = memory_mib
        .checked_mul(1024 * 1024)
        .ok_or("memory budget overflow")?;
    let plane = d.x.checked_mul(d.y).ok_or("plane size overflow")?;
    let mapping = tiff_metadata::write_order(d, order, file.metadata())?;
    let plan = Plan::new(&file, d, &mapping)?;
    if plan.fits(budget) {
        let mut source = UnitSource::new(file.clone(), plan, budget, plane);
        // Decode the first units while descriptions and TIFF directories are
        // prepared, rather than waiting until the writer requests plane zero.
        source.schedule();
        let result = tiff_metadata::write(path, d, order, file.metadata(), &mut source);
        drop(source);
        result
    } else {
        file.set_cache_limit((budget / 4).min(DEFAULT_FILE_CACHE_BYTES))?;
        let mut source = DirectSource {
            file: file.clone(),
            dims: d,
            pixels: crate::tiff_io::pixel_buffer(plane)?,
        };
        tiff_metadata::write(path, d, order, file.metadata(), &mut source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radelta_native::{compress_u16_nd, Options};
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    fn fixture() -> (std::path::PathBuf, Arc<RadeltaFileHandle>, Dims5, Vec<u16>) {
        let d = Dims5 {
            x: 17,
            y: 11,
            z: 7,
            c: 3,
            t: 2,
        };
        let pixels: Vec<_> = (0..d.voxels().unwrap())
            .map(|i| (i * 37 % 60000) as u16)
            .collect();
        let path = std::env::temp_dir().join(format!(
            "radelta-unit-export-{}-{}.rdlt",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(
            &path,
            compress_u16_nd(&pixels, d, Options::default()).unwrap(),
        )
        .unwrap();
        let file = Arc::new(RadeltaFileHandle::open(&path).unwrap());
        (path, file, d, pixels)
    }

    #[test]
    fn interleaved_and_reverse_output_borrow_correct_planes_with_one_worker() {
        let (path, file, d, pixels) = fixture();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        pool.install(|| {
            let imagej: Vec<_> = (0..d.t)
                .flat_map(|t| {
                    (0..d.z).flat_map(move |z| (0..d.c).map(move |c| (t * d.c + c) * d.z + z))
                })
                .collect();
            for mapping in [
                (0..d.t * d.c * d.z).collect(),
                imagej.clone(),
                imagej.into_iter().rev().collect(),
            ] {
                let plan = Plan::new(&file, d, &mapping).unwrap();
                let budget = plan.peak_bytes;
                assert!(plan.fits(budget));
                if plan.peak_units > 1 {
                    assert!(!plan.fits(budget - 1));
                }
                let mut source = UnitSource::new(file.clone(), plan, budget, d.x * d.y);
                for i in mapping {
                    assert_eq!(
                        source.plane(i).unwrap(),
                        &pixels[i * d.x * d.y..(i + 1) * d.x * d.y]
                    );
                    assert!(source.bytes <= budget);
                }
            }
        });
        assert_eq!(Arc::strong_count(&file), 1);
        drop(file);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn early_drop_and_read_errors_release_queued_handles_with_one_worker() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        for damage in [false, true] {
            let (path, file, d, _) = fixture();
            pool.install(|| {
                let mapping: Vec<_> = (0..d.t * d.c * d.z).collect();
                let plan = Plan::new(&file, d, &mapping).unwrap();
                let mut source = UnitSource::new(file.clone(), plan, 1024 * 1024, d.x * d.y);
                if damage {
                    fs::OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_len(0)
                        .unwrap();
                    assert!(source.plane(0).is_err());
                } else {
                    source.plane(0).unwrap();
                    assert!(source.pending > 0);
                }
                drop(source);
                assert_eq!(Arc::strong_count(&file), 1);
            });
            drop(file);
            fs::remove_file(path).unwrap();
        }
    }
}
