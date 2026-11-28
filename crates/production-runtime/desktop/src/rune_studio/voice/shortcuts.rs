use super::*;

pub(super) const ACTIONS: &[&str] = &[
    "muteMic", "unmuteMic", "enableClips", "disableClips", "saveClip",
    "setHeadline", "switchLayout", "playSound",
];

pub(super) fn command(live: &Live, action: &str, value: &str) -> Result<RoomCommand> {
    let snap = &live.snapshot;
    let project = &snap.project;
    match action {
        "muteMic" | "unmuteMic" => {
            let lane = project.lanes.iter().find(|l| l.source == "microphone").context("Studio has no mic lane")?;
            Ok(RoomCommand::Gain { source: lane.source.clone(), gain: lane.gain, muted: action == "muteMic" })
        }
        "enableClips" | "disableClips" | "saveClip" => {
            let (status, _) = live.clip_view.as_ref().context("Clips unavailable")?;
            ensure!(!status.saving, "A clip is already saving");
            if action == "saveClip" {
                ensure!(status.enabled && status.ready, "Enable Clips and wait for the buffer to be ready");
                Ok(RoomCommand::ClipsSave)
            } else {
                Ok(RoomCommand::ClipsEnable { enabled: action == "enableClips" })
            }
        }
        "playSound" => {
            let desk = &live.handle.studio_audio;
            let id = if desk.sound_name(value).is_some() {
                value.to_owned()
            } else {
                let page = desk.room_sounds(value, 0, 256);
                let mut matches = page.items.into_iter().filter(|s| s.name.eq_ignore_ascii_case(value));
                let sound = matches.next().context("Choose a sound from this studio")?;
                ensure!(matches.next().is_none(), "Multiple sounds have that name. Choose the exact sound ID");
                sound.id
            };
            Ok(RoomCommand::StudioAudio { command: crate::studio_audio::Command::Trigger { id, mode: None } })
        }
        "setHeadline" => {
            ensure!(!value.is_empty(), "Supply the headline text");
            let (id, pair, field) = quick_controls::on_air_headline(snap).context("The on-air layout has no editable headline")?;
            let expected = project.production.sets[&id].clone();
            let mut draft = expected.clone();
            field.apply(&mut draft, &pair, value)?;
            Ok(RoomCommand::SetEditSave { id, expected, draft, layout: Some(pair) })
        }
        "switchLayout" => {
            ensure!(matches!(snap.program_kind.as_str(), "show" | "set"), "Take a set or show first");
            let id = project.mounted_set.as_ref().context("Take a set first")?;
            let mut choices = project.set_layout_choices(id).into_iter()
                .filter(|l| l.label.eq_ignore_ascii_case(value) || l.pair.landscape == value);
            let pair = choices.next().context("Choose a layout from the current on-air set")?.pair;
            ensure!(choices.next().is_none(), "Multiple layouts match. Choose the layout ID");
            if snap.program_kind == "show" {
                let (show, segment) = snap.program_segment.clone().context("No active show segment")?;
                Ok(RoomCommand::ShowLayout { show, segment, layout: Some(pair) })
            } else {
                Ok(RoomCommand::SetMount { id: Some(id.clone()), layout: Some(pair) })
            }
        }
        _ => bail!("Unknown shortcut"),
    }
}
