//! Times `read_directory` on a real folder of a million files.
//!
//! Creates the fixture in a fresh directory under the system temp dir, scans
//! it a few times (`read_directory` enumerates, reads the metadata and sorts
//! by name), times the name sort alone on a shuffled copy of the entries, and
//! removes the fixture before it exits, even when a step fails.
//!
//! ```text
//! cargo run --release --offline --example read_dir_bench [entries]
//! ```

use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use fileflux::core::fs::{read_directory, sort_entries};

/// A tiny xorshift64*, so the names are the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn bench(root: &Path, count: usize) -> io::Result<()> {
    const STEMS: [&str; 4] = ["IMG_", "report-", "Photo ", "a"];
    const EXTENSIONS: [&str; 4] = ["jpg", "txt", "PNG", "rs"];
    let mut rng = Rng(0xF11E_F10C);
    let started = Instant::now();
    for serial in 0..count {
        let word = rng.next();
        let stem = STEMS.get((word % 4) as usize).copied().unwrap_or("f");
        let extension = EXTENSIONS
            .get((word >> 8) as usize % 4)
            .copied()
            .unwrap_or("");
        let path = root.join(format!(
            "{stem}{}_{serial}.{extension}",
            (word >> 16) % 1_000
        ));
        if word.is_multiple_of(50) {
            fs::create_dir(path)?;
        } else {
            fs::File::create(path)?;
        }
    }
    println!(
        "created {count} entries in {:.0} ms",
        millis(started.elapsed())
    );

    let mut best = Duration::MAX;
    let mut listing = None;
    for _ in 0..3 {
        let started = Instant::now();
        let scanned = read_directory(root)?;
        best = best.min(started.elapsed());
        listing = Some(scanned);
    }
    let mut listing = listing.ok_or_else(|| io::Error::other("no scan ran"))?;
    println!(
        "read_directory (best of 3): {:.1} ms, {} entries, {} warnings",
        millis(best),
        listing.entries.len(),
        listing.warnings.len()
    );

    // The scan already sorted the listing; shuffle it for an honest sort.
    for position in (1..listing.entries.len()).rev() {
        let other = (rng.next() % (position as u64 + 1)) as usize;
        listing.entries.swap(position, other);
    }
    let started = Instant::now();
    sort_entries(&mut listing.entries);
    println!(
        "sort_entries on a shuffled listing: {:.1} ms",
        millis(started.elapsed())
    );
    Ok(())
}

fn main() -> io::Result<()> {
    let count = std::env::args()
        .nth(1)
        .and_then(|argument| argument.parse().ok())
        .unwrap_or(1_000_000);
    let root = std::env::temp_dir().join(format!("fileflux-read-dir-bench-{}", std::process::id()));
    fs::create_dir_all(&root)?;
    let result = bench(&root, count);
    let cleanup = fs::remove_dir_all(&root);
    if let Err(error) = &cleanup {
        eprintln!("could not remove the fixture {}: {error}", root.display());
    }
    result.and(cleanup)
}
