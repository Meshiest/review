//! The token tables of a theme file. Each is one struct generic over its value
//! type: `Option<String>` as parsed, [`Color`](crate::Color) once resolved, and
//! gpui `Hsla` (or `Pixels`) at runtime, so the key list is written once.

macro_rules! key {
    ($field:ident) => {
        stringify!($field)
    };
    ($field:ident $key:literal) => {
        $key
    };
}

macro_rules! tokens {
    (
        $(#[$meta:meta])*
        $name:ident<$default:ty>, $section:literal {
            $( $(#[$fmeta:meta])* $field:ident $(= $key:literal)? ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, serde::Deserialize)]
        #[serde(default, deny_unknown_fields)]
        #[serde(bound(deserialize = "C: serde::Deserialize<'de> + Default"))]
        pub struct $name<C = $default> {
            $( $(#[$fmeta])* $(#[serde(rename = $key)])? pub $field: C, )*
        }

        impl<C> $name<C> {
            /// The table's name in a theme file.
            pub const SECTION: &'static str = $section;
            /// Every key, in file order.
            pub const KEYS: &'static [&'static str] = &[$( key!($field $($key)?) ),*];

            /// Converts every value; `f` gets the key too.
            pub fn map<D>(self, mut f: impl FnMut(&'static str, C) -> D) -> $name<D> {
                $name { $( $field: f(key!($field $($key)?), self.$field), )* }
            }

            /// [`Self::map`] for conversions that can fail; stops at the first error.
            ///
            /// # Errors
            ///
            /// The first error `f` returns.
            pub fn try_map<D, E>(
                self,
                mut f: impl FnMut(&'static str, C) -> Result<D, E>,
            ) -> Result<$name<D>, E> {
                Ok($name { $( $field: f(key!($field $($key)?), self.$field)?, )* })
            }

            /// Pairs each value with the same key's value in `other`.
            pub fn zip<D, E>(self, other: $name<D>, mut f: impl FnMut(C, D) -> E) -> $name<E> {
                $name { $( $field: f(self.$field, other.$field), )* }
            }

            /// `(key, value)` for every token, in file order.
            pub fn entries(&self) -> impl Iterator<Item = (&'static str, &C)> {
                Self::KEYS.iter().copied().zip([$( &self.$field ),*])
            }
        }
    };
}

tokens! {
    /// UI color tokens (`[colors]`).
    Colors<gpui::Hsla>, "colors" {
        /// The window behind every panel.
        background,
        /// Panels and the diff.
        surface,
        /// Popovers, menus, the composer, bars and headers.
        surface_raised,
        /// Menus and popovers. Geist draws them on its page background, edged by
        /// a border; the original theme keeps them on `surface_raised`.
        surface_menu,
        /// Text inputs and segmented controls.
        field,
        /// Hairline dividers and panel edges.
        border,
        border_strong,
        /// Outlines that identify a control (inputs, checkboxes): 3:1 on surface.
        control_border,
        text,
        /// Body text that reads below `text` (descriptions, empty states).
        text_secondary,
        /// Metadata and secondary labels.
        text_muted,
        /// De-emphasized text; 3:1 only.
        text_faint,
        /// Primary buttons and other accent fills.
        accent,
        /// Text on `accent`.
        accent_text,
        /// Accent-colored text and icons on surfaces (a selected tab's label).
        accent_fg,
        /// The second accent: the backdrop's other color, cyan slots.
        accent_2,
        /// Focus rings.
        focus,
        link,
        ok,
        warn,
        danger,
        info,
        /// Text on a solid `ok` / `warn` / `danger` / `info` fill.
        state_text,
        /// Selected text.
        selection,
        hover,
        /// Pressed controls.
        active,
        /// Selected rows and items.
        selected,
        diff_add_bg,
        diff_remove_bg,
        /// Word-level changes inside a changed line.
        diff_add_emphasis,
        diff_remove_emphasis,
        diff_add_gutter,
        diff_remove_gutter,
        gutter_bg,
        gutter_text,
        /// The side of a split diff where a line does not exist.
        diff_empty_bg,
        diff_hunk_bg,
        diff_hunk_text,
    }
}

tokens! {
    /// Syntax colors (`[syntax]`), one per `diff::HighlightKind`.
    Syntax<gpui::Hsla>, "syntax" {
        keyword,
        function,
        type_ = "type",
        string,
        number,
        comment,
        constant,
        operator,
        punctuation,
        property,
        variable,
        attribute,
        tag,
        /// Unhighlighted code.
        plain,
    }
}

tokens! {
    /// Terminal colors (`[terminal]`): the ground, the cursor and the 16 ANSI colors.
    Terminal<gpui::Hsla>, "terminal" {
        background,
        foreground,
        cursor,
        black,
        red,
        green,
        yellow,
        blue,
        magenta,
        cyan,
        white,
        bright_black,
        bright_red,
        bright_green,
        bright_yellow,
        bright_blue,
        bright_magenta,
        bright_cyan,
        bright_white,
    }
}

impl<C: Copy> Terminal<C> {
    /// The ANSI colors 0-7 then the bright 8-15, as terminals index them.
    pub fn ansi(&self) -> [C; 16] {
        [
            self.black,
            self.red,
            self.green,
            self.yellow,
            self.blue,
            self.magenta,
            self.cyan,
            self.white,
            self.bright_black,
            self.bright_red,
            self.bright_green,
            self.bright_yellow,
            self.bright_blue,
            self.bright_magenta,
            self.bright_cyan,
            self.bright_white,
        ]
    }
}

tokens! {
    /// Corner radii (`[radius]`), px.
    Radius<gpui::Pixels>, "radius" {
        /// Tags and small rows.
        sm,
        /// Controls: buttons, inputs, chips.
        md,
        /// Panels and cards.
        lg,
    }
}

tokens! {
    /// The status-chip tint recipe (`[chips]`): fill and border are the state
    /// color at these alphas, text is the state color itself.
    Chips<f32>, "chips" {
        fill_alpha,
        border_alpha,
    }
}
