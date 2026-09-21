//! Colours, in one place.
//!
//! Nothing anywhere else names a colour. That rule is cheap to keep from the
//! first line and expensive to impose later, and it is what makes a second
//! theme — or a theme somebody else writes — an addition rather than a hunt
//! through every widget.
//!
//! Themes are values today, not files. A file needs a serialisation format, and
//! a dependency should wait until it does something; reading one of these from
//! disk later changes this module and nothing else, which is the point.

use eframe::egui::{Color32, Context, Visuals};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub name: &'static str,
    pub dark: bool,
    /// Behind everything.
    pub background: Color32,
    /// Panels and strips that sit on the background.
    pub panel: Color32,
    /// Controls at rest.
    pub control: Color32,
    /// Controls under the pointer.
    pub hover: Color32,
    pub text: Color32,
    /// Text that is present but not the point.
    pub dim: Color32,
    /// What the eye should go to: a fader in hand, a selection.
    pub accent: Color32,
    /// A track that has been silenced.
    pub muted: Color32,
    /// A track that has taken the output for itself.
    pub soloed: Color32,
    /// Something the person should know went wrong.
    pub warning: Color32,
}

impl Theme {
    pub const fn midnight() -> Self {
        Self {
            name: "Midnight",
            dark: true,
            background: Color32::from_rgb(0x14, 0x16, 0x1a),
            panel: Color32::from_rgb(0x1d, 0x20, 0x26),
            control: Color32::from_rgb(0x2a, 0x2e, 0x37),
            hover: Color32::from_rgb(0x36, 0x3b, 0x47),
            text: Color32::from_rgb(0xe4, 0xe7, 0xec),
            dim: Color32::from_rgb(0x8a, 0x91, 0x9e),
            accent: Color32::from_rgb(0x4f, 0xa3, 0xff),
            muted: Color32::from_rgb(0x7a, 0x82, 0x90),
            soloed: Color32::from_rgb(0xff, 0xc1, 0x4d),
            warning: Color32::from_rgb(0xff, 0x6b, 0x6b),
        }
    }

    pub const fn paper() -> Self {
        Self {
            name: "Paper",
            dark: false,
            background: Color32::from_rgb(0xf4, 0xf4, 0xf2),
            panel: Color32::from_rgb(0xe8, 0xe8, 0xe5),
            control: Color32::from_rgb(0xd8, 0xd8, 0xd4),
            hover: Color32::from_rgb(0xc6, 0xc6, 0xc1),
            text: Color32::from_rgb(0x1b, 0x1d, 0x22),
            dim: Color32::from_rgb(0x6a, 0x6e, 0x76),
            accent: Color32::from_rgb(0x1d, 0x6f, 0xd0),
            muted: Color32::from_rgb(0x8d, 0x91, 0x99),
            soloed: Color32::from_rgb(0xb9, 0x77, 0x00),
            warning: Color32::from_rgb(0xc0, 0x33, 0x33),
        }
    }

    /// Every theme the application ships with.
    pub fn all() -> [Theme; 2] {
        [Self::midnight(), Self::paper()]
    }

    /// Pushes the theme into egui's own styling, so stock widgets follow it
    /// without each one being told about it.
    pub fn apply(&self, ctx: &Context) {
        let mut visuals = if self.dark { Visuals::dark() } else { Visuals::light() };
        visuals.panel_fill = self.panel;
        visuals.window_fill = self.panel;
        visuals.extreme_bg_color = self.background;
        visuals.faint_bg_color = self.panel;
        visuals.override_text_color = Some(self.text);
        visuals.hyperlink_color = self.accent;
        visuals.selection.bg_fill = self.accent;

        visuals.widgets.noninteractive.bg_fill = self.panel;
        visuals.widgets.inactive.bg_fill = self.control;
        visuals.widgets.inactive.weak_bg_fill = self.control;
        visuals.widgets.hovered.bg_fill = self.hover;
        visuals.widgets.hovered.weak_bg_fill = self.hover;
        visuals.widgets.active.bg_fill = self.accent;
        visuals.widgets.active.weak_bg_fill = self.accent;

        ctx.set_visuals(visuals);
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::midnight()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_is_named_and_readable() {
        // A theme whose text disappears into its background is not a theme,
        // and it is the one mistake a contributor will make first.
        for theme in Theme::all() {
            assert!(!theme.name.is_empty());
            let contrast = |a: Color32, b: Color32| {
                let lum =
                    |c: Color32| 0.2126 * f32::from(c.r()) + 0.7152 * f32::from(c.g()) + 0.0722 * f32::from(c.b());
                (lum(a) - lum(b)).abs()
            };
            assert!(contrast(theme.text, theme.background) > 80.0, "{} has unreadable text", theme.name);
            assert!(contrast(theme.text, theme.panel) > 80.0, "{} has unreadable text on panels", theme.name);
            assert!(contrast(theme.accent, theme.panel) > 20.0, "{}'s accent does not stand out", theme.name);
        }
    }

    #[test]
    fn the_themes_are_actually_different() {
        let [first, second] = Theme::all();
        assert_ne!(first.background, second.background);
        assert_ne!(first.dark, second.dark);
    }
}
