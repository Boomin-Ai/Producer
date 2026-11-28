//! A private GPT-Live conversation owned by the native Assistant.
mod log;
mod shortcuts;
use log::TurnLog;
use super::*;
use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr::NonNull;

extern "C" {
    fn producer_voice_open(config: *const c_char) -> *mut c_void;
    fn producer_voice_close(session: *mut c_void);
    fn producer_voice_next(session: *mut c_void) -> *mut c_char;
    fn producer_voice_send(session: *mut c_void, event: *const c_char);
}

struct Connection(NonNull<c_void>);

impl Drop for Connection {
    fn drop(&mut self) {
        unsafe { producer_voice_close(self.0.as_ptr()) }
    }
}

impl Connection {
    fn send(&self, event: Value) {
        let encoded = CString::new(event.to_string()).unwrap();
        unsafe { producer_voice_send(self.0.as_ptr(), encoded.as_ptr()) }
    }

    fn next(&self) -> Option<Value> {
        let ptr = NonNull::new(unsafe { producer_voice_next(self.0.as_ptr()) })?;
        let event = unsafe { serde_json::from_slice(CStr::from_ptr(ptr.as_ptr()).to_bytes()) };
        unsafe { libc::free(ptr.as_ptr().cast()) };
        event.ok()
    }

    fn reply(&self, call: &str, result: String) {
        self.send(
            json!({"type":"response.item.create","item":{"type":"function_call_output",
            "call_id":call,"output":result}}),
        );
        self.send(json!({"type":"response.create"}));
    }
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type":"function","name":name,"description":description,"strict":true,
        "parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
}

fn local_clock() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let mut buffer = [0i8; 64];
    unsafe {
        if libc::localtime_r(&now, &mut tm).is_null() ||
            libc::strftime(buffer.as_mut_ptr(), buffer.len(), c"%A %Y-%m-%d %H:%M %Z".as_ptr(), &tm) == 0 {
            return "unavailable".into();
        }
        CStr::from_ptr(buffer.as_ptr()).to_string_lossy().into_owned()
    }
}

fn config() -> Value {
    let surfaces: Vec<_> = ["boominSignIn", "publishingAccounts", "newPost"].into_iter()
        .chain(PanelId::launch_panels().map(PanelId::key)).collect();
    let tools = vec![
        tool("operate_shortcut", "Execute a requested studio shortcut. Mute affects the program mic. Switch the on-air layout, edit its headline, save or toggle rolling clips, or play an exact studio sound.", json!({"action":{"type":"string","enum":shortcuts::ACTIONS},"value":{"type":"string","description":"New headline, layout or sound ID/name. Empty for actions without a value."}}), &["action","value"]),
        tool("open_surface", "Open the requested native modal or studio panel. Use boominSignIn for sign-in, publishingAccounts to connect Threads and newPost for the manual composer.", json!({"surface":{"type":"string","enum":surfaces}}), &["surface"]),
        tool("inspect_studio", "Read current program, visible panels, publishing readiness and the current voice draft.", json!({}), &[]),
        tool("review_threads", "Read the saved voice draft and resolve the destination after signing in or choosing a Threads account. Use an empty account to keep the previous choice.", json!({"account":{"type":"string"}}), &["account"]),
        tool("draft_threads", "Create or replace the current Threads draft. Give the complete revised text. Include the latest saved clip only when requested.",json!({"text":{"type":"string"},"latest_clip":{"type":"boolean"},"account":{"type":"string","description":"Requested Threads handle, or empty to use the only connected account"}}), &["text","latest_clip","account"]),
        tool("schedule_draft", "Schedule the reviewed Threads revision only when the operator asks for a specific future time. Pass YYYY-MM-DD HH:MM in the operator's local time.",json!({"revision":{"type":"string"},"local_time":{"type":"string"}}), &["revision","local_time"]),
        tool("publish_draft", "Queue the current reviewed draft only after the operator explicitly asks to publish it. Pass the revision returned by draft_threads.",json!({"revision":{"type":"string"}}), &["revision"]),
        tool("studio_request","Send the operator's request to the existing Producer studio assistant.",json!({"prompt":{"type":"string"}}), &["prompt"]),
    ];
    let manifest = tools.iter().map(|t| format!("- {}: {}", t["name"].as_str().unwrap(), t["description"].as_str().unwrap())).collect::<Vec<_>>().join("\n");
    let instructions = format!("Studio local time when this session opened: {}.\n{}\nCurrently registered Producer actions:\n{manifest}", local_clock(), include_str!("duce_voice_prompt.txt"));
    json!({"model":"gpt-live-1.1","instructions":instructions,
        "audio":{"format":{"type":"audio/pcm","rate":24000},"output":{"voice":"marin"}},
        "delegation":{"type":"responses","responses":{"model":"gpt-5.6-luna","tools":tools,"parallel_tool_calls":false,
        "instructions":"Use tools on behalf of the speaking operator. Preserve their wording and tone. For a draft with no account specified, call draft_threads with an empty account so Producer can resolve it. Use review_threads after sign-in, connection or account selection. Ask the operator to enter credentials in the native form. Open native surfaces on request. Inspect studio state before guessing. Route design, input, scene, layout, audio and workspace changes through studio_request for native validation and review. Treat revision as an opaque identifier and never read it aloud. Only call publish_draft after an explicit spoken instruction to publish the current draft."}}})
}

#[derive(Default)]
pub(super) struct Voice {
    connection: Option<Connection>,
    pub status: String,
    draft: Option<publish::VoiceDraft>,
    seen: HashSet<String>,
    log: Option<TurnLog>,
    rows: [Option<usize>; 2],
    input_end_ms: i64,
    first_reply_ms: Option<i64>,
    pending: Option<PendingAction>,
}

struct PendingAction {
    call: String,
    name: String,
    success: String,
    rx: mpsc::Receiver<Result<()>>,
    log: Option<TurnLog>,
    started: Instant,
}

impl Voice {
    pub(super) fn listening(&self) -> bool {
        self.connection.is_some()
    }
}

pub(super) fn control(s: &Studio, compact: bool) -> El {
    let on = s.chat.voice.connection.is_some();
    super::panels::tool(
        if compact { "duce-quick" } else { "duce-voice" },
        if on { "stop" } else { "mic" },
        if on {
            "End Duce voice session"
        } else {
            "Talk to Duce"
        },
        on,
        move |s| {
            if s.chat.voice.connection.is_some() {
                s.chat.voice = Voice::default();
            } else {
                let cfg = CString::new(config().to_string()).unwrap();
                s.chat.voice = Voice {
                    connection: NonNull::new(unsafe { producer_voice_open(cfg.as_ptr()) })
                        .map(Connection),
                    status: "Connecting to Duce…".into(),
                    ..Default::default()
                };
                if compact {
                    s.show_chat = true;
                }
            }
        },
    )
    .size(28., 28.)
}

fn open_accounts(s: &mut Studio) -> Result<&'static str> {
    use quick_controls::PublishPopout;
    match &s.quick_controls.publish {
        None | Some(PublishPopout::DuceAccounts) => {
            s.quick_controls.publish = Some(PublishPopout::DuceAccounts);
        }
        Some(PublishPopout::SignIn { .. }) => return Ok("The Boomin sign-in form is open"),
        Some(PublishPopout::Composer) => bail!("Close the current post composer to open publishing accounts"),
    }
    Ok(if s.publish.scope.is_none() {
        "The Boomin sign-in form is open. Enter credentials directly in Producer."
    } else {
        "Publishing accounts is open. Connect or refresh Threads there."
    })
}

fn review_draft(s: &mut Studio, account: &str) -> Result<String> {
    let mut draft = s.chat.voice.draft.take().context("Prepare a draft first")?;
    let result = draft.review(s, account);
    let text = draft.text.clone();
    s.chat.voice.draft = Some(draft);
    match result {
        Ok(result) => {
            if matches!(s.quick_controls.publish, Some(quick_controls::PublishPopout::DuceAccounts)) {
                quick_controls::dismiss_publish(s);
            }
            Ok(result)
        }
        Err(error) => {
            let mut message = format!("Draft held in this voice session: {text}\n{error}.");
            if !publish::has_threads(s) {
                match open_accounts(s) {
                    Ok(status) => message.push_str(&format!(" {status}")),
                    Err(error) => message.push_str(&format!(" {error}.")),
                }
            }
            Ok(format!("{message} Review this draft after connecting or choosing an account."))
        }
    }
}

fn open_surface(s: &mut Studio, surface: &str) -> Result<String> {
    match surface {
        "boominSignIn" if s.publish.scope.is_some() => Ok("Already signed in to Boomin".into()),
        "boominSignIn" | "publishingAccounts" => Ok(open_accounts(s)?.into()),
        "newPost" => {
            ensure!(s.publish.can_open_composer(), "Publishing is busy");
            if !quick_controls::publish_open(s) {
                quick_controls::open_publish(s, None);
            }
            match s.quick_controls.publish {
                Some(quick_controls::PublishPopout::Composer) => Ok("New post composer is open".into()),
                Some(_) if s.publish.scope.is_none() => Ok("Boomin sign-in form is open".into()),
                _ => bail!("Close publishing accounts before opening a new post"),
            }
        }
        name => {
            let panel = PanelId::launch_panels().find(|p| p.key() == name).context("Unknown studio surface")?;
            let live = s.live.as_mut().context("Open a studio first")?;
            live.handle.send(RoomCommand::Workspace { command: WorkspaceCommand::Activate { panel } })?;
            live.updated = Instant::now() - Duration::from_secs(1);
            Ok(format!("Requested the {} panel", panel.label()))
        }
    }
}

pub(super) fn run(s: &mut Studio, name: &str, args: &Value, call: &str) -> Result<Option<String>> {
    match name {
        "open_surface" => Ok(Some(open_surface(s, args["surface"].as_str().context("Missing surface")?)?)),
        "inspect_studio" => {
            let live = s.live.as_ref().context("Open a studio first")?;
            let snap = &live.snapshot;
            let panels: Vec<_> = PanelId::launch_panels()
                .filter(|p| snap.project.workspace.visible(*p)).map(PanelId::key).collect();
            let layouts: Vec<_> = snap.project.mounted_set.iter().flat_map(|id| snap.project.set_layout_choices(id))
                .map(|choice| json!({"label":choice.label,"id":choice.pair.landscape})).collect();
            let sounds: Vec<_> = snap.project.workspace.shortcuts().filter_map(|pin| {
                if let crate::workspace::QuickControl::Sound(id) = pin {
                    Some(json!({"id":id,"name":live.handle.studio_audio.sound_name(id)}))
                } else { None }
            }).collect();
            Ok(Some(json!({"localTime":local_clock(),"layouts":layouts,"pinnedSounds":sounds,"program": snap.program_kind, "mountedSet":snap.project.mounted_set,
                "layout":snap.program_layout, "recording":snap.recording, "visiblePanels":panels,
                "boominSignedIn":s.publish.scope.is_some(), "threadsAvailable":publish::has_threads(s),
                "draft":s.chat.voice.draft.as_ref().map(|d| &d.text)}).to_string()))
        }
        "operate_shortcut" => {
            let live = s.live.as_ref().context("Open a studio first")?;
            let action = args["action"].as_str().context("Missing shortcut action")?;
            let cmd = shortcuts::command(live, action, args["value"].as_str().unwrap_or("").trim())?;
            let handle = live.handle.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let result = handle.request(cmd).and_then(|reply| {
                    ensure!(reply["ok"] == true, "{}", reply["error"].as_str().unwrap_or("Studio rejected the shortcut"));
                    Ok(())
                });
                let _ = tx.send(result);
            });
            s.chat.voice.pending = Some(PendingAction { call: call.into(), name: name.into(),
                success: format!("Producer accepted {action}"), rx, log: s.chat.voice.log.clone(), started: Instant::now() });
            Ok(None)
        }
        "draft_threads" => {
            let draft = publish::VoiceDraft::prepare(s, args["text"].as_str().context("Missing draft")?,
                args["account"].as_str().unwrap_or(""), args["latest_clip"] == true)?;
            s.chat.voice.draft = Some(draft);
            Ok(Some(review_draft(s, "")?))
        }
        "review_threads" => Ok(Some(review_draft(s, args["account"].as_str().unwrap_or(""))?)),
        "publish_draft" | "schedule_draft" => {
            let local_time = if name == "schedule_draft" {
                Some(args["local_time"].as_str().filter(|t| !t.is_empty())
                    .context("Choose a future local date and time")?.to_owned())
            } else { None };
            let draft = s
                .chat
                .voice
                .draft
                .as_ref()
                .context("Prepare a draft first")?;
            ensure!(
                args["revision"].as_str() == Some(draft.revision.as_str()),
                "Review the current draft first"
            );
            let rx = match publish::queue_voice(s, draft, local_time.clone()) {
                Ok(rx) => rx,
                Err(error) if !publish::has_threads(s) => {
                    let status = open_accounts(s)?;
                    bail!("{error}. {status} Review the draft before publishing.");
                }
                Err(error) => return Err(error),
            };
            let success = match local_time {
                Some(t) => format!("Queued for scheduled delivery to Threads at {t} local time"),
                None => "Queued for Threads".to_owned(),
            };
            s.chat.voice.pending = Some(PendingAction { call: call.into(), name: name.into(),
                success, rx, log: s.chat.voice.log.clone(), started: Instant::now() });
            Ok(None)
        }
        "studio_request" => {
            ensure!(
                !s.chat.busy() && s.chat.input.text.is_empty(),
                "Assistant is busy or has an unsent message"
            );
            let prompt = args["prompt"]
                .as_str()
                .filter(|t| !t.trim().is_empty())
                .context("Missing request")?;
            s.chat.input = TextEdit::new(prompt);
            chat::submit(s, false);
            ensure!(
                s.chat.busy(),
                "Check the Assistant account and request status"
            );
            Ok(Some(
                "Studio Assistant started the request. Its progress appears in the conversation."
                    .into(),
            ))
        }
        _ => bail!("Unknown Producer action"),
    }
}

fn ensure_log(s: &mut Studio) {
    if s.chat.voice.log.is_none() {
        if let Some(live) = &s.live {
            match TurnLog::start(&live.handle.root, s.chat.history.len()) {
                Ok(log) => s.chat.voice.log = Some(log),
                Err(error) => s.chat.voice.status = format!("Voice logging unavailable: {error}"),
            }
        }
    }
}

fn transcript(s: &mut Studio, event: &Value) {
    let Some(delta) = event["delta"].as_str().filter(|v| !v.is_empty()) else { return };
    let input = event["type"] == "session.input_transcript.delta";
    let side = usize::from(!input);
    let start = event["start_ms"].as_i64();
    let end = event["end_ms"].as_i64();
    // Captions can arrive interleaved. Retain overlapping input on its original row.
    if input && s.chat.voice.rows[0].is_some() && s.chat.voice.rows[1].is_some()
        && start.zip(s.chat.voice.first_reply_ms).is_none_or(|(t, reply)| t >= reply && t >= s.chat.voice.input_end_ms + 350) {
        s.chat.voice.log = None;
        s.chat.voice.rows = [None; 2];
        s.chat.voice.input_end_ms = 0;
        s.chat.voice.first_reply_ms = None;
    }
    ensure_log(s);
    let role = if input { "user" } else { "assistant" };
    if let Some(row) = s.chat.voice.rows[side] {
        s.chat.history[row].1.push_str(delta);
    } else {
        let row = s.chat.history.len();
        s.chat.history.push((role.into(), delta.into()));
        s.chat.voice.rows[side] = Some(row);
        if input {
            if let Some(log) = s.chat.voice.log.clone() {
                s.chat.attach_voice_receipt(row, log.receipt);
            }
        }
    }
    if input {
        s.chat.voice.input_end_ms = s.chat.voice.input_end_ms.max(end.unwrap_or_default());
    } else if s.chat.voice.first_reply_ms.is_none() {
        s.chat.voice.first_reply_ms = start;
        if let (Some(log), Some(start)) = (&s.chat.voice.log, start) {
            log.record("voice_first_reply_caption", json!({"startMs":start,
                "previousInputEndMs":s.chat.voice.input_end_ms,
                "gapMs":start - s.chat.voice.input_end_ms}));
        }
    }
    if let Some(log) = &s.chat.voice.log {
        log.record("voice_transcript", json!({"role":role,"text":delta,"startMs":start,"endMs":end}));
    }
}

pub(super) fn poll(s: &mut Studio) -> bool {
    let mut changed = false;
    if let Some(pending) = &s.chat.voice.pending {
        let result = match pending.rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err(anyhow::anyhow!("Producer action interrupted")))
            }
            _ => None,
        };
        if let Some(result) = result {
            let pending = s.chat.voice.pending.take().unwrap();
            if let Some(log) = &pending.log {
                log.result(&pending.name, &pending.call, pending.started, result.as_ref().err().map(ToString::to_string));
                if result.is_ok() {
                    log.record("voice_tool_output", json!({"tool":pending.name,"callId":pending.call,"output":pending.success}));
                }
            }
            let status = match result {
                Ok(()) => {
                    if matches!(pending.name.as_str(), "publish_draft" | "schedule_draft") {
                        s.chat.voice.draft = None;
                        s.publish.refresh_voice_queue();
                    }
                    pending.success
                }
                Err(e) => e.to_string(),
            };
            if let Some(c) = &s.chat.voice.connection {
                c.reply(&pending.call, status.clone());
            }
            s.chat.voice.status = status;

            changed = true;
        }
    }
    while let Some(event) = s.chat.voice.connection.as_ref().and_then(Connection::next) {
        changed = true;
        match event["type"].as_str() {
            Some("listening") => s.chat.voice.status = "Duce is listening".into(),
            Some("failure") => {
                s.chat.voice.status = event["message"]
                    .as_str()
                    .unwrap_or("Voice session ended")
                    .into();
                ensure_log(s);
                if let Some(log) = s.chat.voice.log.take() {
                    log.record("voice_connection_ended", json!({"error":s.chat.voice.status}));
                }
                s.chat.voice.connection = None;
            }
            Some("session.input_transcript.delta" | "session.output_transcript.delta") => {
                transcript(s, &event);
            }
            Some("response.event") if event["event"]["type"] == "response.created" => {
                ensure_log(s);
                if let Some(log) = &s.chat.voice.log {
                    log.receipt.calls.fetch_add(1, Ordering::Relaxed);
                    log.record("voice_delegation", json!({"delegationId":event["delegation_id"]}));
                }
            }
            Some("response.event") if event["event"]["type"] == "response.failed" => {
                s.chat.voice.status = event["event"]["response"]["error"]["message"]
                    .as_str()
                    .unwrap_or("Duce could not complete the action")
                    .to_string();
                ensure_log(s);
                if let Some(log) = &s.chat.voice.log {
                    log.record("voice_delegation_failed", json!({"error":s.chat.voice.status}));
                }
            }
            Some("response.event") if event["event"]["type"] == "response.output_item.done" => {
                let item = &event["event"]["item"];
                if item["type"] != "function_call" {
                    continue;
                }
                let Some(call) = item["call_id"].as_str() else {
                    continue;
                };
                if !s.chat.voice.seen.insert(call.to_owned()) {
                    continue;
                }
                ensure_log(s);
                let log = s.chat.voice.log.clone();
                let tool = item["name"].as_str().unwrap_or("unknown");
                let started = Instant::now();
                if let Some(log) = &log {
                    log.receipt.step(tool);
                    log.record("voice_tool_call", json!({"tool":tool,"callId":call,
                        "delegationId":event["delegation_id"]}));
                }
                let result = if s.chat.voice.pending.is_some() && matches!(item["name"].as_str(), Some("draft_threads" | "review_threads" | "publish_draft" | "schedule_draft" | "operate_shortcut")) {
                    Err(anyhow::anyhow!("Another Producer action is pending"))
                } else {
                    serde_json::from_str(item["arguments"].as_str().unwrap_or(""))
                        .context("Invalid tool arguments")
                        .and_then(|args| run(s, item["name"].as_str().unwrap_or(""), &args, call))
                };
                if let Some(log) = &log {
                    if !matches!(result, Ok(None)) {
                        log.result(tool, call, started, result.as_ref().err().map(ToString::to_string));
                        if let Ok(Some(output)) = &result {
                            log.record("voice_tool_output", json!({"tool":tool,"callId":call,"output":output}));
                        }
                    }
                }
                if let Some(reply) = result.unwrap_or_else(|e| Some(e.to_string())) {
                    s.chat.voice.status = if matches!(item["name"].as_str(), Some("draft_threads" | "review_threads"))
                        && s.chat
                            .voice
                            .draft
                            .as_ref()
                            .is_some_and(|d| reply.contains(&d.revision))
                    {
                        s.chat
                            .voice
                            .draft
                            .as_ref()
                            .map(publish::VoiceDraft::summary)
                            .unwrap_or_default()
                    } else {
                        reply.clone()
                    };
                    if let Some(c) = &s.chat.voice.connection {
                        c.reply(call, reply);
                    }
                }
            }
            _ => {}
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_capabilities_are_advertised_from_registered_tools() {
        let cfg = config();
        let instructions = cfg["instructions"].as_str().unwrap();
        for tool in cfg["delegation"]["responses"]["tools"].as_array().unwrap() {
            assert!(instructions.contains(tool["name"].as_str().unwrap()));
        }
        assert!(instructions.contains("schedule_draft"));
        assert!(instructions.contains("queued"));
    }

    #[test]
    fn voice_log_is_a_normal_turn_receipt_and_keeps_tool_timing() {
        let scratch = tempfile::tempdir().unwrap();
        let mut s = Studio::new(scratch.path().into());
        let log = TurnLog::start(scratch.path(), 0).unwrap();
        s.chat.voice.log = Some(log.clone());
        transcript(&mut s, &json!({"type":"session.input_transcript.delta","start_ms":0,"end_ms":400,"delta":"A draft"}));
        transcript(&mut s, &json!({"type":"session.output_transcript.delta","start_ms":610,"end_ms":940,"delta":"Okay"}));
        let mut rt = rune::Runtime::new(s, |s, _| chat::panel(s, true), (410., 590.), rune::Env::default());
        rt.frame(0.);
        assert!(rt.find("assistant-turn-log-0").is_some());
        log.result("schedule_draft", "call-1", Instant::now(), Some("Choose a future time".into()));
        let path = crate::assistant_log::directory(scratch.path()).join(format!("{}.jsonl", log.receipt.journal.as_ref().unwrap().id));
        drop(log);
        rt.state.chat.voice = Voice::default();
        let started = Instant::now();
        loop {
            let journal = std::fs::read_to_string(&path).unwrap();
            if journal.contains("turn_finished") {
                assert!(journal.contains("A draft") && journal.contains("Okay"));
                assert!(journal.contains("\"gapMs\":210") && journal.contains("Choose a future time"));
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
