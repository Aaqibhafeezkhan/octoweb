//! Bounded delegated transcripts and recovery, independent of the parent turn.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};

const MAX_RUN_CONTENT: usize = 4 * 1024 * 1024;
const MAX_COMPLETED_RUNS: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
	Prompt {
		content: String,
	},
	Assistant {
		content: String,
	},
	Thinking {
		content: String,
	},
	ToolUse {
		tool: String,
		tool_id: String,
		params: serde_json::Value,
	},
	ToolResult {
		tool_id: String,
		content: String,
		success: Option<bool>,
	},
	Usage {
		cost: f64,
		input_tokens: u64,
		output_tokens: u64,
	},
	Status {
		status: Status,
		error: Option<String>,
	},
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
	Running,
	Done,
	Failed,
	Cancelled,
	Unknown,
}

impl Status {
	pub fn terminal(self) -> bool {
		matches!(self, Self::Done | Self::Failed | Self::Cancelled)
	}
}

impl Event {
	fn bytes(&self) -> usize {
		// Include the record itself so empty text/status events are bounded too.
		std::mem::size_of::<Self>()
			+ match self {
				Self::Prompt { content } | Self::Assistant { content } | Self::Thinking { content } => content.len(),
				Self::ToolUse { tool, tool_id, params } => tool.len() + tool_id.len() + params.to_string().len(),
				Self::ToolResult { tool_id, content, .. } => tool_id.len() + content.len(),
				Self::Usage { .. } => 0,
				Self::Status { error, .. } => error.as_ref().map_or(0, String::len),
			}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
	pub session_id: String,
	pub run_id: String,
	pub role: String,
	pub seq: u64,
	pub event: Event,
}

#[derive(Debug, Deserialize)]
pub struct Log {
	pub session_id: String,
	pub run_id: String,
	pub role: Option<String>,
	pub status: Status,
	pub seq: u64,
	pub truncated: bool,
	pub events: Vec<Event>,
	pub error: Option<String>,
	pub request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Run {
	pub run_id: String,
	pub role: Option<String>,
	pub status: Status,
	pub seq: u64,
	pub truncated: bool,
	pub events: VecDeque<Event>,
	pub error: Option<String>,
	pub complete: bool,
	#[serde(skip)]
	bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Delta {
	pub run_id: String,
	pub role: Option<String>,
	pub status: Status,
	pub seq: u64,
	pub truncated: bool,
	pub event: Option<Event>,
	pub merged: bool,
	pub dropped: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Update {
	Recovering { run_id: String },
	Snapshot { run: Run },
	Delta { delta: Delta },
	Error { run_id: String, error: String },
}

impl Run {
	fn new(run_id: String) -> Self {
		Self {
			run_id,
			role: None,
			status: Status::Unknown,
			seq: 0,
			truncated: false,
			events: VecDeque::new(),
			error: None,
			complete: false,
			bytes: 0,
		}
	}

	fn push(&mut self, event: Event) -> (bool, usize) {
		let merged = match (self.events.back_mut(), &event) {
			(Some(Event::Assistant { content }), Event::Assistant { content: chunk })
			| (Some(Event::Thinking { content }), Event::Thinking { content: chunk }) => {
				self.bytes += chunk.len();
				content.push_str(chunk);
				true
			}
			_ => {
				self.bytes += event.bytes();
				self.events.push_back(event);
				false
			}
		};
		let mut dropped = 0;
		while self.bytes > MAX_RUN_CONTENT {
			self.bytes -= self
				.events
				.pop_front()
				.expect("content budget implies a retained event")
				.bytes();
			dropped += 1;
			self.truncated = true;
		}
		(merged, dropped)
	}

	fn frame(&mut self, frame: Frame) -> Delta {
		self.seq = frame.seq;
		self.role = Some(frame.role);
		self.complete = true;
		if let Event::Status { status, .. } = &frame.event {
			self.status = *status;
		}
		let previous_len = self.events.len();
		let event = frame.event;
		let (merged, dropped) = self.push(event.clone());
		// An oversized newest item was dropped too; never queue its payload.
		let (event, merged, dropped) = if self.events.is_empty() {
			(None, false, previous_len)
		} else {
			(Some(event), merged, dropped)
		};
		Delta {
			run_id: self.run_id.clone(),
			role: self.role.clone(),
			status: self.status,
			seq: self.seq,
			truncated: self.truncated,
			event,
			merged,
			dropped,
		}
	}
}

/// Ordered by last activity, shared by the connection and sidebar caches.
#[derive(Debug, Default, Clone)]
pub struct Cache {
	runs: VecDeque<Run>,
}

impl Cache {
	pub fn values(&self) -> impl Iterator<Item = &Run> {
		self.runs.iter()
	}
	pub fn clear(&mut self) {
		self.runs.clear();
	}
	fn get(&self, id: &str) -> Option<&Run> {
		self.runs.iter().find(|run| run.run_id == id)
	}
	fn take(&mut self, id: &str) -> Run {
		match self.runs.iter().position(|run| run.run_id == id) {
			Some(index) => self.runs.remove(index).expect("position exists"),
			None => Run::new(id.to_owned()),
		}
	}
	fn put(&mut self, run: Run) -> Vec<String> {
		self.runs.push_back(run);
		let mut evicted = Vec::new();
		while self.runs.iter().filter(|run| run.status.terminal()).count() > MAX_COMPLETED_RUNS {
			let index = self
				.runs
				.iter()
				.position(|run| run.status.terminal())
				.expect("completed runs exceed limit");
			evicted.push(self.runs.remove(index).expect("position exists").run_id);
		}
		evicted
	}

	pub fn apply(&mut self, update: &Update) -> Vec<String> {
		let run = match update {
			Update::Snapshot { run } => {
				self.take(&run.run_id);
				run.clone()
			}
			Update::Delta { delta } => {
				let mut run = self.take(&delta.run_id);
				if let Some(event) = &delta.event {
					run.push(event.clone());
				} else {
					for _ in 0..delta.dropped {
						run.bytes -= run.events.pop_front().expect("delta removes retained events").bytes();
					}
				}
				run.role = delta.role.clone();
				run.status = delta.status;
				run.seq = delta.seq;
				run.truncated = delta.truncated;
				run.complete = true;
				run
			}
			Update::Recovering { run_id } => {
				let mut run = self.take(run_id);
				run.complete = false;
				run.error = None;
				run
			}
			Update::Error { run_id, error } => {
				let mut run = self.take(run_id);
				run.error = Some(error.clone());
				run.complete = false;
				run
			}
		};
		self.put(run)
	}
}

#[derive(Default)]
struct Buffered {
	frames: BTreeMap<u64, Frame>,
	bytes: usize,
	lost_through: u64,
}

impl Buffered {
	fn push(&mut self, frame: Frame) {
		self.bytes += frame.event.bytes();
		if let Some(old) = self.frames.insert(frame.seq, frame) {
			self.bytes -= old.event.bytes();
		}
		while self.bytes > MAX_RUN_CONTENT {
			let (seq, frame) = self.frames.pop_first().expect("buffer exceeds budget");
			self.bytes -= frame.event.bytes();
			self.lost_through = self.lost_through.max(seq);
		}
	}
}

#[derive(Default)]
struct Recovery {
	request_id: Option<String>,
	failed: bool,
	buffer: Buffered,
}

pub struct Streams {
	cache: Cache,
	recovery: HashMap<String, Recovery>,
	binding: bool,
}

impl Default for Streams {
	fn default() -> Self {
		Self {
			cache: Cache::default(),
			recovery: HashMap::new(),
			binding: true,
		}
	}
}

impl Streams {
	/// Called once with all cached baselines, before any bind-time events can publish.
	pub fn recover(&mut self, cache: Cache) -> Vec<Update> {
		self.cache = cache;
		for run in self.cache.values() {
			if !run.status.terminal() || !run.complete {
				self.recovery.entry(run.run_id.clone()).or_default();
			}
		}
		// Every run seen during bind needs a snapshot, including a terminal-only stream.
		self.binding = false;
		let updates: Vec<_> = self
			.recovery
			.keys()
			.map(|run_id| Update::Recovering { run_id: run_id.clone() })
			.collect();
		for update in &updates {
			let evicted = self.cache.apply(update);
			self.forget_evicted(evicted);
		}
		updates
	}

	fn forget_evicted(&mut self, evicted: Vec<String>) {
		for id in evicted {
			// Outstanding replies still need correlation; failed recoveries have
			// no reply left to consume and must not retain evicted transcripts.
			if self.recovery.get(&id).is_some_and(|recovery| recovery.failed) {
				self.recovery.remove(&id);
			}
		}
	}

	pub fn requests(&self) -> Vec<String> {
		if self.binding {
			return Vec::new();
		}
		self.recovery
			.iter()
			.filter(|(_, recovery)| recovery.request_id.is_none() && !recovery.failed)
			.map(|(id, _)| id.clone())
			.collect()
	}

	pub fn request(&mut self, run_id: &str, request_id: String) {
		self.recovery.get_mut(run_id).expect("run needs recovery").request_id = Some(request_id);
	}

	pub fn event(&mut self, frame: Frame) -> Vec<Update> {
		let id = frame.run_id.clone();
		if self.binding || self.recovery.contains_key(&id) {
			self.recovery.entry(id).or_default().buffer.push(frame);
			return Vec::new();
		}
		let previous = self.cache.get(&id).map_or(0, |run| run.seq);
		if frame.seq <= previous {
			return Vec::new();
		}
		if frame.seq != previous + 1 {
			self.recovery.entry(id.clone()).or_default().buffer.push(frame);
			let update = Update::Recovering { run_id: id };
			let evicted = self.cache.apply(&update);
			self.forget_evicted(evicted);
			return vec![update];
		}
		let mut run = self.cache.take(&id);
		let delta = run.frame(frame);
		let evicted = self.cache.put(run);
		self.forget_evicted(evicted);
		vec![Update::Delta { delta }]
	}

	pub fn log(&mut self, log: Log) -> anyhow::Result<Vec<Update>> {
		let recovery = self
			.recovery
			.get(&log.run_id)
			.ok_or_else(|| anyhow::anyhow!("unsolicited run_log for {}", log.run_id))?;
		anyhow::ensure!(
			log.request_id == recovery.request_id && log.request_id.is_some(),
			"run_log request_id mismatch for {}",
			log.run_id
		);
		let replay_error = log.error.or_else(|| {
			self.cache
				.get(&log.run_id)
				.filter(|run| log.seq < run.seq)
				.map(|run| format!("run_log sequence {} precedes cached sequence {}", log.seq, run.seq))
		});
		if let Some(error) = replay_error {
			self.recovery.get_mut(&log.run_id).expect("recovery exists").failed = true;
			let update = Update::Error {
				run_id: log.run_id,
				error: format!("Replay failed: {error}"),
			};
			let evicted = self.cache.apply(&update);
			self.forget_evicted(evicted);
			return Ok(vec![update]);
		}
		let recovery = self.recovery.remove(&log.run_id).expect("recovery exists");
		let mut run = Run::new(log.run_id.clone());
		run.role = log.role;
		run.status = log.status;
		run.seq = log.seq;
		run.truncated = log.truncated;
		run.complete = true;
		for event in log.events {
			run.push(event);
		}
		// A snapshot older than dropped buffered frames cannot establish continuity.
		if run.seq < recovery.buffer.lost_through {
			self.recovery.insert(
				log.run_id,
				Recovery {
					buffer: recovery.buffer,
					..Recovery::default()
				},
			);
			return Ok(Vec::new());
		}
		let mut buffered = recovery
			.buffer
			.frames
			.into_values()
			.filter(|frame| frame.seq > run.seq)
			.collect::<VecDeque<_>>();
		while let Some(frame) = buffered.pop_front() {
			if frame.seq != run.seq + 1 {
				let mut recovery = Recovery::default();
				recovery.buffer.push(frame);
				for frame in buffered {
					recovery.buffer.push(frame);
				}
				self.recovery.insert(log.run_id, recovery);
				return Ok(Vec::new());
			}
			run.frame(frame);
		}
		let update = Update::Snapshot { run };
		let evicted = self.cache.apply(&update);
		self.forget_evicted(evicted);
		Ok(vec![update])
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	fn frame(seq: u64, event: serde_json::Value) -> Frame {
		serde_json::from_value(
			json!({"session_id":"parent", "run_id":"tap-general-abcdef", "role":"developer:general", "seq":seq, "event":event}),
		)
		.unwrap()
	}
	fn snapshot(seq: u64, events: serde_json::Value) -> Log {
		serde_json::from_value(json!({"session_id":"parent", "run_id":"tap-general-abcdef", "role":"developer:general", "status":"running", "seq":seq, "truncated":false, "events":events, "request_id":"replay"})).unwrap()
	}
	fn ready() -> Streams {
		let mut streams = Streams::default();
		streams.recover(Cache::default());
		streams
	}
	fn text(seq: u64, content: &str) -> Frame {
		frame(seq, json!({"kind":"assistant","content":content}))
	}

	#[test]
	fn every_event_maps_without_losing_optional_success_or_totals() {
		let events = [
			json!({"kind":"prompt","content":"task"}),
			json!({"kind":"assistant","content":"reply"}),
			json!({"kind":"thinking","content":"thought"}),
			json!({"kind":"tool_use","tool":"read","tool_id":"one","params":{"path":"src"}}),
			json!({"kind":"tool_result","tool_id":"one","content":"result","success":true}),
			json!({"kind":"tool_result","tool_id":"two","content":"failed","success":false}),
			json!({"kind":"tool_result","tool_id":"three","content":"disk","success":null}),
			json!({"kind":"usage","cost":0.12,"input_tokens":1200,"output_tokens":300}),
			json!({"kind":"status","status":"running","error":null}),
			json!({"kind":"status","status":"done","error":null}),
			json!({"kind":"status","status":"failed","error":"failure"}),
			json!({"kind":"status","status":"cancelled","error":null}),
		];
		let mut streams = ready();
		for (index, event) in events.iter().enumerate() {
			let updates = streams.event(frame(u64::try_from(index).unwrap() + 1, event.clone()));
			let Update::Delta { delta } = &updates[0] else {
				panic!("expected delta")
			};
			assert_eq!(serde_json::to_value(&delta.event).unwrap(), *event);
			assert_eq!(delta.role.as_deref(), Some("developer:general"));
		}
		let unknown: Event =
			serde_json::from_value(json!({"kind":"tool_result","tool_id":"one","content":"disk"})).unwrap();
		assert!(matches!(unknown, Event::ToolResult { success: None, .. }));
	}

	#[test]
	fn replay_replaces_chunks_and_deduplicates_both_sides_of_the_snapshot() {
		let mut streams = ready();
		streams.event(text(1, "old"));
		assert!(matches!(
			&streams.event(text(3, " there"))[0],
			Update::Recovering { .. }
		));
		streams.request("tap-general-abcdef", "replay".into());
		for (seq, content) in [(4, "!"), (2, "duplicate"), (3, " there"), (4, "!")] {
			assert!(streams.event(text(seq, content)).is_empty());
		}
		let mut log = snapshot(2, json!([{"kind":"assistant","content":"Hello"}]));
		log.truncated = true;
		let updates = streams.log(log).unwrap();
		let Update::Snapshot { run } = &updates[0] else {
			panic!("snapshot")
		};
		assert_eq!(run.seq, 4);
		assert!(run.truncated);
		assert_eq!(
			run.events,
			VecDeque::from([Event::Assistant {
				content: "Hello there!".into()
			}])
		);
		assert!(streams.event(text(4, "duplicate")).is_empty());
		assert_eq!(
			streams.event(frame(5, json!({"kind":"status","status":"done"}))).len(),
			1
		);
	}

	#[test]
	fn failed_replay_preserves_saved_transcript_and_marks_it_incomplete() {
		let mut streams = ready();
		streams.event(text(1, "kept"));
		streams.event(text(3, "gap"));
		streams.request("tap-general-abcdef", "replay".into());
		let mut log = snapshot(0, json!([]));
		log.error = Some("run not found".into());
		assert!(matches!(&streams.log(log).unwrap()[0], Update::Error { .. }));
		let run = streams.cache.get("tap-general-abcdef").unwrap();
		assert_eq!(
			run.events,
			VecDeque::from([Event::Assistant { content: "kept".into() }])
		);
		assert_eq!(run.seq, 1);
		assert!(!run.complete);
		assert_eq!(run.error.as_deref(), Some("Replay failed: run not found"));
		assert!(streams.requests().is_empty());
		let mut reconnect = Streams::default();
		reconnect.recover(streams.cache);
		assert_eq!(reconnect.requests(), ["tap-general-abcdef"]);
	}

	#[test]
	fn independent_runs_and_resumed_runs_keep_their_own_sequence() {
		let mut streams = ready();
		streams.event(frame(1, json!({"kind":"status","status":"done"})));
		let mut second = frame(1, json!({"kind":"status","status":"running"}));
		second.run_id = "tap-other-123456".into();
		assert_eq!(streams.event(second).len(), 1);
		assert_eq!(
			streams
				.event(frame(2, json!({"kind":"status","status":"running"})))
				.len(),
			1
		);
	}

	#[test]
	fn mismatched_replay_correlation_fails_without_consuming_the_pending_replay() {
		let mut streams = ready();
		streams.event(text(2, "missing baseline"));
		streams.request("tap-general-abcdef", "replay".into());
		let mut log = snapshot(2, json!([]));
		log.request_id = Some("wrong".into());
		assert!(streams
			.log(log)
			.unwrap_err()
			.to_string()
			.contains("request_id mismatch"));
		assert_eq!(
			streams.recovery["tap-general-abcdef"].request_id.as_deref(),
			Some("replay")
		);
	}

	#[test]
	fn terminal_gap_and_missing_snapshot_require_recovery() {
		let mut streams = ready();
		let mut sidebar = Cache::default();
		for update in streams.event(frame(1, json!({"kind":"status","status":"done"}))) {
			sidebar.apply(&update);
		}
		let updates = streams.event(frame(3, json!({"kind":"status","status":"done"})));
		assert!(matches!(&updates[0], Update::Recovering { .. }));
		for update in updates {
			sidebar.apply(&update);
		}
		assert_eq!(streams.requests(), ["tap-general-abcdef"]);
		assert_eq!(streams.cache.get("tap-general-abcdef").unwrap().seq, 1);
		assert!(!sidebar.get("tap-general-abcdef").unwrap().complete);
		// Losing the connection before replay returns must not lose the gap.
		let mut reconnect = Streams::default();
		reconnect.recover(sidebar);
		assert_eq!(reconnect.requests(), ["tap-general-abcdef"]);
		let mut fresh = ready();
		fresh.event(frame(9, json!({"kind":"status","status":"done"})));
		assert_eq!(fresh.requests(), ["tap-general-abcdef"]);
		assert!(!fresh.cache.get("tap-general-abcdef").unwrap().complete);
	}

	#[test]
	fn content_cap_trims_oldest_items_and_oversized_chunks() {
		let mut streams = ready();
		let mut sidebar = Cache::default();
		for frame in [
			frame(1, json!({"kind":"prompt","content":"a".repeat(MAX_RUN_CONTENT / 2)})),
			text(2, &"b".repeat(MAX_RUN_CONTENT / 2)),
			text(3, &"c".repeat(MAX_RUN_CONTENT)),
		] {
			for update in streams.event(frame) {
				sidebar.apply(&update);
			}
			let run = streams.cache.get("tap-general-abcdef").unwrap();
			assert!(run.bytes <= MAX_RUN_CONTENT);
			assert_eq!(run.bytes, run.events.iter().map(Event::bytes).sum::<usize>());
			assert_eq!(
				serde_json::to_value(run).unwrap(),
				serde_json::to_value(sidebar.get(&run.run_id).unwrap()).unwrap()
			);
		}
		let run = sidebar.get("tap-general-abcdef").unwrap();
		assert!(run.truncated);
		assert!(run.events.is_empty());
	}

	#[test]
	fn completed_runs_evict_oldest_and_keep_active_runs() {
		let mut streams = ready();
		let mut sidebar = Cache::default();
		for update in streams.event(frame(1, json!({"kind":"status","status":"running"}))) {
			sidebar.apply(&update);
		}
		let mut evicted = Vec::new();
		for index in 0..21 {
			let mut frame = frame(1, json!({"kind":"status","status":"done"}));
			frame.run_id = format!("run-{index}");
			for update in streams.event(frame) {
				evicted.extend(sidebar.apply(&update));
			}
		}
		assert_eq!(evicted, ["run-0"]);
		assert_eq!(streams.cache.values().filter(|run| run.status.terminal()).count(), 20);
		assert_eq!(sidebar.values().filter(|run| run.status.terminal()).count(), 20);
		assert!(sidebar.get("tap-general-abcdef").is_some());
		assert!(sidebar.get("run-20").is_some());
		assert!(streams.cache.get("run-0").is_none());
	}

	#[test]
	fn deltas_carry_only_new_content_and_apply_to_the_same_transcript() {
		let mut streams = ready();
		let mut sidebar = Cache::default();
		for update in streams.event(text(1, &"x".repeat(64 * 1024))) {
			sidebar.apply(&update);
		}
		let updates = streams.event(text(2, "tail"));
		assert!(serde_json::to_vec(&updates[0]).unwrap().len() < 512);
		let Update::Delta { delta } = &updates[0] else {
			panic!("delta")
		};
		assert!(delta.merged);
		assert_eq!(delta.event, Some(Event::Assistant { content: "tail".into() }));
		sidebar.apply(&updates[0]);
		assert_eq!(
			serde_json::to_value(sidebar.values().next().unwrap()).unwrap(),
			serde_json::to_value(streams.cache.values().next().unwrap()).unwrap()
		);
	}

	#[test]
	fn replay_cannot_regress_the_cached_sequence() {
		let mut streams = ready();
		streams.event(text(1, "saved"));
		streams.event(text(3, "missing middle"));
		streams.request("tap-general-abcdef", "replay".into());
		let updates = streams.log(snapshot(0, json!([]))).unwrap();
		assert!(matches!(&updates[0], Update::Error { error, .. } if error.contains("precedes cached sequence")));
		let saved = streams.cache.get("tap-general-abcdef").unwrap();
		assert_eq!(saved.seq, 1);
		assert_eq!(
			saved.events[0],
			Event::Assistant {
				content: "saved".into()
			}
		);
	}

	#[test]
	fn bind_buffer_is_bounded_and_stale_snapshots_cannot_replace_saved_content() {
		let mut previous = ready();
		previous.event(text(1, "saved"));
		let mut streams = Streams::default();
		for seq in 3..7 {
			assert!(streams.event(text(seq, &"x".repeat(MAX_RUN_CONTENT / 2))).is_empty());
		}
		assert!(streams.recovery["tap-general-abcdef"].buffer.bytes <= MAX_RUN_CONTENT);
		streams.recover(previous.cache);
		streams.request("tap-general-abcdef", "replay".into());
		assert!(streams
			.log(snapshot(2, json!([{"kind":"assistant","content":"partial"}])))
			.unwrap()
			.is_empty());
		assert_eq!(
			streams.cache.get("tap-general-abcdef").unwrap().events[0],
			Event::Assistant {
				content: "saved".into()
			}
		);
		assert_eq!(streams.requests(), ["tap-general-abcdef"]);
	}
}
