use gpui2::Hsla;
use refineable::Refineable;

use crate::SyntaxTheme;

#[derive(Clone)]
pub struct SystemColors {
    pub transparent: Hsla,
    pub mac_os_traffic_light_red: Hsla,
    pub mac_os_traffic_light_yellow: Hsla,
    pub mac_os_traffic_light_green: Hsla,
}

#[derive(Debug, Clone, Copy)]
pub struct PlayerColor {
    pub cursor: Hsla,
    pub background: Hsla,
    pub selection: Hsla,
}

#[derive(Clone)]
pub struct PlayerColors(pub Vec<PlayerColor>);

#[derive(Refineable, Clone, Debug)]
#[refineable(debug)]
pub struct StatusColors {
    pub conflict: Hsla,
    pub created: Hsla,
    pub deleted: Hsla,
    pub error: Hsla,
    pub hidden: Hsla,
    pub ignored: Hsla,
    pub info: Hsla,
    pub modified: Hsla,
    pub renamed: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
}

#[derive(Refineable, Clone, Debug)]
#[refineable(debug)]
pub struct GitStatusColors {
    pub conflict: Hsla,
    pub created: Hsla,
    pub deleted: Hsla,
    pub ignored: Hsla,
    pub modified: Hsla,
    pub renamed: Hsla,
}

#[derive(Refineable, Clone, Debug, Default)]
#[refineable(debug)]
pub struct ThemeColors {
    pub border: Hsla,
    pub border_variant: Hsla,
    pub border_focused: Hsla,
    pub border_transparent: Hsla,
    pub elevated_surface: Hsla,
    pub surface: Hsla,
    pub background: Hsla,
    pub element: Hsla,
    pub element_hover: Hsla,
    pub element_active: Hsla,
    pub element_selected: Hsla,
    pub element_disabled: Hsla,
    pub element_placeholder: Hsla,
    pub ghost_element: Hsla,
    pub ghost_element_hover: Hsla,
    pub ghost_element_active: Hsla,
    pub ghost_element_selected: Hsla,
    pub ghost_element_disabled: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_placeholder: Hsla,
    pub text_disabled: Hsla,
    pub text_accent: Hsla,
    pub icon: Hsla,
    pub icon_muted: Hsla,
    pub icon_disabled: Hsla,
    pub icon_placeholder: Hsla,
    pub icon_accent: Hsla,
    pub status_bar: Hsla,
    pub title_bar: Hsla,
    pub toolbar: Hsla,
    pub tab_bar: Hsla,
    pub editor: Hsla,
    pub editor_subheader: Hsla,
    pub editor_active_line: Hsla,
}

#[derive(Refineable, Clone)]
pub struct ThemeStyles {
    pub system: SystemColors,
    pub colors: ThemeColors,
    pub status: StatusColors,
    pub git: GitStatusColors,
    pub player: PlayerColors,
    pub syntax: SyntaxTheme,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_a_single_theme_color() {
        let mut colors = ThemeColors::default_light();

        let magenta: Hsla = gpui2::rgb(0xff00ff);

        assert_ne!(colors.text, magenta);

        let overrides = ThemeColorsRefinement {
            text: Some(magenta),
            ..Default::default()
        };

        colors.refine(&overrides);

        assert_eq!(colors.text, magenta);
    }

    #[test]
    fn override_multiple_theme_colors() {
        let mut colors = ThemeColors::default_light();

        let magenta: Hsla = gpui2::rgb(0xff00ff);
        let green: Hsla = gpui2::rgb(0x00ff00);

        assert_ne!(colors.text, magenta);
        assert_ne!(colors.background, green);

        let overrides = ThemeColorsRefinement {
            text: Some(magenta),
            background: Some(green),
            ..Default::default()
        };

        colors.refine(&overrides);

        assert_eq!(colors.text, magenta);
        assert_eq!(colors.background, green);
    }
}