//! One-shot mail AI jobs (spec AI-1, AI-2, AI-3, AI-5, AI-6): a specialist with no tools
//! reads mail and answers in text, and the app acts on the answer. Mail comes
//! from anyone, so nothing in it reaches an agent that can browse, read tabs
//! or touch the mailbox — the assistant only ever gets what the user types.
//!
//! The engine (`mail.rs`) builds the prompt and names the job; this runs it on
//! the main thread and reports its text as it grows. One run per kind (per
//! reply for drafts): a new one replaces a run still going.

use crate::acp::{AcpHandle, AgentEvent};
use crate::mail::{self, Job};

struct Run {
    job: Job,
    handle: AcpHandle,
    pid: Option<u32>,
    response: String,
}

/// What a run said since the last poll: its whole answer so far, and how it
/// ended once it has.
pub struct Progress {
    pub job: Job,
    pub response: String,
    pub ended: Option<Result<(), String>>,
}

#[derive(Default)]
pub struct Jobs {
    runs: Vec<Run>,
}

impl Jobs {
    /// Start `job` on its agent with `prompt`, in place of any run in its slot.
    pub fn start(
        &mut self,
        job: Job,
        prompt: String,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<(), String> {
        self.stop(|j| j.same_slot(&job));
        let handle = AcpHandle::connect(&format!("octomind acp {}", job.agent()), None, wake)
            .map_err(|e| format!("The AI could not start: {e}"))?;
        if !handle.send_prompt(prompt, vec![]) {
            return Err("The AI closed before it started.".into());
        }
        self.runs.push(Run {
            job,
            handle,
            pid: None,
            response: String::new(),
        });
        Ok(())
    }

    /// Drop the runs whose job matches, killing their processes.
    pub fn stop(&mut self, matches: impl Fn(&Job) -> bool) {
        self.runs.retain(|r| !matches(&r.job));
    }

    pub fn running(&self, matches: impl Fn(&Job) -> bool) -> bool {
        self.runs.iter().any(|r| matches(&r.job))
    }

    /// Agent processes still running, for a clean quit.
    pub fn pids(&self) -> Vec<u32> {
        self.runs.iter().filter_map(|r| r.pid).collect()
    }

    /// Drain every run's events; ended runs are dropped.
    pub fn poll(&mut self) -> Vec<Progress> {
        let mut out = Vec::new();
        self.runs.retain_mut(|run| {
            let mut ended = None;
            let mut grew = false;
            for ev in run.handle.poll() {
                match ev {
                    AgentEvent::ProcessPid(pid) => run.pid = Some(pid),
                    AgentEvent::Chunk(chunk) => {
                        run.response.push_str(&chunk);
                        grew = true;
                    }
                    AgentEvent::Done => ended = Some(Ok(())),
                    AgentEvent::Error(e) => ended = Some(Err(e)),
                    _ => {}
                }
            }
            if grew || ended.is_some() {
                out.push(Progress {
                    job: run.job.clone(),
                    response: run.response.clone(),
                    ended: ended.clone(),
                });
            }
            ended.is_none()
        });
        out
    }
}

/// What the app does with a run's progress.
#[derive(Default)]
pub struct Outcome {
    /// For the mail page.
    pub script: Option<String>,
    /// For the engine.
    pub command: Option<mail::Command>,
    /// For the user, as a toast.
    pub toast: Option<String>,
    /// A reply finished writing: the draft-ready sound.
    pub ready: bool,
}

/// How a job that did not finish shows: `message` where the user asked for it.
pub fn failed(job: &Job, message: String) -> Outcome {
    let script = |name: &str, data: serde_json::Value| Outcome {
        script: Some(mail::script(name, &data)),
        ..Default::default()
    };
    match job {
        Job::Draft { key } => script(
            "draftFailed",
            serde_json::json!({ "key": key, "message": message }),
        ),
        Job::Summary { item } => script(
            "summaryFailed",
            serde_json::json!({ "item": item, "message": message }),
        ),
        Job::Search { ws, id } => script(
            "searchFailed",
            serde_json::json!({ "ws": ws, "id": id, "message": message }),
        ),
        Job::Tasks { .. } => Outcome {
            toast: Some(message),
            ..Default::default()
        },
        // In the background: nobody is waiting for it.
        Job::Summaries { .. } => {
            tracing::warn!(error = %message, "mail summaries run failed");
            Outcome::default()
        }
    }
}

pub fn outcome(p: Progress) -> Outcome {
    let block = mail::text_block(&p.response);
    let ended = match p.ended {
        Some(Err(e)) => return failed(&p.job, format!("The AI stopped: {e}")),
        Some(Ok(())) => true,
        None => false,
    };
    // Finished without its block: whatever it said instead (a question, a
    // refusal) is the message.
    if ended
        && block.is_none_or(|(t, _)| t.trim().is_empty() && !matches!(p.job, Job::Tasks { .. }))
    {
        let said: String = p.response.trim().chars().take(300).collect();
        return failed(
            &p.job,
            if said.is_empty() {
                "The AI returned nothing.".into()
            } else {
                said
            },
        );
    }
    let script = |name: &str, data: serde_json::Value| Outcome {
        script: Some(mail::script(name, &data)),
        ready: ended && name == "draftStream",
        ..Default::default()
    };
    match (p.job, block) {
        (Job::Draft { key }, Some((text, _))) => script(
            "draftStream",
            serde_json::json!({ "key": key, "text": text, "done": ended }),
        ),
        (Job::Summary { item }, Some((text, _))) => script(
            "summaryStream",
            serde_json::json!({ "item": item, "text": text, "done": ended }),
        ),
        (Job::Search { ws, id }, Some((text, true)))
            if ended && text.trim().lines().count() == 1 =>
        {
            script(
                "searchReady",
                serde_json::json!({ "ws": ws, "id": id, "query": text.trim() }),
            )
        }
        (job @ Job::Search { .. }, _) if ended => {
            failed(&job, "The AI did not return a single search query.".into())
        }
        (Job::Summaries { accounts, items }, _) if ended => {
            let summaries = mail::parse_summaries(&items, &p.response);
            tracing::info!(
                asked = items.len(),
                got = summaries.len(),
                "mail summaries run ended"
            );
            Outcome {
                command: Some(mail::Command::Summaries {
                    accounts,
                    summaries,
                }),
                ..Default::default()
            }
        }
        (Job::Tasks { ws, accounts, item }, _) if ended => Outcome {
            command: Some(mail::Command::Tasks {
                ws,
                accounts,
                item,
                tasks: mail::parse_tasks(&p.response),
            }),
            ..Default::default()
        },
        _ => Outcome::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::{Command, Item};

    fn item() -> Item {
        Item {
            account: "acc".into(),
            thread: "t".into(),
        }
    }

    fn run(job: Job, response: &str, ended: Option<Result<(), String>>) -> Outcome {
        outcome(Progress {
            job,
            response: response.into(),
            ended,
        })
    }

    #[test]
    fn drafts_for_different_replies_run_side_by_side() {
        let draft = |k: &str| Job::Draft { key: k.into() };
        assert!(draft("reply:a:1").same_slot(&draft("reply:a:1")));
        assert!(!draft("reply:a:1").same_slot(&draft("reply:a:2")));
        let summary = |t: &str| Job::Summary {
            item: Item {
                account: "a".into(),
                thread: t.into(),
            },
        };
        assert!(summary("1").same_slot(&summary("2")));
        assert!(!draft("reply:a:1").same_slot(&summary("1")));
    }

    #[test]
    fn replies_and_summaries_stream_to_the_page() {
        let draft = || Job::Draft { key: "k".into() };
        let o = run(draft(), "<text>\nHi Ann", None);
        assert!(o.script.unwrap().contains("draftStream") && !o.ready);
        let o = run(draft(), "<text>\nHi Ann\n</text>", Some(Ok(())));
        assert!(o.script.unwrap().contains(r#""done":true"#) && o.ready);
        // Done without the block: what it said instead is the error.
        let o = run(draft(), "I can't help with that.", Some(Ok(())));
        assert!(o.script.unwrap().contains("I can't help with that."));
        let o = run(Job::Summary { item: item() }, "<text>\nAnna asks", None);
        assert!(o.script.unwrap().contains("summaryStream"));
        let o = run(Job::Summary { item: item() }, "", Some(Err("gone".into())));
        assert!(o.script.unwrap().contains("summaryFailed"));
        assert!(run(draft(), "thinking", None).script.is_none());
    }

    #[test]
    fn search_waits_for_a_complete_query_and_preserves_its_request() {
        let search = || Job::Search {
            ws: "work".into(),
            id: 7,
        };
        assert_eq!(search().agent(), "octoweb:mail-reader");
        assert!(search().same_slot(&Job::Search {
            ws: "other".into(),
            id: 8
        }));
        assert!(!search().same_slot(&Job::Summary { item: item() }));
        assert!(run(search(), "<text>from:Anna", None).script.is_none());
        assert!(run(search(), "<text>from:Anna</text>", None)
            .script
            .is_none());
        let o = run(search(), "<text>\nfrom:Anna invoice\n</text>", Some(Ok(())));
        assert_eq!(
            o.script,
            Some(mail::script(
                "searchReady",
                &serde_json::json!({
                    "ws": "work", "id": 7, "query": "from:Anna invoice"
                })
            ))
        );
        assert!(o.command.is_none() && !o.ready);
        for response in [
            "<text>from:Anna",
            "<text>from:Anna\ninvoice</text>",
            "<text></text>",
            "Sorry.",
        ] {
            let o = run(search(), response, Some(Ok(())));
            let script = o.script.unwrap();
            assert!(script.contains("searchFailed"), "{script}");
            assert!(script.contains(r#""ws":"work""#) && script.contains(r#""id":7"#));
            assert!(o.command.is_none());
        }
        let o = run(search(), "", Some(Err("offline".into())));
        assert!(o.script.unwrap().contains("The AI stopped: offline"));
        let o = failed(&search(), "The AI could not start".into());
        assert!(o.script.unwrap().contains("searchFailed"));
    }

    #[test]
    fn the_app_saves_summaries_and_tasks_itself() {
        let summaries = Job::Summaries {
            accounts: vec!["acc".into()],
            items: vec![item()],
        };
        let o = run(summaries, "<text>\n1: Anna asks\n</text>", Some(Ok(())));
        assert!(
            matches!(o.command, Some(Command::Summaries { summaries, .. }) if summaries.len() == 1)
        );
        let tasks = || Job::Tasks {
            ws: "w".into(),
            accounts: vec!["acc".into()],
            item: item(),
        };
        let o = run(tasks(), "<text>\n- Pay\n</text>", Some(Ok(())));
        assert!(matches!(o.command, Some(Command::Tasks { tasks, .. }) if tasks == ["Pay"]));
        // An empty answer is "no tasks"; no answer at all is an error.
        let o = run(tasks(), "<text>\n</text>", Some(Ok(())));
        assert!(matches!(o.command, Some(Command::Tasks { tasks, .. }) if tasks.is_empty()));
        let o = run(tasks(), "Sorry.", Some(Ok(())));
        assert_eq!(o.toast.as_deref(), Some("Sorry."));
        assert!(o.command.is_none());
    }
}
