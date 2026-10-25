//! Stem separation progress window

use crate::{SoundApp, theme::{Tokens, bold}};

/// Shows stem separation progress with a loading bar.
pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    let Some((status, progress)) = &app.stem_progress else { return };
    let mut cancel = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    
    egui::Modal::new(egui::Id::new("stem_separation_progress")).show(ctx, |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        ui.set_width(320.0);
        ui.label(egui::RichText::new("Stem Separation").font(bold(13.0)));
        ui.label(egui::RichText::new(status).color(Tokens::current().text_dim));
        ui.add(egui::ProgressBar::new(*progress));
        ui.horizontal(|ui| {
            cancel |= ui.button("Cancel").clicked();
            ui.label(egui::RichText::new(format!("{:.0}%", progress * 100.0)).color(Tokens::current().text_dim));
        });
    });
    
    if cancel {
        app.stem_progress = None;
        app.ui.status = "Stem separation cancelled".into();
    }
}
