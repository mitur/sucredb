use database::{Context, NodeId};
use fabric::FabricMsg;
use rand::{thread_rng, Rng};
use std::sync::mpsc;
use std::{thread, time};

pub enum WorkerMsg {
    Fabric(NodeId, FabricMsg),
    Command(Context),
    Tick(time::Instant),
    DHTFabric(NodeId, FabricMsg),
    DHTChange,
    Exit,
}

/// A Sender attached to a WorkerManager
/// messages are distributed to threads in a Round-Robin manner.
pub struct WorkerSender {
    cursor: usize,
    channels: Vec<mpsc::Sender<WorkerMsg>>,
}

/// A thread pool containing threads prepared to receive WorkerMsg's
pub struct WorkerManager {
    ticker_interval: time::Duration,
    ticker_thread: Option<thread::JoinHandle<()>>,
    ticker_chan: Option<mpsc::Sender<()>>,
    thread_count: usize,
    threads: Vec<thread::JoinHandle<()>>,
    channels: Vec<mpsc::Sender<WorkerMsg>>,
    node: NodeId,
}

impl WorkerManager {
    pub fn new(node: NodeId, thread_count: usize, ticker_interval: time::Duration) -> Self {
        assert!(thread_count > 0);
        WorkerManager {
            ticker_interval: ticker_interval,
            ticker_thread: None,
            ticker_chan: None,
            thread_count: thread_count,
            threads: Default::default(),
            channels: Default::default(),
            node: node,
        }
    }

    pub fn start<F>(&mut self, mut worker_fn_gen: F)
    where
        F: FnMut() -> Box<FnMut(mpsc::Receiver<WorkerMsg>) + Send>,
    {
        assert!(self.channels.is_empty());
        for i in 0..self.thread_count {
            // since neither closure cloning or Box<FnOnce> are stable use Box<FnMut>
            let mut worker_fn = worker_fn_gen();
            let (tx, rx) = mpsc::channel();
            self.threads.push(
                thread::Builder::new()
                    .name(format!("Worker:{}:{}", i, self.node))
                    .spawn(move || {
                        worker_fn(rx);
                        info!("Exiting worker");
                    })
                    .unwrap(),
            );
            self.channels.push(tx);
        }

        let (ticker_tx, ticker_rx) = mpsc::channel();
        self.ticker_chan = Some(ticker_tx);
        let ticker_interval = self.ticker_interval;
        let mut sender = self.sender();
        self.ticker_thread = Some(
            thread::Builder::new()
                .name(format!("WorkerTicker:{}", self.node))
                .spawn(move || loop {
                    thread::sleep(ticker_interval);
                    match ticker_rx.try_recv() {
                        Err(mpsc::TryRecvError::Empty) => (),
                        _ => break,
                    }
                    let _ = sender.try_send(WorkerMsg::Tick(time::Instant::now()));
                })
                .unwrap(),
        );
    }

    pub fn sender(&self) -> WorkerSender {
        assert!(!self.channels.is_empty());
        WorkerSender {
            cursor: thread_rng().gen(),
            channels: self.channels.clone(),
        }
    }
}

impl WorkerSender {
    pub fn send(&mut self, msg: WorkerMsg) {
        // right now only possible error is disconected, so no need to do anything
        let _ = self.try_send(msg);
    }
    pub fn try_send(&mut self, msg: WorkerMsg) -> Result<(), mpsc::SendError<WorkerMsg>> {
        self.cursor = self.cursor.wrapping_add(1);
        self.channels[self.cursor % self.channels.len()].send(msg)
    }
}

impl Drop for WorkerManager {
    fn drop(&mut self) {
        for c in &*self.channels {
            let _ = c.send(WorkerMsg::Exit);
        }
        if let Some(c) = self.ticker_chan.take() {
            let _ = c.send(());
        }
        if let Some(t) = self.ticker_thread.take() {
            let _ = t.join();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
