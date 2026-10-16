//! What Craft AI does inside SoundCraft. Craft AI is one assistant across the ArtCraft family
//! — the model, the runtime and the shared folder belong to the family — but its *work* is the
//! app's own: the system prompt tells the model which app it is inside and what that app does,
//! and every ready-made task is framed for that app's world, so the answer that comes back is
//! about tracks and buses and gain staging, not generic advice.
//!
//! Each app of the family carries its own `tasks.rs` — VectorCraft's asks about paths and
//! Pathfinder steps, WordCraft's about documents and styles, and so on — same model, same
//! shared folder, different craft. The labels and hints are catalog keys: the window shows
//! them translated.

/// The app Craft AI is embedded in, as the shared folder's records name it.
pub const APP: &str = "SoundCraft";

/// One ready-made task of the Assistant tab.
pub struct Task {
    /// The label the picker shows (a catalog key).
    pub label: &'static str,
    /// The input field's hint (a catalog key).
    pub hint: &'static str,
}

/// The tasks, in the picker's order.
pub const TASKS: &[Task] = &[
    Task { label: "Ask anything", hint: "Ask in any language" },
    Task { label: "Explain an effect or term", hint: "The effect, term or chain you have in mind" },
    Task { label: "Build an effect chain", hint: "The tracks, buses or sends it should cover" },
    Task { label: "Audio repair steps", hint: "What you hear, and what is wrong with it" },
    Task { label: "Loudness and export targets", hint: "Where it plays, and how loud it must be" },
    Task { label: "Name this track or preset", hint: "The track or preset, for names" },
];

/// Tell the model where it is and how to answer: briefly, practically, in the language the user
/// wrote in, about this app's own world — the words of its interface, the files it opens and
/// writes. The last line is the honesty clause: a local model can be wrong, and it is told to
/// say so rather than invent.
pub fn system_prompt() -> String {
    "You are Craft AI, the assistant built into SoundCraft, a desktop digital audio \
     workstation. SoundCraft's world, in its own words: tracks and their kinds, buses and \
     sends, inserts and routing; EQ, compression, reverb and the other effects, and the \
     effect chains on a track or a bus; gain staging, levels and metering; automation; \
     recording and takes, punch in and out; editing clips, crossfades and fades; the \
     transport, the big counter, memory locations, the mix window and the edit window; \
     loudness targets and exporting a session. Answer in the language the user wrote in, \
     Arabic included. Be brief and practical: name the window or the control, give the steps \
     as a short list, say what to listen for, and when a question is not about SoundCraft or \
     audio, answer it anyway, briefly. Never invent menu entries, window names or control \
     names: if you are not sure SoundCraft has a feature, say so plainly."
        .to_string()
}

/// Frame the user's input for the task they picked.
pub fn user_prompt(task: usize, input: &str) -> String {
    let input = input.trim();
    match TASKS[task.min(TASKS.len() - 1)].label {
        "Explain an effect or term" => format!(
            "Explain this audio effect, term or chain the way a SoundCraft engineer would — what it does, when to reach for it, what to watch out for: {input}"
        ),
        "Build an effect chain" => format!(
            "Design an effect chain in SoundCraft for this purpose — order of the effects, roughly where each goes, and why: {input}"
        ),
        "Audio repair steps" => format!(
            "Give the repair steps in SoundCraft for this audio problem — what to try first, in order, and how to tell it worked: {input}"
        ),
        "Loudness and export targets" => format!(
            "Advise on loudness and export for this session in SoundCraft — the target it plays on, the level to aim for, the format to export: {input}"
        ),
        "Name this track or preset" => format!(
            "Suggest short, fitting names for the track or preset described below — a handful of options, no explanations: {input}"
        ),
        _ => input.to_string(),
    }
}
