use super::flow::ChangeRequest;
use crate::util::human_readable_bytes;
use crate::util::Shell;
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

#[derive(Clone, Debug, PartialEq)]
enum ProgressUpdate {
    Start { id: usize },
    Done { id: usize },
}

#[derive(Clone)]
pub(super) struct ProgressSender {
    sender: mpsc::Sender<ProgressUpdate>,
    total_bytes_read: Arc<AtomicU64>,
}

impl ProgressSender {
    pub async fn notify_done(&self, id: usize) {
        let _ = self.sender.send(ProgressUpdate::Done { id }).await;
    }

    pub async fn notify_start(&self, id: usize) {
        let _ = self.sender.send(ProgressUpdate::Start { id }).await;
    }

    /// Note: However this is implemented, it is fast and synchronous (so that it can be called often)!
    pub fn notify_bytes_read(&self, _id: usize, delta: u64) {
        self.total_bytes_read.fetch_add(delta, Ordering::Relaxed);
    }
}

struct Throughput {
    // We keep two samples around. We use the older sample to determine the rate of change
    // (so that we always average over a long enough timespan). Once the newer sample is "old enough"
    // it replaces the older sample
    samples: [(Instant, u64); 2],
    // Index to old sample (as boolean, for convenience)
    cur: usize,
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

pub(super) struct ProgressReceiver<'a> {
    /// List of changes so that we can determine their names and stuff. If we have ever lifetime troubles,
    /// this can be changed to Arc<[FileChange]>.
    changes: &'a [ChangeRequest],
    receiver: mpsc::Receiver<ProgressUpdate>,
    /// How many bytes we have transferred
    total_bytes_read: Arc<AtomicU64>,
    /// How many bytes we expect to transfer
    total_bytes: u64,
    /// How many changes already finished.
    done: usize,
    throughput: Throughput,
}

impl<'a> ProgressReceiver<'a> {
    fn new(
        changes: &'a [ChangeRequest],
        receiver: mpsc::Receiver<ProgressUpdate>,
        total_bytes_read: Arc<AtomicU64>,
    ) -> Self {
        let total_bytes = changes
            .iter()
            .filter_map(|change| match change {
                ChangeRequest::CreateFile { info } | ChangeRequest::UpdateFile { info } => {
                    Some(info.size)
                }
                ChangeRequest::DeleteFile { .. } | ChangeRequest::DeleteDir { .. } => None,
            })
            .sum();

        Self {
            receiver,
            total_bytes_read,
            total_bytes,
            throughput: Throughput::new(),
            changes,
            done: 0,
        }
    }

    /// mut since we modify the throughput here...
    fn print_progress_line(&mut self, shell: &Shell) -> std::io::Result<()> {
        let bytes_read = self.total_bytes_read.load(Ordering::Acquire);
        let remaining = self.total_bytes.saturating_sub(bytes_read);
        let throughput = self.throughput.update_and_get(bytes_read) as u64;

        let percentage = 100 * bytes_read / self.total_bytes;
        let (rem, rem_unit) = human_readable_bytes(remaining);
        let (thru, thru_unit) = human_readable_bytes(throughput);
        let status_line =
            format!("{percentage}%, Remaining: {rem:.2} {rem_unit} ({thru:.2} {thru_unit}/s)");
        shell.print_transient(&status_line)?;

        Ok(())
    }

    pub async fn run(&mut self, shell: &Shell) -> anyhow::Result<()> {
        let mut start = None;

        loop {
            let receive_and_update = async {
                let ret = self.receiver.recv().await;
                if start.is_none() {
                    start = Some(Instant::now());
                }
                ret
            };

            // Documentation states that receiver does not have a problem with being cancelled.
            let next_value = timeout(Duration::from_millis(500), receive_and_update).await;

            match next_value {
                Ok(Some(ProgressUpdate::Done { id })) => {
                    self.done += 1;
                    writeln!(
                        shell.err(),
                        "({} / {}) Done: {}",
                        self.done,
                        self.changes.len(),
                        &self.changes[id],
                    )?;
                    self.print_progress_line(shell)?;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    // Timeout has elapsed. Let's show a progress update.
                    self.print_progress_line(shell)?;
                }
            }
        }

        let elapsed = start.map(|start| start.elapsed()).unwrap_or(Duration::ZERO);

        let avg_throughput = self.total_bytes as f64 / elapsed.as_secs_f64();
        let (avg_throughput, avg_unit) = human_readable_bytes(avg_throughput as u64);
        write!(
            shell.err(),
            "Finished in {}s (Average throughput: {avg_throughput:.2} {avg_unit})",
            elapsed.as_secs()
        )?;

        Ok(())
    }
}

pub(super) fn make_progress<'a>(
    changes: &'a [ChangeRequest],
) -> (ProgressSender, ProgressReceiver) {
    let (sender, receiver) = mpsc::channel(16);
    let total_bytes_read = Arc::new(AtomicU64::new(0));

    let s = ProgressSender {
        sender,
        total_bytes_read: total_bytes_read.clone(),
    };
    let r = ProgressReceiver::new(changes, receiver, total_bytes_read);
    (s, r)
}
