//! Voice uses the same private journals and conversation receipts as typed turns.
use super::*;
use crate::assistant::Turn;
use crate::assistant_log::Journal;
use std::path::Path;

#[derive(Clone)]
pub(super) struct TurnLog {
    pub receipt: Arc<Turn>,
    events: mpsc::Sender<(&'static str, Value)>,
}

impl TurnLog {
    pub fn start(root: &Path, row: usize) -> Result<Self> {
        let mut turn = Turn::new(row as u64);
        turn.journal = Some(Journal::start(root, row as u64, json!({
            "inputMode":"voice", "assistant":"Duce", "build":env!("CARGO_PKG_VERSION")
        }))?);
        let receipt = Arc::new(turn);
        let journal = receipt.clone();
        let (events, receive) = mpsc::channel::<(&'static str, Value)>();
        std::thread::spawn(move || {
            let mut failure = None;
            for (event, data) in receive {
                if let Some(reason) = data["error"].as_str() {
                    failure = Some(reason.to_owned());
                }
                if let Err(error) = journal.record(event, data) {
                    eprintln!("Could not write voice turn journal: {error}");
                    return;
                }
            }
            journal.finish(failure.as_deref());
            let outcome = if failure.is_some() { "failed" } else { "completed" };
            if let Err(error) = journal.finish_log(outcome, failure.as_deref()) {
                eprintln!("Could not finish voice turn journal: {error}");
            }
        });
        Ok(Self { receipt, events })
    }

    pub fn record(&self, event: &'static str, data: Value) {
        let _ = self.events.send((event, data));
    }

    pub fn result(&self, tool: &str, call: &str, started: Instant, error: Option<String>) {
        self.record("voice_tool_result", json!({"tool":tool,"callId":call,
            "durationMs":started.elapsed().as_millis(),"error":error}));
    }
}
