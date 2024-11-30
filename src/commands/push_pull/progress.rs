use crate::util::human_readable_bytes;
use crate::util::Shell;
use slotmap::new_key_type;
use slotmap::SlotMap;
use std::fmt::Display;
use std::io::Write;
use std::time::Instant;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::mpsc, time::timeout};

#[derive(Debug)]
enum ProgressUpdate {
    Start { slot: ProgressId },
    Done { slot: ProgressId },
    Failed { slot: ProgressId },
}

#[derive(Clone)]
pub(super) struct ProgressSender {
    sender: mpsc::UnboundedSender<ProgressUpdate>,
    total_bytes_read: Arc<AtomicU64>,
}

new_key_type! {
    pub(super) struct ProgressId;
}

impl ProgressSender {
    pub fn notify_start(&self, id: ProgressId) {
        self.sender
            .send(ProgressUpdate::Start { slot: id })
            .unwrap()
    }

    pub fn notify_bytes_read(&self, id: ProgressId, bytes: u64) {
        let _ = id;
        self.total_bytes_read.fetch_add(bytes, Ordering::SeqCst);
    }

    pub fn notify_done(&self, id: ProgressId) {
        self.sender
            .send(ProgressUpdate::Done { slot: id })
            .unwrap();
    }

    pub fn notify_failed(&self, id: ProgressId) {
        self.sender
            .send(ProgressUpdate::Failed { slot: id })
            .unwrap();
    }
}

enum ItemState {
    Unstarted,
    Started,
    Done,
    Failed
}

struct Item<T> {
    state: ItemState,
    item: T
}

pub(super) struct ProgressReceiver<T> {
    items: SlotMap<ProgressId, Item<T>>,
    total_bytes_read: Arc<AtomicU64>,
    /// List of changes so that we can determine their names and stuff. If we have ever lifetime troubles,
    /// this can be changed to Arc<[FileChange]>.
    receiver: mpsc::UnboundedReceiver<ProgressUpdate>,
    /// How many changes already finished.
    done: usize,
    throughput: Throughput,
}

impl<T: Display> ProgressReceiver<T> {
    /// mut since we modify the throughput here...
    fn print_progress_line(&mut self, shell: &Shell) -> std::io::Result<()> {
        if self.items.is_empty() {
            return Ok(());
        }

        let bytes_read = self.total_bytes_read.load(Ordering::Acquire);
        let throughput = self.throughput.update_and_get(bytes_read) as u64;

        let percentage = (self.done * 100) / self.items.len();
        let (thru, thru_unit) = human_readable_bytes(throughput);
        let status_line =
            format!("{percentage}% {thru:.2} {thru_unit}/s");
        shell.print_transient(&status_line)?;

        Ok(())
    }

    pub fn add_item(&mut self, item: T) -> ProgressId {
        self.items.insert(Item {
            state: ItemState::Unstarted,
            item
        })
    }

    pub async fn run(&mut self, shell: &Shell) -> anyhow::Result<()> {
        let mut start = None;

        loop {
            let receive_and_update = async { self.receiver.recv().await };

            // Documentation states that receiver does not have a problem with being cancelled.
            let next_value = timeout(Duration::from_millis(500), receive_and_update).await;
            let update = match next_value {
                Ok(None) => {
                    // Channel is closed
                    break;
                }
                Err(_) => {
                    // Timeout expired, let's say something
                    self.print_progress_line(shell)?;
                    continue;
                }
                Ok(Some(update)) => update,
            };

            match update {
                ProgressUpdate::Start { slot } => {
                    if start.is_none() {
                        start = Some(Instant::now());
                    }

                    self.items[slot].state = ItemState::Started;
                }
                ProgressUpdate::Failed { slot } => {
                    self.done += 1;
                    writeln!(
                        shell.err(),
                        "({} / {}) Failed: {}",
                        self.done,
                        self.items.len(),
                        &self.items[slot].item,
                    )?;
                    self.print_progress_line(shell)?;

                    self.items[slot].state = ItemState::Failed;
                }
                ProgressUpdate::Done { slot } => {
                    self.done += 1;
                    writeln!(
                        shell.err(),
                        "({} / {}) Done: {}",
                        self.done,
                        self.items.len(),
                        &self.items[slot].item,
                    )?;
                    self.print_progress_line(shell)?;
                    self.items[slot].state = ItemState::Done;
                }
            }
        }

        // All items that are in started are now probably done. We could do something with that
        // information, but we do not.

        let elapsed = start.map(|start| start.elapsed()).unwrap_or(Duration::ZERO);

        let avg_throughput = self.total_bytes_read.load(Ordering::SeqCst) as f64 / elapsed.as_secs_f64();
        let (avg_throughput, avg_unit) = human_readable_bytes(avg_throughput as u64);
        write!(
            shell.err(),
            "Finished in {}s (Average throughput: {avg_throughput:.2} {avg_unit}/s)",
            elapsed.as_secs()
        )?;

        Ok(())
    }
}

pub(super) fn make<T: Display>() -> (ProgressSender, ProgressReceiver<T>) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let total_bytes_read = Arc::new(AtomicU64::new(0));

    let sender = ProgressSender {
        sender,
        total_bytes_read: total_bytes_read.clone()
    };

    let receiver = ProgressReceiver {
        items: Default::default(),
        total_bytes_read,
        receiver,
        done: 0,
        throughput: Default::default()
    };

    (sender, receiver)
}

struct Throughput {
    // We keep two samples around. We use the older sample to determine the rate of change
    // (so that we always average over a long enough timespan). Once the newer sample is "old enough"
    // it replaces the older sample
    samples: [(Instant, u64); 2],
    // Index to old sample
    cur: usize,
}

impl Default for Throughput {
    fn default() -> Self {
        Self::new()
    }
}

impl Throughput {
    fn new() -> Self {
        Self {
            samples: [(Instant::now(), 0), (Instant::now(), 0)],
            cur: 0,
        }
    }

    /// Return throughput in bytes per second
    fn update_and_get(&mut self, cur: u64) -> f64 {
        let now = Instant::now();

        let old = self.cur;
        let new = (self.cur + 1) % 2;

        // Replace the newer sample with this sample (and the older with the newer)?
        if (now - self.samples[old].0) >= Duration::from_millis(500) {
            self.samples.swap(old, new);
            self.samples[new] = (now, cur);
        }

        // Now use the older sample to calculate throughput
        let (old_sampled_at, old_sample) = &self.samples[old];
        (cur - old_sample) as f64 / (now - *old_sampled_at).as_secs_f64()
    }
}
