use super::drag::fade_drag_preview;
use super::input::{gain_icon, near_fade_midpoint};
use super::layout::{sample_at, x_of};
use super::*;

/// The outline of the Strip Silence rectangles.
pub fn strip_silence_stroke() -> Stroke {
    Stroke::new(1.5, Tokens::current().accent)
}

/// The Strip Silence rectangles on one lane, while the Strip Silence window is open: one around each
/// range Strip keeps.
pub(super) fn draw_strip_silence(painter: &egui::Painter, s: &Session, tl: Rect, lane: Rect, ranges: &[Range]) {
    let stroke = strip_silence_stroke();
    for r in ranges {
        let x0 = x_of(s, tl, r.start).max(lane.min.x);
        let x1 = x_of(s, tl, r.end).min(lane.max.x);
        if x1 > x0 {
            let rect = Rect::from_min_max(pos2(x0, lane.min.y + 1.0), pos2(x1, lane.max.y - 1.0));
            painter.rect(rect, 0.0, stroke.color.gamma_multiply(0.12), stroke, StrokeKind::Inside);
        }
    }
}

pub(super) fn grid_lines(painter: &egui::Painter, s: &Session, tl: Rect, lane: Rect) {
    let t = Tokens::current();
    let view = Range::new(sample_at(s, tl, tl.min.x).max(0), sample_at(s, tl, tl.max.x));
    let sr = s.sample_rate;
    // Bar lines always; grid lines when enabled and dense enough.
    let bar = GridValue::Note { value: NoteValue::Bar, dotted: false, triplet: false };
    let bar_px = bar.step_samples(view.start, sr, &s.tempo, s.frame_rate) as f64 / s.edit.zoom.samples_per_px;
    if bar_px > 6.0 {
        let col = grid_veil(t.bar_line, t.playlist_bg);
        for at in bar.lines(view.start, view.end, sr, &s.tempo, s.frame_rate, 600) {
            let x = x_of(s, tl, at);
            painter.line_segment([pos2(x, lane.min.y), pos2(x, lane.max.y)], Stroke::new(1.0, col));
        }
    }
    if s.edit.grid_lines {
        let step_px = s.edit.grid.step_samples(view.start, sr, &s.tempo, s.frame_rate) as f64 / s.edit.zoom.samples_per_px;
        if step_px > 7.0 {
            let col = grid_veil(t.grid_line, t.playlist_bg);
            for at in s.edit.grid.lines(view.start, view.end, sr, &s.tempo, s.frame_rate, 2000) {
                let x = x_of(s, tl, at);
                painter.line_segment([pos2(x, lane.min.y), pos2(x, lane.max.y)], Stroke::new(1.0, col));
            }
        }
    }
}

pub(super) fn clip_name_height(s: &Session, lane: Rect) -> f32 {
    if lane.height() >= 40.0 && s.edit.flag("view.clip.name") { 13.0 } else { 0.0 }
}

pub(super) fn fade_curve_points(fade: soundcraft_model::Fade, fade_in: bool, a: f32, b: f32, body: Rect, pixels_per_point: f32) -> Vec<Pos2> {
    let scale = if pixels_per_point.is_finite() { pixels_per_point.clamp(0.5, 4.0) } else { 1.0 };
    let extent = (b - a).abs().max(body.height().abs()) * scale;
    let extent = if extent.is_finite() { extent.clamp(64.0, 4096.0) } else { 64.0 };
    // A narrow fade needs fewer: the curve between two samples stays inside a box `width / segments`
    // wide, so it is never further than that from its chord. Zoomed out on an edited drum take,
    // thousands of fades are a pixel or two wide.
    let narrow = ((b - a).abs() * scale * 4.0).ceil() + 1.0;
    // Physical-pixel sampling avoids visible corners, even for steep bends.
    // Keep an exact midpoint and cap allocations for extreme zoom/clip sizes.
    let segments = (extent.min(narrow).max(2.0).ceil() as usize).div_ceil(2) * 2;
    (0..=segments)
        .map(|i| {
            let k = i as f32 / segments as f32;
            pos2(a + (b - a) * k, body.max.y - body.height() * fade.gain(if fade_in { k } else { 1.0 - k }))
        })
        .collect()
}

pub(super) fn fade_control_position(fade: soundcraft_model::Fade, a: f32, b: f32, body: Rect) -> Pos2 {
    let padding = body.height().clamp(0.0, 8.0) * 0.5;
    let y = body.max.y - body.height() * fade.gain(0.5);
    // Keep the whole handle inside the body when a bend approaches an edge.
    pos2((a + b) * 0.5, y.max(body.min.y + padding).min(body.max.y - padding))
}

fn clip_preview_fades(clip: &Clip, gesture: Option<&Gesture>) -> (soundcraft_model::Fade, soundcraft_model::Fade) {
    let (mut fade_in, mut fade_out) = (clip.fade_in, clip.fade_out);
    match gesture {
        Some(Gesture::Fade { clip: id, fade_in: side, to, .. }) if *id == clip.id => {
            if let Some(fade) = fade_drag_preview(clip, *side, *to) {
                if *side {
                    fade_in = fade;
                } else {
                    fade_out = fade;
                }
            }
        }
        Some(Gesture::FadeCurve { clip: id, fade_in: side, curvature, .. }) if *id == clip.id => {
            if *side {
                fade_in.curvature = *curvature;
            } else {
                fade_out.curvature = *curvature;
            }
        }
        _ => {}
    }
    (fade_in, fade_out)
}

/// The take `track` is recording, drawn in red as it grows, as in Pro Tools.
pub(super) fn draw_live_take(app: &SoundApp, painter: &egui::Painter, s: &Session, track: &Track, tl: Rect, lane: Rect) {
    let Some(rec) = &app.recorder else { return };
    let Some(take) = rec.live_take(track.id) else { return };
    let block_len = soundcraft_playback::record::LIVE_BLOCK as f64 * s.sample_rate.as_f64() / f64::from(rec.sample_rate.max(1));
    let len = soundcraft_time::to_samples(take.first().map_or(0, Vec::len) as f64 * block_len);
    let sel = s.edit.selection;
    let (start, pass) = if s.edit.loop_record && !sel.is_empty() { (sel.start, Some(sel.len())) } else { (app.record_start, None) };
    let Some((origin, span)) = soundcraft_engine::io::live_span(start, pass, app.punch, len) else { return };
    let r = Rect::from_min_max(pos2(x_of(s, tl, span.start), lane.min.y + 1.0), pos2(x_of(s, tl, span.end), lane.max.y - 1.0));
    let t = Tokens::current();
    let (body, _, wave) = clip_colors([t.rec.r(), t.rec.g(), t.rec.b()], false);
    painter.rect(r, CornerRadius::same(2), body, Stroke::new(1.0, Color32::from_rgb(10, 10, 10)), StrokeKind::Inside);
    if track.view != "blocks" {
        crate::waveform::draw_live(
            crate::waveform::Canvas { painter: &painter.with_clip_rect(r.intersect(lane)), s, tl },
            origin as f64,
            block_len,
            &take,
            r,
            wave,
        );
    }
}

/// The translucent white or black that turns `bg` into `line`, so grid lines also show over clips.
fn grid_veil(line: Color32, bg: Color32) -> Color32 {
    let level = |c: Color32| (f32::from(c.r()) + f32::from(c.g()) + f32::from(c.b())) / 3.0;
    let (l, b) = (level(line), level(bg));
    if l > b {
        Color32::from_white_alpha(((l - b) / (255.0 - b) * 255.0).round() as u8)
    } else {
        Color32::from_black_alpha(((b - l) / b.max(1.0) * 255.0).round() as u8)
    }
}

/// Draw one clip of `clips` (its playlist), with `partners`, its [`ops::crossfade_partners`]. The two clips of a
/// crossfade are drawn butted at the cut, each only on its side of it; the zone laid over the cut is drawn with the
/// later clip. `sorted`: whether `track.clips()` is sorted by start (worked out once per track and frame).
pub(super) fn draw_clip<'a>(
    app: &SoundApp,
    outer: &egui::Painter,
    s: &Session,
    track: &Track,
    sorted: bool,
    clips: &[Clip],
    partners: (Option<&'a Clip>, Option<&'a Clip>),
    clip: &Clip,
    tl: Rect,
    lane: Rect,
    dim: bool,
) {
    let (before, after) = partners;
    // The clip's own extent (its fades run along it), and the part of it that shows.
    let (cx0, cx1) = (x_of(s, tl, clip.start), x_of(s, tl, clip.end()).max(x_of(s, tl, clip.start) + 1.0));
    let (shown_from, shown_to) = shown_span(clip, before, after);
    let x0 = x_of(s, tl, shown_from);
    let x1 = x_of(s, tl, shown_to).max(x0 + 1.0);
    // The two clips of a crossfade show only on their side of the cut; any other clip keeps the lane as it was.
    let narrowed;
    let painter = if before.is_some() || after.is_some() {
        narrowed = outer.with_clip_rect(Rect::from_x_y_ranges(x0..=x1, lane.y_range()));
        &narrowed
    } else {
        outer
    };
    let r = Rect::from_min_max(pos2(x0, lane.min.y + 1.0), pos2(x1, lane.max.y - 1.0));
    let selected = s.edit.selected_clips.contains(&clip.id)
        || (s.edit.selected_tracks.contains(&track.id)
            && !s.edit.selection.is_empty()
            && s.edit.selection.start <= clip.start
            && s.edit.selection.end >= clip.end());
    let color = clip.color.unwrap_or(track.color);
    let (mut body, bar, mut wave) = clip_colors(color, selected);
    if clip.muted || dim {
        body = body.gamma_multiply(0.45);
        wave = Color32::from_rgb(70, 70, 70);
    }
    let body = if s.edit.flag("view.clip.transparency") { body.gamma_multiply(0.55) } else { body };
    // Square where the clip is butted to its crossfade partner at the cut.
    let (left, right) = (if before.is_some() { 0 } else { 2 }, if after.is_some() { 0 } else { 2 });
    // The outline stops at the cut: its edge there lies just past the painter's clip, or the two outlines would draw
    // a dark line across the zone where the clips meet, and nothing marks the cut inside a crossfade.
    let outline = Rect::from_min_max(
        pos2(if before.is_some() { r.min.x - 2.0 } else { r.min.x }, r.min.y),
        pos2(if after.is_some() { r.max.x + 2.0 } else { r.max.x }, r.max.y),
    );
    painter.rect(
        outline,
        CornerRadius { nw: left, sw: left, ne: right, se: right },
        body,
        Stroke::new(1.0, Color32::from_rgb(10, 10, 10)),
        StrokeKind::Inside,
    );
    let name_h = clip_name_height(s, lane);
    // The name bar of a crossfade's clips stops and starts at the zone, not at the cut: the earlier clip's ends
    // where the zone begins, the later one's begins where the zone ends. Between them the zone is the crossfade's.
    let bar_from = before.map_or(r.min.x, |a| x_of(s, tl, a.end()).max(r.min.x));
    let bar_to = after.map_or(r.max.x, |b| x_of(s, tl, b.start).min(r.max.x));
    if name_h > 0.0 && bar_to > bar_from {
        let nb = Rect::from_min_max(pos2(bar_from, r.min.y), pos2(bar_to, r.min.y + name_h));
        painter.rect_filled(nb, CornerRadius { nw: left, ne: right, sw: 0, se: 0 }, if clip.muted { Color32::from_rgb(70, 70, 70) } else { bar });
        let name_clip = nb.intersect(lane);
        // Narrower than this, no glyph would show: the name starts 4 points in.
        if name_clip.width() > 6.0 {
            painter.with_clip_rect(name_clip).text(
                pos2(nb.min.x.max(lane.min.x) + 4.0, nb.center().y),
                Align2::LEFT_CENTER,
                &clip.name,
                bold(10.5),
                Color32::from_rgb(12, 12, 12),
            );
        }
    }
    let body_r = Rect::from_min_max(pos2(r.min.x, r.min.y + name_h), r.max);
    let vis = body_r.intersect(lane);
    // The waveform and fade curve share the same temporary envelope every frame.
    let (fade_in, fade_out) = clip_preview_fades(clip, app.gesture.as_ref().filter(|_| !dim));
    match &clip.content {
        _ if track.view == "blocks" => {}
        ClipContent::Audio { source, .. } => {
            if let Some(audio) = s.pool.get(*source) {
                if vis.width() > 0.0 && vis.height() > 2.0 {
                    // Drawn with the fades being dragged, so the waveform follows them before release.
                    let preview;
                    let shown = if (fade_in, fade_out) == (clip.fade_in, clip.fade_out) {
                        clip
                    } else {
                        preview = Clip { fade_in, fade_out, ..clip.clone() };
                        &preview
                    };
                    crate::waveform::draw(
                        crate::waveform::Canvas { painter: &painter.with_clip_rect(vis), s, tl },
                        shown,
                        audio,
                        track.channels(),
                        body_r,
                        wave,
                    );
                    let n = track.channels().min(audio.peaks.len()).max(1);
                    for k in 1..n {
                        let y = body_r.min.y + body_r.height() / n as f32 * k as f32;
                        painter.line_segment([pos2(vis.min.x, y), pos2(vis.max.x, y)], Stroke::new(1.0, body.gamma_multiply(0.7)));
                    }
                }
            } else {
                painter.text(body_r.center(), Align2::CENTER_CENTER, "media offline", regular(10.0), Color32::from_rgb(220, 120, 120));
            }
        }
        ClipContent::Video { .. } => {} // thumbnails: video_track::draw_lane
        ClipContent::Midi { sequence } => {
            let (lo, hi) = sequence.notes.iter().fold((127u8, 0u8), |(lo, hi), n| (lo.min(n.pitch), hi.max(n.pitch)));
            let (lo, hi) = if lo > hi { (48, 72) } else { (lo.saturating_sub(2), hi.saturating_add(2)) };
            let span = f32::from(hi.saturating_sub(lo).max(1));
            let base = s.tempo.samples_to_ticks(clip.start, s.sample_rate);
            let p = painter.with_clip_rect(vis);
            for n in &sequence.notes {
                let a = s.tempo.tick_to_samples(base + n.start, s.sample_rate);
                let b = s.tempo.tick_to_samples(base + n.start + n.length, s.sample_rate).min(clip.end());
                let nx0 = x_of(s, tl, a);
                let nx1 = x_of(s, tl, b).max(nx0 + 2.0);
                if nx1 < vis.min.x || nx0 > vis.max.x {
                    continue;
                }
                let y = body_r.max.y - 3.0 - (f32::from(n.pitch.saturating_sub(lo)) / span) * (body_r.height() - 6.0);
                let nh = ((body_r.height() - 6.0) / span).clamp(1.5, 6.0);
                let vcol = if crate::theme::is_light() {
                    Color32::from_rgb(20, 20 + n.velocity, 40)
                } else {
                    wave.gamma_multiply(0.5 + f32::from(n.velocity) / 254.0)
                };
                p.rect_filled(
                    Rect::from_min_max(pos2(nx0, y - nh * 0.5), pos2(nx1, y + nh * 0.5)),
                    1.0,
                    if selected { Color32::from_rgb(16, 16, 16) } else { vcol },
                );
            }
        }
    }
    // A clip laid over another (no crossfade) shows the other's tail through it.
    let layered = crossfade_partner(clips, clip).filter(|p| before.is_none_or(|a| a.id != p.id));
    let overlapped = s.edit.flag("waveform.overlapped_crossfades") && track.view != "blocks";
    if overlapped
        && let Some(prev) = before.or(layered)
        && let ClipContent::Audio { source, .. } = &prev.content
        && let Some(audio) = s.pool.get(*source)
    {
        let xf = Rect::from_min_max(pos2(x0, body_r.min.y), pos2(x_of(s, tl, prev.end()).min(x1), body_r.max.y)).intersect(vis);
        let tail = if prev.muted || dim {
            Color32::from_rgb(70, 70, 70)
        } else {
            clip_colors(prev.color.unwrap_or(track.color), false).2.gamma_multiply(0.6)
        };
        crate::waveform::draw(crate::waveform::Canvas { painter: &painter.with_clip_rect(xf), s, tl }, prev, audio, track.channels(), body_r, tail);
    }
    // In a crossfade both clips play across the whole zone: the later one's audio also shows left of the cut.
    if overlapped
        && before.is_some()
        && let ClipContent::Audio { source, .. } = &clip.content
        && let Some(audio) = s.pool.get(*source)
    {
        let own = Rect::from_min_max(pos2(cx0, body_r.min.y), pos2(x0, body_r.max.y));
        let tail = if clip.muted || dim { Color32::from_rgb(70, 70, 70) } else { clip_colors(color, false).2.gamma_multiply(0.6) };
        crate::waveform::draw(
            crate::waveform::Canvas { painter: &outer.with_clip_rect(own.intersect(lane)), s, tl },
            clip,
            audio,
            track.channels(),
            own,
            tail,
        );
    }
    // Clip gain line.
    let show_gain = s.edit.flag("view.clip.gain_line") && (!clip.gain_env.is_empty() || clip.gain_db.abs() > 0.01 || track.view == "clip_gain");
    if show_gain && body_r.height() > 12.0 {
        let gy = |db: f32| body_r.max.y - 3.0 - ((db.clamp(-36.0, 36.0) + 36.0) / 72.0) * (body_r.height() - 6.0);
        let col = Color32::from_rgba_unmultiplied(255, 255, 255, 170);
        if clip.gain_env.is_empty() {
            painter.line_segment([pos2(x0, gy(clip.gain_db)), pos2(x1, gy(clip.gain_db))], Stroke::new(1.0, col));
        } else {
            let mut pts = vec![pos2(x0, gy(clip.gain_db + clip.gain_env.first().map_or(0.0, |p| p.1)))];
            for (o, db) in &clip.gain_env {
                let p = pos2(x_of(s, tl, clip.start + o), gy(clip.gain_db + db));
                pts.push(p);
                painter.circle_filled(p, 2.5, col);
            }
            pts.push(pos2(x1, gy(clip.gain_db + clip.gain_env.last().map_or(0.0, |p| p.1))));
            painter.add(Shape::line(pts, Stroke::new(1.0, col)));
        }
    }
    if !dim && let Some(icon) = gain_icon(s, tl, lane, clip, shown_from, shown_to) {
        let col = Color32::from_rgba_unmultiplied(255, 255, 255, 200);
        painter.line_segment([pos2(icon.center().x, icon.min.y), pos2(icon.center().x, icon.max.y)], Stroke::new(1.0, col));
        painter.rect_filled(Rect::from_center_size(pos2(icon.center().x, icon.center().y + 2.0), vec2(icon.width(), 3.0)), 1.0, col);
        painter.text(pos2(icon.max.x + 3.0, icon.center().y), Align2::LEFT_CENTER, format!("{:+.1} dB", clip.gain_db), regular(9.5), col);
    }
    let fade_col = Color32::from_rgba_unmultiplied(255, 255, 255, 90);
    for (side, fade) in [(true, fade_in), (false, fade_out)] {
        if fade.len <= 0 {
            continue;
        }
        let (a, b) =
            if side { (cx0, x_of(s, tl, clip.start.saturating_add(fade.len))) } else { (x_of(s, tl, clip.end().saturating_sub(fade.len)), cx1) };
        let pts = fade_curve_points(fade, side, a, b, body_r, painter.ctx().pixels_per_point());
        // The fade that runs along a crossfade is drawn with the zone; only what lies past it is the clip's.
        let mut shown = vis;
        match (side, before, after) {
            (true, Some(prev), _) => shown.min.x = shown.min.x.max(x_of(s, tl, prev.end())),
            (false, _, Some(next)) => shown.max.x = shown.max.x.min(x_of(s, tl, next.start)),
            _ => {}
        }
        painter.with_clip_rect(shown).add(Shape::line(pts, Stroke::new(1.2, fade_col)));
        let midpoint = fade_control_position(fade, a, b, body_r);
        let active = matches!(app.gesture, Some(Gesture::FadeCurve { clip: id, fade_in, .. }) if id == clip.id && fade_in == side);
        let hovered = painter.ctx().input(|i| i.pointer.hover_pos()).is_some_and(|p| near_fade_midpoint(p, midpoint));
        // The fades that run along a crossfade are the zone's: resized with its edges, not with a handle of their own.
        let hidden = if side { before.is_some() } else { after.is_some() };
        if !dim && !hidden && s.edit.tool == Tool::Smart && AutoParam::parse(&track.view).is_none() && b - a >= 32.0 && vis.contains(midpoint) {
            let col = Color32::from_rgba_unmultiplied(255, 255, 255, if active || hovered { 230 } else { 200 });
            painter.with_clip_rect(vis).circle(midpoint, 3.5, body, Stroke::new(1.0, col));
        }
    }
    // The fade-out of the clip this one is laid over, drawn over the overlap.
    if let Some(c) = layered.filter(|c| c.fade_out.len > 0) {
        let (a, b) = (x_of(s, tl, c.end().saturating_sub(c.fade_out.len)), x_of(s, tl, c.end()));
        let pts = fade_curve_points(c.fade_out, false, a, b, body_r, painter.ctx().pixels_per_point());
        painter.with_clip_rect(vis).add(Shape::line(pts, Stroke::new(1.2, fade_col)));
    }
    // Optional clip overlays (View › Clip).
    let ov = Color32::from_rgb(16, 16, 16);
    if s.edit.flag("view.clip.rating") && clip.rating > 0 && name_h > 0.0 {
        painter.text(pos2(r.max.x - 18.0, r.min.y + 7.0), Align2::RIGHT_CENTER, "★".repeat(usize::from(clip.rating.min(5))), regular(9.0), ov);
    }
    if s.edit.flag("view.clip.sync_point") && clip.sync_point > 0 {
        let x = x_of(s, tl, clip.start + clip.sync_point);
        painter.add(Shape::convex_polygon(
            vec![pos2(x - 4.0, r.max.y - 1.0), pos2(x + 4.0, r.max.y - 1.0), pos2(x, r.max.y - 7.0)],
            Color32::WHITE,
            Stroke::NONE,
        ));
        painter.line_segment([pos2(x, r.min.y + name_h), pos2(x, r.max.y)], Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 90)));
    }
    let time_text = if s.edit.flag("view.clip.time.current") {
        Some(s.format_position(clip.start, s.edit.main_counter))
    } else if s.edit.flag("view.clip.time.original") || s.edit.flag("view.clip.time.user") {
        clip.source().and_then(|src| s.source(src)).map(|src| {
            format_position(
                i64::try_from(src.time_reference).unwrap_or(0).saturating_add(clip.source_offset()).saturating_sub(s.timecode_start),
                TimeFormat::Timecode,
                s.sample_rate,
                &s.tempo,
                s.frame_rate,
                0,
                0,
            )
        })
    } else {
        None
    };
    if let Some(tt) = time_text
        && r.height() > 30.0
    {
        painter.with_clip_rect(r.intersect(lane)).text(pos2(r.min.x.max(lane.min.x) + 4.0, r.max.y - 7.0), Align2::LEFT_CENTER, tt, regular(9.0), ov);
    }
    if s.edit.flag("view.clip.effects_status") && s.edit.values.keys().any(|k| k.starts_with(&format!("clip_fx.{}.", clip.id.0))) {
        painter.text(pos2(r.max.x - 6.0, r.max.y - 7.0), Align2::RIGHT_CENTER, "FX", bold(9.0), Color32::from_rgb(250, 230, 120));
    }
    if s.edit.flag("view.clip.overlap_shadows") {
        // Shade where a later clip covers this one (crossfades and layered edits). Only clips that
        // start inside this one can: on a sorted track, find them instead of checking every clip.
        let clips = track.clips();
        let later = if sorted {
            let from = clips.partition_point(|o| o.start <= clip.start);
            clips.get(from..clips.partition_point(|o| o.start < clip.end()).max(from)).unwrap_or_default()
        } else {
            clips
        };
        for o in later {
            if o.id != clip.id && o.start > clip.start && o.start < clip.end() && after.is_none_or(|b| b.id != o.id) {
                let x0 = x_of(s, tl, o.start);
                let x1 = x_of(s, tl, clip.end().min(o.end()));
                painter.rect_filled(Rect::from_min_max(pos2(x0, r.min.y), pos2(x1, r.max.y)), 0.0, Color32::from_black_alpha(70));
            }
        }
    }
    if clip.edit_locked || clip.time_locked {
        painter.text(pos2(r.max.x - 10.0, r.min.y + 7.0), Align2::CENTER_CENTER, "🔒", regular(9.0), Color32::WHITE);
    }
    if let Some(earlier) = before {
        draw_zone(app, outer, s, tl, lane, earlier, clip, body_r.y_range(), dim);
    }
}

/// The ink of a crossfade's zone: the theme's insertion colour, light on the dark clip bodies of the
/// dark palette and dark on the pale ones of the light palette.
fn zone_ink() -> Color32 {
    Tokens::current().insertion
}

/// A box over the zone of a crossfade, translucent so the clips and their waveforms show through. A
/// `ghost` is the outline of where a drag will leave it.
pub(super) fn zone_box(painter: &egui::Painter, zone: Rect, ghost: bool) {
    let ink = zone_ink();
    if zone.width() >= 3.0 {
        let (fill, line) = if ghost { (0.10, Stroke::new(2.0, ink)) } else { (0.16, Stroke::new(1.0, ink.gamma_multiply(0.7))) };
        painter.rect(zone, CornerRadius::ZERO, ink.gamma_multiply(fill), line, StrokeKind::Inside);
    }
}

/// The zone of the crossfade between `earlier` and `later` laid over their cut: a box across the whole
/// lane (the name band included), the outgoing fade of the earlier clip and the incoming fade of the
/// later one across all of it (the X). The incumbent marks no cut inside a crossfade, and neither do
/// we: it shows where the earlier clip's waveform gives way to the later one's. `body_y` is the height
/// the clips' fades span. Over another view of the track (`dim`, the automation view) the zone is only
/// the box, so the crossfade stays visible without competing with the curve being edited.
fn draw_zone(
    app: &SoundApp,
    painter: &egui::Painter,
    s: &Session,
    tl: Rect,
    lane: Rect,
    earlier: &Clip,
    later: &Clip,
    body_y: egui::Rangef,
    dim: bool,
) {
    let (zs, ze) = (x_of(s, tl, later.start), x_of(s, tl, earlier.end()));
    let zone = Rect::from_x_y_ranges(zs..=ze, lane.y_range());
    zone_box(painter, zone, false);
    if !dim {
        let ppp = painter.ctx().pixels_per_point();
        let curves = painter.with_clip_rect(zone);
        let body = Rect::from_x_y_ranges(zs..=ze, body_y);
        let (_, fade_out) = clip_preview_fades(earlier, app.gesture.as_ref());
        let (fade_in, _) = clip_preview_fades(later, app.gesture.as_ref());
        for (fade, rising) in [(fade_out, false), (fade_in, true)] {
            if fade.len <= 0 {
                continue;
            }
            let (a, b) = if rising {
                (zs, x_of(s, tl, later.start.saturating_add(fade.len)))
            } else {
                (x_of(s, tl, earlier.end().saturating_sub(fade.len)), ze)
            };
            curves.add(Shape::line(fade_curve_points(fade, rising, a, b, body, ppp), Stroke::new(1.5, zone_ink())));
        }
    }
}

/// The audio crossfade whose zone holds `at`: its earlier and its later clip. Clips that merely overlap
/// (one laid over another, MIDI) are not a crossfade and are grabbed as clips.
pub(super) fn zone_at(clips: &[Clip], at: Samples) -> Option<(&Clip, &Clip)> {
    let top = clips.iter().rev().find(|c| c.range().contains(at))?;
    let earlier = ops::crossfade_earlier(clips, top).filter(|a| a.end() > at)?;
    Some((earlier, top))
}

/// The part of a clip that shows: the two clips of a crossfade are butted at the cut, so the earlier one
/// shows up to it and the later one from it. What lies under the zone belongs to the zone.
pub(super) fn shown_span(clip: &Clip, before: Option<&Clip>, after: Option<&Clip>) -> (Samples, Samples) {
    let span = ops::visible_span(clip, before, after);
    (span.start, span.end)
}

/// A crossfade as it is on screen, in points: its two clips and where its zone and its cut are.
struct ScreenZone<'a> {
    earlier: &'a Clip,
    later: &'a Clip,
    left: f32,
    right: f32,
    cut: f32,
}

impl ScreenZone<'_> {
    /// Narrower than the bands of its two edges and the band around its cut put together: the edges would
    /// leave the Grabber less than that whole band to slide it by.
    fn narrow(&self) -> bool {
        self.right - self.left < 2.0 * EDGE_GRAB + 2.0 * ZONE_CENTRE
    }

    /// What the Grabber takes, by pixels rather than samples (a 10 ms crossfade is a few points wide): the
    /// zone, and for a narrow one also the points within [`ZONE_CENTRE`] of its cut, a band the edges leave to it.
    fn grabs(&self, x: f32) -> bool {
        let (lo, hi) =
            if self.narrow() { (self.left.min(self.cut - ZONE_CENTRE), self.right.max(self.cut + ZONE_CENTRE)) } else { (self.left, self.right) };
        lo <= x && x <= hi
    }
}

/// The crossfades of `clips` whose zone is under `x` or within grabbing distance of it.
fn zones_near<'a>(s: &Session, tl: Rect, clips: &'a [Clip], x: f32) -> Vec<ScreenZone<'a>> {
    clips
        .iter()
        .filter(|b| x_of(s, tl, b.start) - EDGE_GRAB <= x && x <= x_of(s, tl, b.end()) + EDGE_GRAB)
        .filter_map(|later| {
            let earlier = ops::crossfade_earlier(clips, later)?;
            let (left, right) = (x_of(s, tl, later.start), x_of(s, tl, earlier.end()));
            Some(ScreenZone { earlier, later, left, right, cut: x_of(s, tl, crossfade_cut(earlier, later)) })
        })
        .filter(|z| z.left - EDGE_GRAB <= x && x <= z.right + EDGE_GRAB)
        .collect()
}

/// The audio crossfade the Grabber takes at `x`, by pixels (see [`ScreenZone::grabs`]): its earlier and its later
/// clip. Not one that can't slide (a clip edit-locked or stretched: the press is then the clips', as for any clip),
/// nor one with another clip laid over it at that point, which is grabbed as the clip it is.
pub(super) fn zone_under<'a>(s: &Session, tl: Rect, clips: &'a [Clip], x: f32) -> Option<(&'a Clip, &'a Clip)> {
    let top = clips.iter().rev().find(|c| c.range().contains(sample_at(s, tl, x).max(0)))?;
    zones_near(s, tl, clips, x)
        .into_iter()
        .filter(|z| z.grabs(x))
        .filter(|z| z.earlier.id == top.id || z.later.id == top.id)
        .filter(|z| ops::crossfade_slide(s, z.later.id, 0).is_some())
        .min_by(|p, q| (x - p.cut).abs().total_cmp(&(x - q.cut).abs()))
        .map(|z| (z.earlier, z.later))
}

/// An edge of a crossfade's zone that the Trim tool pulls.
pub(super) struct ZoneEdge {
    /// The later clip of the crossfade, the one its commands name.
    pub(super) later: ClipId,
    /// The left edge (the later clip's start), or the right one (the earlier clip's end).
    pub(super) start: bool,
    /// Where the edge is now.
    pub(super) at: Samples,
    /// The pointer is within grabbing distance of the edge, rather than somewhere in the zone.
    pub(super) near: bool,
    /// The pointer is in the band around the cut that a narrow zone keeps for sliding it.
    pub(super) centre: bool,
}

/// The zone edge nearest to the pointer at `x`, when it is in a zone or within grabbing distance of one.
pub(super) fn zone_edge_at(s: &Session, tl: Rect, clips: &[Clip], x: f32) -> Option<ZoneEdge> {
    zones_near(s, tl, clips, x)
        .into_iter()
        .flat_map(|z| {
            let centre = z.narrow() && (x - z.cut).abs() < ZONE_CENTRE;
            [(z.later.id, true, z.later.start, z.left, centre), (z.later.id, false, z.earlier.end(), z.right, centre)]
        })
        .map(|(later, start, at, edge_x, centre)| (ZoneEdge { later, start, at, near: false, centre }, (edge_x - x).abs()))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(edge, dist)| ZoneEdge { near: dist < EDGE_GRAB && !edge.centre, ..edge })
}

/// How far from the cut of a narrow zone, in points, the Grabber still takes it.
const ZONE_CENTRE: f32 = 3.0;

/// How close the pointer has to be to a clip edge or a zone edge to trim it, in points.
pub(super) const EDGE_GRAB: f32 = 6.0;

/// The clip before `clip` in its playlist that overlaps its start, whatever the overlap is: a crossfade,
/// or a clip laid over another. The crossfade pairs are told apart with [`ops::crossfade_earlier`].
fn crossfade_partner<'a>(clips: &'a [Clip], clip: &Clip) -> Option<&'a Clip> {
    let i = clips.partition_point(|c| c.start < clip.start);
    clips.get(i).is_some_and(|c| c.id == clip.id).then_some(())?;
    clips.get(i.checked_sub(1)?).filter(|c| c.end() > clip.start)
}
